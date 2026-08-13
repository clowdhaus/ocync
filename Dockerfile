# syntax=docker/dockerfile:1

# --- Builder stage ---
FROM cgr.dev/chainguard/rust:latest-dev AS builder

USER root

# FIPS build dependencies: Go, CMake, Perl (required by aws-lc-fips-sys).
# Go builds the delocator that assembles the FIPS module.
RUN apk add --no-cache cmake go perl

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
    cargo build --release --locked -p ocync && \
    mv target/"$CARGO_BUILD_TARGET"/release/ocync /usr/local/bin/ocync

# Verify the binary has no dynamic dependencies.
RUN ! readelf -d /usr/local/bin/ocync | grep -q NEEDED

# --- Runtime stage ---
FROM cgr.dev/chainguard/static:latest

COPY --from=builder /usr/local/bin/ocync /usr/local/bin/ocync

ENTRYPOINT ["ocync"]
