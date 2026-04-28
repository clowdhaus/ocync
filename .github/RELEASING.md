# Releasing

Internal reference for cutting an ocync release. The release workflow (`workflows/release.yml`) is fully tag-driven: a single `v*` tag fans out to binaries, Docker images, the Helm chart, and a GitHub Release.

## What ships

A tag push produces, all in one workflow run:

- Static binaries: `ocync-fips-linux-{amd64,arm64}`, `ocync-macos-arm64`, `ocync-windows-amd64.exe`
- Docker images on `public.ecr.aws/clowdhaus/ocync`:
  - per-arch: `<version>-fips-amd64`, `<version>-fips-arm64`
  - multi-arch manifests: `<version>-fips` and `latest-fips`
  - both manifests signed with cosign (keyless, OIDC)
- Helm chart on `oci://public.ecr.aws/clowdhaus/ocync` at `<version>`
- GitHub Release with all binaries, `sha256sums.txt`, build provenance attestation, and git-cliff-generated notes

## Cutting a release

Local flow uses [`cargo-release`](https://github.com/crate-ci/cargo-release) to bump the version, regenerate `Cargo.lock`, commit, tag, and push in one shot. Install once:

```
cargo install cargo-release
```

Then, on a clean `main` checkout:

```
cargo release patch --execute    # 0.1.0 -> 0.1.1
cargo release minor --execute    # 0.1.0 -> 0.2.0
cargo release major --execute    # 0.1.0 -> 1.0.0
cargo release 0.2.0 --execute    # explicit version
```

`cargo-release` will:

1. Bump `[package].version` in the root `Cargo.toml` (the `ocync` binary crate).
2. Regenerate `Cargo.lock` so `cargo build --locked` keeps working.
3. Create a single commit with the bump.
4. Tag it `v<version>` (matches the `v*` pattern the release workflow watches).
5. Push the commit and tag to the remote.

The pushed tag fires `workflows/release.yml` and the rest is automated.

### Manual fallback

If `cargo-release` is unavailable, the same outcome by hand:

```
sed -i '' 's/^version = "0.1.0"/version = "0.2.0"/' Cargo.toml
cargo generate-lockfile
git commit -am "chore(release): v0.2.0"
git tag v0.2.0
git push origin main v0.2.0
```

The validate job (`release.yml:23-37`) gates on `[package].version` matching the tag, so a mismatch fails fast before any artifact is built.

### Workspace versioning

Three crates live in this workspace: `ocync` (root, the binary), `ocync-distribution`, and `ocync-sync`. Only the root crate's version is gated by the release workflow, and only the root crate's version drives the released artifacts (Docker image tag, Helm `appVersion`, GitHub Release name). The library crates can stay at `0.1.0` indefinitely; they are not published to crates.io.

On the first `cargo release` run, do a dry run (no `--execute`) and confirm only the root `Cargo.toml` is touched. If `cargo-release` proposes bumping the library crates, opt them out by adding to each library `Cargo.toml`:

```toml
[package.metadata.release]
release = false
```

Library crates can opt back in later if we ever publish them.

## What you do NOT need to touch

- `charts/ocync/Chart.yaml` `version` / `appVersion`. `helm package --version $TAG --app-version $TAG` overrides them at release time. The committed `0.1.0` is a placeholder for `helm lint` / `helm template` in CI; do not chase it.
- `charts/ocync/values.yaml` `image.tag`. Left empty on purpose. The chart's `ocync.image` helper (`templates/_helpers.tpl`) defaults the tag to `<.Chart.AppVersion>-fips`, which is whatever was passed to `helm package`. Tag bump moves chart and image together.

If you ever need to override the image tag for a one-off install, that is a `helm install --set image.tag=...` decision, not a chart edit.

## Workflow steps

1. **validate**: fails fast unless the tag (minus `v`) matches `Cargo.toml` `[package].version`. This is the only version gate; `Chart.yaml` is not checked because it is overridden during packaging.
2. **build-linux** (matrix amd64/arm64): builds with `RUSTFLAGS="-C target-feature=+crt-static"` for fully static FIPS binaries; `readelf -d` confirms no `NEEDED` entries.
3. **build-macos** / **build-windows**: non-FIPS builds (the `aws-lc-rs` FIPS provider is Linux-only).
4. **docker** (matrix amd64/arm64): assumes `ECR_PUBLIC_ROLE_ARN`, builds the Dockerfile, pushes the per-arch tag.
5. **docker-manifest**: composes the per-arch images into `<version>-fips` and `latest-fips` manifests, signs both with cosign keyless.
6. **helm**: `helm package` with `--version` / `--app-version` set to the tag, then `helm push` to ECR Public over OCI. Chart version and app version both equal the tag.
7. **release**: downloads all binary artifacts, generates `sha256sums.txt`, attests build provenance, runs `git-cliff` (config in `cliff.toml`) for release notes filtered by conventional-commit type, and creates the GitHub Release.

The `helm` job depends on `docker-manifest`, so a failed image build short-circuits the chart push. The chart will never reference an image that does not exist.

## Required secrets

- `ECR_PUBLIC_ROLE_ARN`: IAM role ARN with `ecr-public:*` for pushing under `public.ecr.aws/clowdhaus/`. Used by `docker`, `docker-manifest`, and `helm` jobs via OIDC.

## Pre-flight checks

Run the CI gate locally before tagging:

```
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test && cargo deny check
```

For chart-only sanity:

```
helm lint charts/ocync
helm template ocync charts/ocync
helm unittest charts/ocync
```

## Recovering from a failed release

The workflow has no `concurrency: cancel-in-progress`; a re-run on the same tag will conflict on already-pushed Docker tags and the Helm chart (ECR Public rejects re-push of the same chart version). Recovery options:

- **Binary or release-notes failure only**: re-run failed jobs from the Actions UI; the GitHub Release step is idempotent on the tag.
- **Docker or Helm push partially completed**: delete the bad version from ECR Public (`aws ecr-public batch-delete-image` for the manifest, `aws ecr-public delete-repository --force` is the nuclear option), delete the git tag (`git push --delete origin v0.2.0`), bump to the next patch, and re-tag. Do not try to overwrite a published version.

Never force-push or amend a published tag. Third parties and the GitHub Release point at the original commit.
