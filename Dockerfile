# syntax=docker/dockerfile:1
#
# Multi-stage build producing a *fully static* binary on a `scratch` runtime:
# no libc, no shell, no package manager, no OS packages at all, so an image
# scanner has literally nothing to report (see "Container image" in
# SECURITY.md). That is only achievable by statically linking against musl,
# which is why the builder targets `<arch>-unknown-linux-musl` instead of the
# host's glibc target. The build stays arch-agnostic so the release workflow's
# multi-arch (amd64/arm64) publish keeps working unchanged.
#
# Two things the runtime image has to get right, both because `scratch` is
# truly empty:
#   * CA roots. teloxide's reqwest validates with webpki-roots (compiled in),
#     but jmap-client's reqwest 0.13 goes through rustls-platform-verifier,
#     which loads the *OS* trust store. So we copy Debian's bundle to the
#     canonical path and point rustls-native-certs at it explicitly; without
#     this, /login would fail TLS verification.
#   * identity. `scratch` has no /etc/passwd, so the container user must be
#     numeric and mirror the Helm chart's runAsUser/runAsGroup/fsGroup (65532).

FROM rust:1-slim-bookworm AS builder
# mold: fast linker. musl-tools: musl-gcc plus musl's crt objects and static
# libc, required to link the musl target. Both packages exist for amd64 and
# arm64, so this line is unchanged under the release workflow's QEMU arm64 leg.
RUN apt-get update && apt-get install -y --no-install-recommends mold musl-tools \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
# rustc has no bundled linker/crt for musl targets, so we pick the musl triple
# matching the machine we are actually building on (native per-arch: arm64 runs
# under QEMU).
RUN case "$(uname -m)" in \
      x86_64) musl_target=x86_64-unknown-linux-musl ;; \
      aarch64) musl_target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported build architecture: $(uname -m)" >&2; exit 1 ;; \
    esac \
    && echo "$musl_target" > /build/musl-target \
    && rustup target add "$musl_target"
RUN mkdir -p /build/.cargo && printf '%s\n' \
    "[target.$(cat /build/musl-target)]" \
    'rustflags = ["-C", "link-arg=-fuse-ld=mold", "-C", "target-feature=+crt-static"]' \
    > /build/.cargo/config.toml

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
    && cargo build --release --target "$(cat /build/musl-target)" \
    && rm -rf src

COPY src ./src
# The `i18n!` macro in src/main.rs embeds these at compile time, resolved
# relative to the crate root - so they must sit next to Cargo.toml in the
# builder, not just in the final image.
COPY locales ./locales
# The compiled binary must be copied out of the cache-mounted target/
# before the mount is torn down at the end of this RUN: anything left
# inside a cache mount does not become part of the image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    touch src/main.rs && cargo build --release --target "$(cat /build/musl-target)" \
    && cp "target/$(cat /build/musl-target)/release/jmap2telegram" /build/jmap2telegram \
    && strip /build/jmap2telegram

FROM scratch AS runtime
COPY --from=builder /build/jmap2telegram /usr/local/bin/jmap2telegram
# rustls-platform-verifier (via jmap-client's reqwest) loads the OS trust store
# at runtime; ship it at the path rustls-native-certs probes by default and set
# SSL_CERT_FILE so the lookup does not depend on probe heuristics in a distro
# that no longer exists.
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

# Numeric identity: scratch ships no /etc/passwd. Keep the Helm chart's
# runAsUser/runAsGroup/fsGroup (65532) in sync with this.
USER 65532:65532

WORKDIR /data
VOLUME ["/data"]
ENV DATA_DIR=/data

ENTRYPOINT ["/usr/local/bin/jmap2telegram"]
