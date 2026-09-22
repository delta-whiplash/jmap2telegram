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

# Cache dependency builds separately from source changes: this layer is
# only invalidated when Cargo.toml/Cargo.lock change, not on every source
# edit. Combined with the GitHub Actions buildx cache (see release.yml /
# ci.yml), it's what makes repeat builds fast rather than the Dockerfile
# structure alone.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release \
    && strip target/release/jmap2telegram

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
# distroless nonroot's fixed identity (65532:65532) — keep the Helm
# chart's securityContext in sync with this.
COPY --from=builder /build/target/release/jmap2telegram /usr/local/bin/jmap2telegram

WORKDIR /data
VOLUME ["/data"]
ENV DATA_DIR=/data

ENTRYPOINT ["/usr/local/bin/jmap2telegram"]
