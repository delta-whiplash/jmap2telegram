# syntax=docker/dockerfile:1
#
# Two-stage build. Deliberately glibc (Debian), not musl/Alpine: musl's
# allocator makes multi-core rustc/cargo builds noticeably slower than
# glibc for a dependency tree this size, and that difference is the
# dominant cost in CI, not final image size (we don't ship OpenSSL either
# way — TLS is rustls end to end, so glibc adds no extra runtime
# dependency risk). The runtime stage is distroless: no shell, no package
# manager, nothing beyond the binary, libc, and CA roots.

FROM rust:1-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends mold \
    && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /build/.cargo && printf '%s\n' \
    '[target.x86_64-unknown-linux-gnu]' \
    'rustflags = ["-C", "link-arg=-fuse-ld=mold"]' \
    '[target.aarch64-unknown-linux-gnu]' \
    'rustflags = ["-C", "link-arg=-fuse-ld=mold"]' \
    > /build/.cargo/config.toml
WORKDIR /build

# Cache dependency builds separately from source changes (this layer only
# invalidates when Cargo.toml/Cargo.lock change), *and* mount the cargo
# registry + target dir as BuildKit cache mounts on top of that. The
# layer cache alone only survives unchanged; the mounts persist actual
# rustc/cargo incremental state across builds via the GHA cache backend
# (cache-from/cache-to: type=gha in release.yml/ci.yml), so even a
# Cargo.lock bump only recompiles the crates that actually changed
# instead of the whole dependency tree from zero.
COPY Cargo.toml Cargo.lock ./
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# The compiled binary must be copied out of the cache-mounted target/
# before the mount is torn down at the end of this RUN: anything left
# inside a cache mount does not become part of the image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    touch src/main.rs && cargo build --release \
    && cp target/release/jmap2telegram /build/jmap2telegram \
    && strip /build/jmap2telegram

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
# distroless nonroot's fixed identity (65532:65532) — keep the Helm
# chart's securityContext in sync with this.
COPY --from=builder /build/jmap2telegram /usr/local/bin/jmap2telegram

WORKDIR /data
VOLUME ["/data"]
ENV DATA_DIR=/data

ENTRYPOINT ["/usr/local/bin/jmap2telegram"]
