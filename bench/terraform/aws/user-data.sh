#!/usr/bin/env bash
set -euo pipefail

exec > >(tee /var/log/user-data.log | logger -t user-data -s 2>/dev/console) 2>&1

echo "=== ocync benchmark runner bootstrap ==="
echo "Started: $(date -u +%Y-%m-%dT%H:%M:%SZ)"

# ── Swap (Rust compilation only, not needed for running ocync) ───────────────

if ! swapon --show | grep -q '/swapfile'; then
  echo "--- Creating 8GB swap file for Rust compilation"
  fallocate -l 8G /swapfile
  chmod 600 /swapfile
  mkswap /swapfile
  swapon /swapfile
  echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi

# ── System packages ───────────────────────────────────────────────────────────

dnf update -y
dnf install -y \
  git \
  cmake \
  gcc \
  make \
  openssl-devel \
  gpgme-devel

# ── Docker Hub credentials ───────────────────────────────────────────────────
# Written to a dedicated env file (not .bashrc) so that non-interactive
# SSH sessions (bench-remote) can source them reliably.

cat > /home/ec2-user/.bench-env <<'EOF'
export DOCKERHUB_USERNAME="${dockerhub_username}"
export DOCKERHUB_ACCESS_TOKEN="${dockerhub_token}"
EOF
chmod 600 /home/ec2-user/.bench-env
chown ec2-user:ec2-user /home/ec2-user/.bench-env

# Also source from .bashrc for interactive SSH sessions.
echo 'source ~/.bench-env 2>/dev/null || true' >> /home/ec2-user/.bashrc
chown ec2-user:ec2-user /home/ec2-user/.bashrc

# ── Rust ──────────────────────────────────────────────────────────────────────

echo "--- Installing Rust via rustup (as ec2-user)"

for attempt in 1 2 3; do
  if su - ec2-user -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path'; then
    break
  fi
  echo "  rustup attempt $attempt failed, retrying in 15s..."
  sleep 15
done

su - ec2-user -c 'echo "export PATH=\"\$HOME/.cargo/bin:\$PATH\"" >> ~/.bashrc'

echo "Rust: $(su - ec2-user -c '/home/ec2-user/.cargo/bin/rustc --version')"

# ── Go ────────────────────────────────────────────────────────────────────────

echo "--- Installing Go 1.26.x"
GO_VERSION="1.26.2"
GO_ARCHIVE="go$${GO_VERSION}.linux-amd64.tar.gz"

for attempt in 1 2 3; do
  if curl -fsSL "https://go.dev/dl/$${GO_ARCHIVE}" -o "/tmp/$${GO_ARCHIVE}"; then
    break
  fi
  echo "  Go download attempt $attempt failed, retrying in 15s..."
  sleep 15
done
tar -C /usr/local -xzf "/tmp/$${GO_ARCHIVE}"
rm -f "/tmp/$${GO_ARCHIVE}"

ln -sf /usr/local/go/bin/go   /usr/local/bin/go
ln -sf /usr/local/go/bin/gofmt /usr/local/bin/gofmt

cat >> /home/ec2-user/.bashrc <<'EOF'

# Go
export PATH="/usr/local/go/bin:$HOME/go/bin:$PATH"
EOF

chown ec2-user:ec2-user /home/ec2-user/.bashrc

echo "Go: $(go version)"

# Go env for root builds (dregsy, regsync, skopeo, ecr-credential-helper)
export HOME=/root GOPATH=/root/go GOCACHE=/root/.cache/go-build
export PATH="/usr/local/go/bin:$GOPATH/bin:$PATH"

# Competitor tools install at @latest, so a run compares ocync against what
# was current when this instance was built. These resolve once, here, not per
# run: to measure against newer releases, replace the instance. What makes two
# runs comparable is the record of what each used, which the harness writes
# into bench/results/{registry}.json.

# ── ECR credential helper ────────────────────────────────────────────────────

echo "--- Installing ECR credential helper"
go install github.com/awslabs/amazon-ecr-credential-helper/ecr-login/cli/docker-credential-ecr-login@latest
cp /root/go/bin/docker-credential-ecr-login /usr/local/bin/

mkdir -p /home/ec2-user/.docker
cat > /home/ec2-user/.docker/config.json <<'DCEOF'
{"credHelpers":{"${account_id}.dkr.ecr.${region}.amazonaws.com":"ecr-login","public.ecr.aws":"ecr-login"}}
DCEOF
chown -R ec2-user:ec2-user /home/ec2-user/.docker

# `go version -m` reads the module version stamped into the binary, which is
# the only version several of these report. The credential helper leaves its
# own version at "development" because go install sets none of the ldflags its
# Makefile uses, and dregsy has no version flag at all.
echo "ecr-credential-helper: $(go version -m /usr/local/bin/docker-credential-ecr-login | awk '$1 == "mod" { print $3; exit }')"

# ── skopeo (dregsy transfer backend) ─────────────────────────────────────────

echo "--- Installing skopeo"
# skopeo moved its module path to go.podman.io/skopeo at v1.23.0. The github.com
# path still serves tags, but their go.mod declares the new path, so installing
# from it fails on a module path mismatch rather than a missing version.
CGO_ENABLED=1 go install \
  -tags "exclude_graphdriver_btrfs containers_image_openpgp" \
  go.podman.io/skopeo/cmd/skopeo@latest
cp /root/go/bin/skopeo /usr/local/bin/skopeo

echo "skopeo: $(skopeo --version 2>&1)"

# ── dregsy ────────────────────────────────────────────────────────────────────

echo "--- Installing dregsy"
# dregsy tags releases without the v prefix, so the module proxy cannot read
# them as versions and @latest resolves to a pseudo-version off the default
# branch rather than to a release.
go install github.com/xelalexv/dregsy/cmd/dregsy@latest
cp /root/go/bin/dregsy /usr/local/bin/dregsy

echo "dregsy: $(go version -m /usr/local/bin/dregsy | awk '$1 == "mod" { print $3; exit }')"

# ── regsync ───────────────────────────────────────────────────────────────────

echo "--- Installing regsync"
go install github.com/regclient/regclient/cmd/regsync@latest
cp /root/go/bin/regsync /usr/local/bin/regsync

echo "regsync: $(regsync version 2>&1 || true)"

# ── Clean up Go build cache ──────────────────────────────────────────────────

rm -rf /root/go/pkg/mod/cache /root/.cache/go-build

# ── Clone and build ocync ────────────────────────────────────────────────────

echo "--- Cloning and building ocync (as ec2-user)"

su - ec2-user -c "
  source \$HOME/.cargo/env
  git clone https://github.com/clowdhaus/ocync.git \$HOME/ocync
  cd \$HOME/ocync
  cargo build --release --package ocync --package bench-proxy
  cp target/release/ocync \$HOME/.cargo/bin/ocync
  cp target/release/bench-proxy \$HOME/.cargo/bin/bench-proxy
"

echo "ocync: $(su - ec2-user -c '/home/ec2-user/.cargo/bin/ocync version')"
echo "bench-proxy: built"

# ── Generate bench-proxy CA and install into system trust store ──────────────

echo "--- Generating bench-proxy CA"
mkdir -p /etc/bench-proxy
su - ec2-user -c "/home/ec2-user/.cargo/bin/bench-proxy ca-init \
  --out /tmp/bench-proxy-ca.pem \
  --key /tmp/bench-proxy-ca-key.pem"
install -m 0644 /tmp/bench-proxy-ca.pem /etc/bench-proxy/ca.pem
install -m 0600 /tmp/bench-proxy-ca-key.pem /etc/bench-proxy/ca-key.pem
chown -R ec2-user:ec2-user /etc/bench-proxy
rm -f /tmp/bench-proxy-ca.pem /tmp/bench-proxy-ca-key.pem

echo "--- Installing bench-proxy CA into system trust store"
cp /etc/bench-proxy/ca.pem /etc/pki/ca-trust/source/anchors/bench-proxy-ca.pem
update-ca-trust

# ── Done ──────────────────────────────────────────────────────────────────────

echo "=== Bootstrap complete: $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
