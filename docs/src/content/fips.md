---
title: FIPS 140-3
description: FIPS 140-3 cryptography in ocync - the linked AWS-LC FIPS module, its certification status, FIPS-only TLS, and how to check a binary.
order: 7
---

## Overview

`ocync` uses [aws-lc-rs](https://github.com/aws/aws-lc-rs) as its sole TLS crypto provider. Linux binaries statically link the AWS-LC FIPS module and restrict TLS to FIPS-approved algorithms by default. See [Certification](#certification) for the module's NIST status.

## Build variants

| Platform | Binary | Crypto | FIPS |
|---|---|---|---|
| Linux x86_64 | `ocync-fips-linux-amd64` | aws-lc-rs (FIPS) | Yes |
| Linux arm64 | `ocync-fips-linux-arm64` | aws-lc-rs (FIPS) | Yes |
| macOS arm64 | `ocync-macos-arm64` | aws-lc-rs | No |
| Windows x86_64 | `ocync-windows-amd64.exe` | aws-lc-rs | No |

FIPS mode is a compile-time decision. The `fips` feature flag links the AWS-LC FIPS module. The `non-fips` feature flag uses aws-lc-rs without the FIPS module.

A FIPS build restricts TLS to FIPS-approved algorithms: AES-GCM cipher suites, key exchange over P-256, P-384 or the X25519MLKEM768 post-quantum hybrid, and extended master secret for TLS 1.2. ChaCha20-Poly1305 and plain X25519 are never offered. The restriction covers every TLS client in the binary: `reqwest`, which Google authentication also uses, and the client the AWS SDK builds itself. A FIPS build checks this at startup and refuses to run if TLS is not FIPS-approved.

`rustls` counts X25519MLKEM768 as approved through its ML-KEM part, although its own source notes that ML-KEM in the AWS-LC FIPS module is FIPS-pending and that it follows the interpretation some regimes (such as FedRAMP Rev. 5 SC-13) allow for modules in that state. Treat the hybrid as approved only if your assessor accepts that interpretation.

## Certification

| AWS-LC FIPS module (static library) | NIST status | Used by ocync |
|---|---|---|
| module 4 | [Modules in Process](https://csrc.nist.gov/Projects/Cryptographic-Module-Validation-Program/Modules-In-Process/Modules-In-Process-List): tested by an accredited lab and submitted to NIST, not yet certified | yes |
| module 3 | Certificate [#5314](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5314) | no |
| module 2.0 | Certificate [#4816](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/4816) | no |

Statuses follow AWS-LC's [FIPS.md](https://github.com/aws/aws-lc/blob/main/crypto/fipsmodule/FIPS.md). AWS-LC lists certificate #5314 as "AWS-LC-FIPS v3.1", and aws-lc-rs before 1.18 links the module 3 branch it calls 3.0.x. Check the certificate's security policy for the exact module versions it covers.

`ocync` links module 4 because no other module is safe to ship with. aws-lc-rs 1.18 is the first release on module 4, and every `rustls` and `rustls-webpki` release that still works with module 3 is affected by published TLS advisories (RUSTSEC-2026-0285, RUSTSEC-2026-0346, RUSTSEC-2026-0348, RUSTSEC-2026-0349, RUSTSEC-2026-0382). The fixes require aws-lc-rs 1.18.

For FedRAMP, the Rev5 cryptographic module rules expect modules with an active validation or update streams of them (CMU-CSO-UVM), and require documenting each module and whether it is validated (CMU-CSO-CMD). Until NIST certifies module 4, record `ocync` as using AWS-LC FIPS module 4, submitted and not yet certified. `ocync version` prints the status of the module a binary links, and a test fails the build if aws-lc-rs moves to a FIPS module number `ocync` has no record of. Patch releases within module 4 are its update stream and keep the same record.

## FIPS endpoints for ECR

When `AWS_USE_FIPS_ENDPOINT=true`, `ocync` rewrites ECR hostnames to use FIPS endpoints:

```
123456789012.dkr.ecr.us-east-1.amazonaws.com
  -> 123456789012.dkr.ecr-fips.us-east-1.amazonaws.com
```

With FIPS endpoints, both ends of the connection run FIPS cryptography: the client offers only FIPS-approved algorithms and the endpoint is AWS FIPS-validated. Only ECR has FIPS endpoints among supported registries.

## Building with FIPS

FIPS builds require CMake, Go, and Perl for the aws-lc-rs build process:

```bash
# FIPS build (default)
cargo build --release

# Non-FIPS build
cargo build --release --no-default-features --features non-fips
```

## Docker images

The `latest-fips` Docker tag ships the FIPS build:

```bash
docker pull public.ecr.aws/clowdhaus/ocync:latest-fips
```

## Verifying FIPS status

The `ocync version` command reports whether the binary is a FIPS build, the linked module and its certification status, and whether TLS is restricted to FIPS-approved algorithms at runtime:

```
$ ocync version
ocync 0.8.2
FIPS 140-3: enabled
  module: AWS-LC FIPS module 4
  release: AWS-LC 4.2.0
  certificate: none yet, module in process (https://csrc.nist.gov/Projects/Cryptographic-Module-Validation-Program/Modules-In-Process/Modules-In-Process-List)
  tls: approved
```

`module` names the module the way AWS-LC's `openssl version -fips` does; the module number is what a NIST certificate names. `release` is the AWS-LC release, which tells builds of one module apart but does not identify a certificate. Non-FIPS builds show `FIPS 140-3: disabled` and the release. FIPS is a compile-time selection; there is no runtime toggle.

## Banned dependencies

`ring` and `native-tls` are banned via `deny.toml` to prevent accidental use of non-aws-lc-rs crypto. `reqwest` is configured with `rustls-no-provider`, and the crypto provider is installed explicitly at process startup.
