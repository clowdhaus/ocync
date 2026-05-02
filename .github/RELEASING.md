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
brew install norwoodj/tap/helm-docs   # or follow the upstream installer
```

`helm-docs` is invoked as a `pre-release-hook` during the bump to regenerate `charts/ocync/README.md` against the new chart version. Without it, cargo-release fails fast with a clear error before producing any commit or tag.

Then, on a clean `main` checkout:

```
cargo release patch --execute    # 0.1.0 -> 0.1.1
cargo release minor --execute    # 0.1.0 -> 0.2.0
cargo release major --execute    # 0.1.0 -> 1.0.0
cargo release 0.2.0 --execute    # explicit version
```

`cargo-release` will:

1. Bump `[workspace.package].version` in the root `Cargo.toml`. All five workspace crates (`ocync`, `ocync-distribution`, `ocync-sync`, `bench-proxy`, `xtask`) inherit via `version.workspace = true` and move together.
2. Rewrite `version` and `appVersion` in `charts/ocync/Chart.yaml` to the new value (driven by `pre-release-replacements` in the root `[package.metadata.release]`).
3. Run `helm-docs --chart-search-root charts` to regenerate `charts/ocync/README.md` against the new chart version (driven by `pre-release-hook`).
4. Regenerate `Cargo.lock` so `cargo build --locked` keeps working.
5. Create a single commit with all of the above.
6. Tag it `v<version>` (matches the `v*` pattern the release workflow watches).
7. Push the commit and tag to the remote.

The pushed tag fires `workflows/release.yml` and the rest is automated.

The four non-binary crates carry `[package.metadata.release] release = false` so cargo-release skips them entirely (no per-crate tags, no publish attempts). They still receive the version bump because their `[package].version` inherits from the workspace.

### Manual fallback

If `cargo-release` is unavailable, the same outcome by hand:

```
sed -i '' 's/^version = "0.3.0"/version = "0.4.0"/' Cargo.toml
sed -i '' 's/^version: 0.3.0/version: 0.4.0/' charts/ocync/Chart.yaml
sed -i '' 's/^appVersion: "0.3.0"/appVersion: "0.4.0"/' charts/ocync/Chart.yaml
helm-docs --chart-search-root charts
cargo generate-lockfile
git commit -am "chore(release): v0.4.0"
git tag v0.4.0
git push origin main v0.4.0
```

The validate job (`release.yml`) gates the tag against `[workspace.package].version`, `Chart.yaml` `version`, and `Chart.yaml` `appVersion`. Any mismatch fails fast before artifacts are built.

### Workspace versioning

The workspace has one version, defined once at `[workspace.package].version` and inherited by every crate. The five crates always carry the same number. None of them publish to crates.io; the version is the released-artifact identity (Docker tag, Helm chart version, GitHub Release name).

If a library crate ever needs to publish independently, drop `version.workspace = true` from its `[package]`, set an explicit version, and remove `release = false` from its `[package.metadata.release]`.

## What you do NOT need to touch

- `charts/ocync/values.yaml` `image.tag`. Left empty on purpose. The chart's `ocync.image` helper (`templates/_helpers.tpl`) defaults the tag to `<.Chart.AppVersion>-fips`, which now equals the bumped chart `appVersion`. Tag bump moves chart and image together.

If you ever need to override the image tag for a one-off install, that is a `helm install --set image.tag=...` decision, not a chart edit.

## Workflow steps

1. **validate**: fails fast unless the tag (minus `v`) matches `[workspace.package].version` in `Cargo.toml`, `version` in `charts/ocync/Chart.yaml`, AND `appVersion` in `charts/ocync/Chart.yaml`. Any mismatch is reported with `::error file=...::` against the offending file.
2. **build-linux** (matrix amd64/arm64): builds with `RUSTFLAGS="-C target-feature=+crt-static"` for fully static FIPS binaries; `readelf -d` confirms no `NEEDED` entries.
3. **build-macos** / **build-windows**: non-FIPS builds (the `aws-lc-rs` FIPS provider is Linux-only).
4. **docker** (matrix amd64/arm64): assumes `ECR_PUBLIC_ROLE_ARN`, builds the Dockerfile, pushes the per-arch tag.
5. **docker-manifest**: composes the per-arch images into `<version>-fips` and `latest-fips` manifests, signs both with cosign keyless.
6. **helm**: `helm package charts/ocync` (no flag overrides; chart version and appVersion come straight from `Chart.yaml`), then `helm push` to ECR Public over OCI.
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
