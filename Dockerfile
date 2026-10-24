# syntax=docker/dockerfile:1

# --- Builder stage ---
FROM cgr.dev/chainguard/rust:latest-dev AS builder

USER root

# FIPS build dependencies: Go, CMake, Perl (required by aws-lc-fips-sys).
# Go builds the delocator that assembles the FIPS module. cargo-auditable
# embeds the dependency list the release bill of materials is read from.
# Keep cargo-auditable in step with .github/actions/setup-rust.
RUN apk add --no-cache cargo-auditable~0.7.7 cmake go perl

WORKDIR /src
COPY . .

# Build with FIPS (default feature) and static CRT linking.
# gnu target (not musl) - aws-lc-rs FIPS certification is against glibc.
# +crt-static produces a fully static binary with no dynamic dependencies.
# CARGO_BUILD_TARGET triggers host/target separation so proc-macros
# (which must be dynamic libraries) are not compiled with +crt-static.
ENV CARGO_BUILD_RUSTFLAGS="-C target-feature=+crt-static"
RUN CARGO_BUILD_TARGET=$(rustc -vV | awk '/^host:/ { print $2 }') && \
    export CARGO_BUILD_TARGET && \
    cargo auditable build --release --locked -p ocync && \
    mv target/"$CARGO_BUILD_TARGET"/release/ocync /usr/local/bin/ocync

# Verify the binary has no dynamic dependencies.
RUN ! readelf -d /usr/local/bin/ocync | grep -q NEEDED

# --- Runtime stage ---
FROM cgr.dev/chainguard/static:latest

COPY --from=builder /usr/local/bin/ocync /usr/local/bin/ocync

ENTRYPOINT ["ocync"]
