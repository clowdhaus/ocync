---
title: Security design
description: How ocync handles cryptography, cloud authentication and supply chain, measured against a FedRAMP High bar
order: 5
---

## The bar

`ocync` targets regulated environments: FIPS, GovCloud and DoD. FedRAMP High is the bar each decision is measured against. Three principles follow from it:

- **Every cryptographic operation runs inside a validated module.** TLS, request signing, token signing and digests all go through `aws-lc-rs` built against the FIPS module. No pure-Rust cryptography in a FIPS build.
- **Own the code on the security path.** Cloud authentication is a small surface: a few API calls and a credential chain. Owning it keeps the dependency tree small and every cryptographic call on the validated module.
- **Every release carries evidence.** Signatures, provenance and a bill of materials, so an assessor can check what shipped.

## Current state

- `aws-lc-rs` is the only TLS provider. `ring`, `openssl` and `native-tls` are banned in `deny.toml`.
- Linux release binaries are built with the `fips` feature, statically linked against glibc. See [FIPS 140-3](/fips).
- Content digests use `aws-lc-rs` (`crates/ocync-distribution/src/sha256.rs`).
- Release images are signed with cosign and carry a build provenance attestation. GitHub Actions are pinned to commit SHAs.
- The Docker base images (`cgr.dev/chainguard/rust:latest-dev`, `cgr.dev/chainguard/static:latest`) float by design so every build picks up Chainguard's patches. They are not pinned by digest.

## Dependency weight

The FIPS build of the `ocync` binary resolves 244 crates (normal and build dependencies). The cloud SDKs account for most of what can be removed:

| Dependency | Used for | Crates only it brings |
|---|---|---|
| `aws-config`, `aws-sdk-ecr`, `aws-sdk-ecrpublic` | `GetAuthorizationToken` (ECR, ECR Public), `BatchCheckLayerAvailability`, the credential chain | 41 |
| `google-cloud-auth` | one access token | 21 |
| Both together | | 80 |

The duplicated hash crate versions (`digest`, `block-buffer`, `crypto-common`, `cpufeatures`) all come from these SDKs, and four of the `skip` entries in `deny.toml` exist for them.

## TLS uses only FIPS-approved algorithms

The `fips` feature enables `rustls/fips`. That feature applies to every `rustls` user in the build, so `aws_lc_rs::default_provider()` drops ChaCha20-Poly1305 and every key exchange group the module does not approve, for `reqwest`, which `google-cloud-auth` also uses, and for the TLS client the AWS SDK builds itself. TLS 1.2 also requires extended master secret.

At startup a FIPS build checks `tls_fips()`, which asks `rustls` whether the installed provider is FIPS-approved, including whether the module is in approved mode. Every command except `version` refuses to start if it is not, so `version` can still report the failing state. `ocync version` prints the AWS-LC-FIPS module number a certificate names (`module: AWS-LC-FIPS 4`), the AWS-LC release (`release: AWS-LC 4.2.0`) and the result (`tls: approved`), in the shape `openssl list -providers` uses. A FIPS build decides all of this from one constant, `CRYPTO_BUILD` in `ocync-distribution`, so the CLI cannot disagree with the linked module.

`tests/fips_tls.rs` in `ocync-distribution` pins the behavior: the registry client fails the handshake against servers offering only TLS 1.3 ChaCha20-Poly1305, only TLS 1.2 ChaCha20-Poly1305, or only X25519, and succeeds against one offering AES-128-GCM.

## The shipped module's certification is recorded

`ocync` links AWS-LC FIPS module 4, which NIST lists as in process: lab-tested and submitted, not yet certified. Module 3.1 holds certificate #5314, but every `rustls` and `rustls-webpki` release that works with it carries published TLS advisories whose fixes require module 4. [FIPS 140-3](/fips#certification) lists each module's status and the FedRAMP documentation it implies.

`certification()` in `ocync-distribution` maps the linked module to its status, and `ocync version` prints it. `linked_module_has_a_certification_record` fails when aws-lc-rs moves to a module with no record, so a module change cannot ship unrecorded. When NIST certifies module 4, the record gains its certificate number.

The X25519MLKEM768 key exchange group stays on: `rustls` treats it as approved because ML-KEM in the AWS-LC FIPS module is FIPS-pending, the reading FedRAMP Rev. 5 SC-13 allows.

## Releases carry a bill of materials

> **Status: Planned.**

Each release publishes a CycloneDX software bill of materials for the binaries and the image, signed and attested alongside the existing cosign signature and provenance.

## Owned Google authentication

> **Status: Planned.**

`google-cloud-auth` signs and hashes with the pure-Rust `sha2` and `hmac` crates and draws randomness from a pure-Rust ChaCha20 generator. Replace it with an owned Application Default Credentials resolver that matches upstream behavior:

- `GOOGLE_APPLICATION_CREDENTIALS`, then the gcloud well-known file, then the GCE and GKE metadata server.
- Credential file types: service account keys (RS256 signed assertion via `aws-lc-rs`), authorized user (refresh token), and external account (Workload Identity Federation).
- Federation subject tokens from a file (such as a projected Kubernetes service account token), a URL, or AWS. The AWS source signs `GetCallerIdentity` with the owned AWS signer and credential chain, so `ocync` on EKS reaches Artifact Registry with no long-lived key.
- Service account impersonation after the federation exchange (`service_account_impersonation_url`).
- The real token lifetime from `expires_in`, replacing the fixed 10-minute assumption in `auth/gcp.rs`.

## Owned AWS request signing

> **Status: Planned.**

`aws-sigv4` computes Signature Version 4 with the pure-Rust `sha2` and `hmac` crates. Replace it with an owned signer on `aws-lc-rs` HMAC and SHA-256. The signer covers the calls `ocync` makes: ECR and ECR Public (`GetAuthorizationToken`, `BatchCheckLayerAvailability`) and STS (`AssumeRole` for role chaining).

## Owned AWS credential chain

> **Status: Planned.**

Replace `aws-config` and the AWS SDK crates with an owned credential chain that matches the documented behavior in [Amazon ECR](/registries/ecr), in AWS precedence order:

- Environment variables.
- Shared config and credentials files, including `source_profile` and `role_arn` role chaining and the per-registry `aws_profile` field.
- Web identity (IRSA) through `AssumeRoleWithWebIdentity`.
- `credential_process`.
- IAM Identity Center (`aws sso login`): read the cached token, exchange it with `GetRoleCredentials`, and refresh it with `CreateToken` within 5 minutes of expiry, at most once every 30 seconds, writing the refreshed token back to the shared cache in a form the AWS CLI still reads. Neither call is signed. The cache file name is a SHA-1 of the session name, computed with `aws-lc-rs`.
- The container credential endpoint (ECS task roles, EKS Pod Identity).
- The EC2 instance metadata service, IMDSv2 only.
- FIPS endpoints through `AWS_USE_FIPS_ENDPOINT`, and the GovCloud, China and ISO partitions.

The SDK crates are removed once the owned chain and signer reach parity.

## Alternatives considered

Lighter crates were evaluated before choosing to own the code:

| Crate | Why it does not fit |
|---|---|
| `reqsign` (Apache OpenDAL) | The core crate depends on the pure-Rust `sha1`, `sha2` and `hmac` crates, so signing stays outside the validated module. |
| `aws-sign-v4` | Built on `ring`, which `deny.toml` bans. |
| `aws-creds` | Credentials only, blocking HTTP, and no IAM Identity Center, `source_profile` role chaining or `credential_process`. |
| `gcp_auth` | Its cryptography runs on `aws-lc-rs`, but it has no Workload Identity Federation. Federation lets `ocync` outside Google Cloud reach Artifact Registry without a long-lived service account key. |

## Replacing a dependency

Each replacement starts from the upstream crate's tests and works backwards:

1. Copy the upstream tests and test data into the repo with Apache-2.0 attribution and the upstream commit recorded. Point them at the new module's API, so they fail.
2. Implement until they pass. The AWS signing test suite shipped in `aws-sigv4` (`aws-signing-test-suite/v4`) is the reference for the signer. `google-cloud-auth` and `aws-config` supply theirs.
3. An upstream test for behavior `ocync` deliberately does not support is listed in this document with the reason. A gap is a decision, never an omission.
4. While both implementations exist, the upstream crate stays as a dev-dependency and a differential test runs both on the same inputs: identical signatures, identical resolved credentials.
5. Before the upstream crate is removed, the real binary runs against real ECR, ECR Public and Artifact Registry, and the result is diffed against the base commit.
