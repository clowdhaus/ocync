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

## FIPS endpoints for ECR

When `AWS_USE_FIPS_ENDPOINT=true`, `ocync` rewrites ECR hostnames to use FIPS endpoints:

```
123456789012.dkr.ecr.us-east-1.amazonaws.com
  -> 123456789012.dkr.ecr-fips.us-east-1.amazonaws.com
```

This ensures the entire TLS chain (client crypto + endpoint) is FIPS-compliant. Only ECR has FIPS endpoints among supported registries.

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

The `ocync version` command reports whether the binary was compiled with FIPS:

```
$ ocync version
ocync 0.1.0
FIPS 140-3: compiled=yes
```

Non-FIPS builds show `compiled=no`. FIPS is a compile-time selection; there is no runtime toggle.

## Banned dependencies

`ring` and `native-tls` are banned via `deny.toml` to prevent accidental use of non-aws-lc-rs crypto. `reqwest` is configured with `rustls-no-provider`, and the crypto provider is installed explicitly at process startup.
