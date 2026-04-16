#!/usr/bin/env bash
set -euo pipefail

exec > >(tee /var/log/user-data.log | logger -t user-data -s 2>/dev/console) 2>&1

echo "=== ocync benchmark runner bootstrap ==="
echo "Started: $(date -u +%Y-%m-%dT%H:%M:%SZ)"

# ── System packages ───────────────────────────────────────────────────────────

dnf update -y
dnf install -y \
  git \
  gcc \
  make \
  openssl-devel \
  gpgme-devel \
  python3 \
  python3-pip

# ── Rust ──────────────────────────────────────────────────────────────────────

echo "--- Installing Rust via rustup (as ec2-user)"

# Retry up to 3 times — NAT gateway may not be routable yet on first boot.
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
GO_ARCHIVE="go${GO_VERSION}.linux-amd64.tar.gz"

curl -fsSL "https://go.dev/dl/${GO_ARCHIVE}" -o "/tmp/${GO_ARCHIVE}"
tar -C /usr/local -xzf "/tmp/${GO_ARCHIVE}"
rm -f "/tmp/${GO_ARCHIVE}"

# Make Go available system-wide
ln -sf /usr/local/go/bin/go   /usr/local/bin/go
ln -sf /usr/local/go/bin/gofmt /usr/local/bin/gofmt

# Add GOPATH/bin and Go to ec2-user PATH
cat >> /home/ec2-user/.bashrc <<'EOF'

# Go
export PATH="/usr/local/go/bin:$HOME/go/bin:$PATH"
EOF

chown ec2-user:ec2-user /home/ec2-user/.bashrc

echo "Go: $(go version)"

# Go env for root builds (dregsy, regsync, skopeo, ecr-credential-helper)
export HOME=/root GOPATH=/root/go GOCACHE=/root/.cache/go-build
export PATH="/usr/local/go/bin:$GOPATH/bin:$PATH"

# ── mitmproxy ─────────────────────────────────────────────────────────────────

echo "--- Installing mitmproxy via pip3"
pip3 install --quiet mitmproxy

# Generate mitmproxy CA cert as ec2-user, then install into system trust store.
# Both rustls-native-certs (ocync/Rust) and Go's x509 package read from the
# system store, ensuring all tools trust the MITM'd proxy connections.
echo "--- Generating mitmproxy CA certificate (as ec2-user)"
su - ec2-user -c 'timeout 3 mitmdump --quiet || true'

echo "--- Installing mitmproxy CA into system trust store"
cp /home/ec2-user/.mitmproxy/mitmproxy-ca-cert.pem /etc/pki/ca-trust/source/anchors/mitmproxy-ca.pem
update-ca-trust

# ── ECR credential helper ────────────────────────────────────────────────────

echo "--- Installing ECR credential helper"
go install github.com/awslabs/amazon-ecr-credential-helper/ecr-login/cli/docker-credential-ecr-login@latest
cp /root/go/bin/docker-credential-ecr-login /usr/local/bin/

# Configure docker credential chain for ec2-user (used by dregsy/regsync).
mkdir -p /home/ec2-user/.docker
cat > /home/ec2-user/.docker/config.json <<'DCEOF'
{"credHelpers":{"660548353186.dkr.ecr.us-east-1.amazonaws.com":"ecr-login","public.ecr.aws":"ecr-login"}}
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

# Read GitHub token from SSM Parameter Store.
GH_TOKEN=$(aws ssm get-parameter \
  --name "/ocync/bench/github-token" \
  --with-decryption \
  --region us-east-1 \
  --query 'Parameter.Value' \
  --output text)

su - ec2-user -c "
  source \$HOME/.cargo/env
  git clone --branch benchmark-suite https://${GH_TOKEN}@github.com/clowdhaus/ocync.git \$HOME/ocync
  cd \$HOME/ocync
  git remote set-url origin https://github.com/clowdhaus/ocync.git
  cargo build --release --package ocync
  cp target/release/ocync \$HOME/.cargo/bin/ocync
"

echo "ocync: $(su - ec2-user -c '/home/ec2-user/.cargo/bin/ocync version')"

# ── Done ──────────────────────────────────────────────────────────────────────

echo "=== Bootstrap complete: $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
