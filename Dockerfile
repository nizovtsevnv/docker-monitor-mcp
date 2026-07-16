FROM rust:1.82-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
# rust:alpine defaults to the musl target → static binary.
RUN cargo build --release

FROM alpine:3.20
RUN apk add --no-cache ca-certificates
COPY --from=builder /app/target/release/docker-monitor-mcp /usr/local/bin/docker-monitor-mcp
# MCP HTTP (Streamable HTTP, JSON-RPC 2.0).
EXPOSE 8080
# busybox wget ships in the base alpine image.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8080/health || exit 1
ENTRYPOINT ["docker-monitor-mcp"]
