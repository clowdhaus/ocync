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

# ── Retry helper ──────────────────────────────────────────────────────────────

# Every network fetch below goes through this. Exhausting the attempts is a
# hard failure: nothing waits on cloud-init, so `terraform apply` reports
# success either way, and continuing past a failed fetch turns a clear error
# here into a confusing missing-binary error when bench-remote runs later.
retry() {
  local what="$1"
  shift
  local attempt
  for attempt in 1 2 3; do
    if "$@"; then
      return 0
    fi
    if [ "$attempt" -lt 3 ]; then
      echo "  $what attempt $attempt failed, retrying in 15s..."
      sleep 15
    fi
  done
  echo "ERROR: $what failed after 3 attempts, aborting bootstrap" >&2
  return 1
}

# ── System packages ───────────────────────────────────────────────────────────

# A cold instance's first mirror contact is the most transient-failure-prone
# fetch here, and gpgme-devel and openssl-devel are what skopeo's cgo build
# needs later.
retry "dnf update" dnf update -y
retry "dnf install" dnf install -y \
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

# pipefail is set inside the su, not inherited from this shell: without it the
# pipeline's status is the installer's, and curl failing feeds it EOF, which it
# exits zero on. retry would then report success having installed nothing.
retry rustup su - ec2-user -c 'set -o pipefail; curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path'

su - ec2-user -c 'echo "export PATH=\"\$HOME/.cargo/bin:\$PATH\"" >> ~/.bashrc'

RUSTC_VERSION="$(su - ec2-user -c '/home/ec2-user/.cargo/bin/rustc --version')"
echo "Rust: $${RUSTC_VERSION}"

# ── Go ────────────────────────────────────────────────────────────────────────

echo "--- Installing Go 1.26.x"
GO_VERSION="1.26.2"
GO_ARCHIVE="go$${GO_VERSION}.linux-amd64.tar.gz"

retry "Go download" curl -fsSL "https://go.dev/dl/$${GO_ARCHIVE}" -o "/tmp/$${GO_ARCHIVE}"
tar -C /usr/local -xzf "/tmp/$${GO_ARCHIVE}"
rm -f "/tmp/$${GO_ARCHIVE}"

ln -sf /usr/local/go/bin/go   /usr/local/bin/go
ln -sf /usr/local/go/bin/gofmt /usr/local/bin/gofmt

cat >> /home/ec2-user/.bashrc <<'EOF'

# Go
export PATH="/usr/local/go/bin:$HOME/go/bin:$PATH"
EOF

chown ec2-user:ec2-user /home/ec2-user/.bashrc

GO_VERSION_LINE="$(go version)"
echo "Go: $${GO_VERSION_LINE}"

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
retry "ecr-credential-helper install" go install github.com/awslabs/amazon-ecr-credential-helper/ecr-login/cli/docker-credential-ecr-login@latest
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
ECR_HELPER_MOD="$(go version -m /usr/local/bin/docker-credential-ecr-login | grep -m1 -E '^[[:space:]]*mod[[:space:]]' || echo 'build info unavailable')"
echo "ecr-credential-helper: $${ECR_HELPER_MOD}"

# ── skopeo (dregsy transfer backend) ─────────────────────────────────────────

echo "--- Installing skopeo"
# skopeo moved its module path to go.podman.io/skopeo at v1.23.0. The github.com
# path still serves tags, but their go.mod declares the new path, so installing
# from it fails on a module path mismatch rather than a missing version.
retry "skopeo install" env CGO_ENABLED=1 go install \
  -tags "exclude_graphdriver_btrfs containers_image_openpgp" \
  go.podman.io/skopeo/cmd/skopeo@latest
cp /root/go/bin/skopeo /usr/local/bin/skopeo

SKOPEO_VERSION_LINE="$(skopeo --version)"
echo "skopeo: $${SKOPEO_VERSION_LINE}"

# ── dregsy ────────────────────────────────────────────────────────────────────

echo "--- Installing dregsy"
# dregsy tags releases without the v prefix, so the module proxy cannot read
# them as versions and @latest resolves to a pseudo-version off the default
# branch rather than to a release.
retry "dregsy install" go install github.com/xelalexv/dregsy/cmd/dregsy@latest
cp /root/go/bin/dregsy /usr/local/bin/dregsy

DREGSY_MOD="$(go version -m /usr/local/bin/dregsy | grep -m1 -E '^[[:space:]]*mod[[:space:]]' || echo 'build info unavailable')"
echo "dregsy: $${DREGSY_MOD}"

# ── regsync ───────────────────────────────────────────────────────────────────

echo "--- Installing regsync"
retry "regsync install" go install github.com/regclient/regclient/cmd/regsync@latest
cp /root/go/bin/regsync /usr/local/bin/regsync

REGSYNC_VERSION_LINE="$(regsync version)"
echo "regsync: $${REGSYNC_VERSION_LINE}"

# ── Clean up Go build cache ──────────────────────────────────────────────────

rm -rf /root/go/pkg/mod/cache /root/.cache/go-build

# ── Clone and build ocync ────────────────────────────────────────────────────

echo "--- Cloning and building ocync (as ec2-user)"

# Cloning is separate from building so it can retry. It clones to a scratch
# path and moves it into place, so a retry after a partial clone is idempotent
# without ever deleting an existing checkout: that path holds the git-tracked
# run-record archive, and re-running this script through `cloud-init clean`
# is a normal way to retry a bootstrap. The build is not retried, since a
# compile failure is deterministic and cargo retries its own fetches.
retry "ocync clone" su - ec2-user -c '
  set -e
  rm -rf $HOME/.ocync-clone
  git clone https://github.com/clowdhaus/ocync.git $HOME/.ocync-clone
  [ -e $HOME/ocync ] || mv $HOME/.ocync-clone $HOME/ocync
  rm -rf $HOME/.ocync-clone
'

# set -e inside the block: without it only the last command's status escapes,
# so a failed cargo build would be masked by the cp that follows it.
su - ec2-user -c "
  set -e
  source \$HOME/.cargo/env
  cd \$HOME/ocync
  cargo build --release --package ocync --package bench-proxy
  cp target/release/ocync \$HOME/.cargo/bin/ocync
  cp target/release/bench-proxy \$HOME/.cargo/bin/bench-proxy
"

# Assigned rather than echoed inline: a command substitution inside an echo
# argument yields echo's status, so set -e can never fire on a failed probe.
OCYNC_VERSION="$(su - ec2-user -c '/home/ec2-user/.cargo/bin/ocync version')"
echo "ocync: $${OCYNC_VERSION}"
BENCH_PROXY_PATH="$(su - ec2-user -c 'command -v /home/ec2-user/.cargo/bin/bench-proxy')"
echo "bench-proxy: $${BENCH_PROXY_PATH}"

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
