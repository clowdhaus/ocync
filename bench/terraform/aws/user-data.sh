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

# ── SSH key (instance access + git clone) ─────────────────────────────────────

echo "--- Configuring SSH key for ec2-user"
mkdir -p /home/ec2-user/.ssh
chmod 700 /home/ec2-user/.ssh

cat > /home/ec2-user/.ssh/id_ed25519 <<'KEYEOF'
${ssh_private_key}
KEYEOF
chmod 600 /home/ec2-user/.ssh/id_ed25519

cat > /home/ec2-user/.ssh/config <<'SSHEOF'
Host github.com
  IdentityFile ~/.ssh/id_ed25519
  StrictHostKeyChecking accept-new
SSHEOF
chmod 600 /home/ec2-user/.ssh/config

chown -R ec2-user:ec2-user /home/ec2-user/.ssh

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

# ── ECR credential helper ────────────────────────────────────────────────────

echo "--- Installing ECR credential helper"
go install github.com/awslabs/amazon-ecr-credential-helper/ecr-login/cli/docker-credential-ecr-login@latest
cp /root/go/bin/docker-credential-ecr-login /usr/local/bin/

mkdir -p /home/ec2-user/.docker
cat > /home/ec2-user/.docker/config.json <<'DCEOF'
{"credHelpers":{"${account_id}.dkr.ecr.us-east-1.amazonaws.com":"ecr-login","public.ecr.aws":"ecr-login"}}
DCEOF
chown -R ec2-user:ec2-user /home/ec2-user/.docker

echo "ecr-credential-helper: $(docker-credential-ecr-login version 2>&1 || true)"

# ── skopeo (dregsy transfer backend) ─────────────────────────────────────────

echo "--- Installing skopeo"
CGO_ENABLED=1 go install \
  -tags "exclude_graphdriver_btrfs exclude_graphdriver_devicemapper containers_image_openpgp" \
  github.com/containers/skopeo/cmd/skopeo@latest
cp /root/go/bin/skopeo /usr/local/bin/skopeo

echo "skopeo: $(skopeo --version 2>&1)"

# ── dregsy ────────────────────────────────────────────────────────────────────

echo "--- Installing dregsy"
go install github.com/xelalexv/dregsy/cmd/dregsy@latest
cp /root/go/bin/dregsy /usr/local/bin/dregsy

echo "dregsy: $(dregsy --version 2>&1 || true)"

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
  git clone git@github.com:clowdhaus/ocync.git \$HOME/ocync
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
