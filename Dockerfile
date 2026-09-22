# syntax=docker/dockerfile:1
#
# Two-stage build producing a small, statically-linked (musl) binary with
# no OpenSSL/native-tls in the dependency tree (TLS is rustls end to end),
# so the runtime image needs nothing but the binary and CA roots.

FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build

# Cache dependency builds separately from source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release \
    && strip target/release/jmap2telegram

FROM alpine:3.20 AS runtime
# Fixed, non-root UID/GID so Kubernetes securityContext (runAsUser/fsGroup)
# can pin the same identity without depending on adduser's auto-assignment.
RUN apk add --no-cache ca-certificates \
    && addgroup -S -g 101 jmap2telegram \
    && adduser -S -u 100 -G jmap2telegram -H -h /data jmap2telegram \
    && mkdir -p /data \
    && chown jmap2telegram:jmap2telegram /data

COPY --from=builder /build/target/release/jmap2telegram /usr/local/bin/jmap2telegram

USER jmap2telegram
WORKDIR /data
VOLUME ["/data"]
ENV DATA_DIR=/data

ENTRYPOINT ["/usr/local/bin/jmap2telegram"]
