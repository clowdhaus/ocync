---
title: FIPS 140-3
description: FIPS 140-3 validated cryptography in ocync using aws-lc-rs with NIST Certificate #4816.
order: 7
---

## Overview

`ocync` uses [aws-lc-rs](https://github.com/aws/aws-lc-rs) as its sole TLS crypto provider. On Linux, binaries ship with FIPS 140-3 validated cryptography (NIST Certificate #4816) enabled by default.

## Build variants

| Platform | Binary | Crypto | FIPS |
|---|---|---|---|
| Linux x86_64 | `ocync-fips-linux-amd64` | aws-lc-rs (FIPS) | Yes |
| Linux arm64 | `ocync-fips-linux-arm64` | aws-lc-rs (FIPS) | Yes |
| macOS arm64 | `ocync-macos-arm64` | aws-lc-rs | No |
| Windows x86_64 | `ocync-windows-amd64.exe` | aws-lc-rs | No |

FIPS mode is a compile-time decision. The `fips` feature flag links against the FIPS-validated aws-lc module. The `non-fips` feature flag uses aws-lc-rs without FIPS validation.

A FIPS build restricts TLS to FIPS-approved algorithms: AES-GCM cipher suites, key exchange over P-256, P-384 or the X25519MLKEM768 post-quantum hybrid, and extended master secret for TLS 1.2. ChaCha20-Poly1305 and plain X25519 are never offered. The restriction covers every TLS client in the binary, including the ones the AWS and Google SDKs build. A FIPS build checks this at startup and refuses to run if TLS is not FIPS-approved.

`rustls` counts X25519MLKEM768 as approved through its ML-KEM part, although its own source notes that ML-KEM in the AWS-LC FIPS module is FIPS-pending and that it follows the interpretation some regimes (such as FedRAMP Rev. 5 SC-13) allow for modules in that state. Treat the hybrid as approved only if your assessor accepts that interpretation.

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

The `latest-fips` Docker tag includes FIPS-validated cryptography:

```bash
docker pull public.ecr.aws/clowdhaus/ocync:latest-fips
```

## Verifying FIPS status

The `ocync version` command reports whether the binary is a FIPS build, the linked cryptographic module and its version, and whether TLS is restricted to FIPS-approved algorithms at runtime:

```
$ ocync version
ocync 0.8.2
FIPS 140-3: enabled
  module: AWS-LC FIPS 4.2.0
  tls: approved
```

The module version is what maps to a NIST validation certificate. Non-FIPS builds show `FIPS 140-3: disabled`. FIPS is a compile-time selection; there is no runtime toggle.

## Banned dependencies

`ring` and `native-tls` are banned via `deny.toml` to prevent accidental use of non-aws-lc-rs crypto. `reqwest` is configured with `rustls-no-provider`, and the crypto provider is installed explicitly at process startup.
