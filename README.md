# docker-monitor-mcp

[![CI](https://github.com/nizovtsevnv/docker-monitor-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/nizovtsevnv/docker-monitor-mcp/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.82%2B-orange.svg)](https://www.rust-lang.org/)

A **stateless, read-only** MCP ([Model Context Protocol](https://modelcontextprotocol.io))
server that gives AI agents programmatic access to **Docker container logs** and
**host/container metrics** over HTTP — no shelling into the host.

Built for AI-agent runtimes (Claude Code, Codex, and any MCP client) that need
to inspect what's happening on a Docker node while working on a task.

Full specification: [`docs/SPEC.md`](docs/SPEC.md).

## Tools

| Tool | What it returns |
|------|-----------------|
| `docker_logs` | Log snapshot for a service/container: `tail`, time window (`since`/`until`), substring `filter`, `level`. No live-follow. |
| `host_metrics` | Host CPU (load avg + per-core usage), memory + swap, disks (usage + I/O), network (rx/tx + errors/drops). |
| `container_metrics` | Per-container CPU%, memory usage/limit, network I/O, status, restart count. |

Everything is **read-only** — the service never starts, stops, or changes
anything.

## Transport

MCP over **Streamable HTTP** (JSON-RPC 2.0), sessionless — each POST is
self-contained. Endpoints:

- `POST /mcp` (and `POST /`) — JSON-RPC (object or batch).
- `GET /health` — `200 ok` for Docker HEALTHCHECK.

Supported methods: `initialize`, `ping`, `tools/list`, `tools/call`.

## Quick start

### Run with Docker

```bash
docker run --rm -p 127.0.0.1:8080:8080 \
  -v /var/run/docker.sock:/var/run/docker.sock:ro \
  -v /proc:/host/proc:ro \
  -v /:/host/rootfs:ro \
  -e HOST_PROC=/host/proc -e HOST_ROOTFS=/host/rootfs \
  ghcr.io/nizovtsevnv/docker-monitor-mcp:latest
```

Or via compose / Swarm — see [`deploy/`](deploy/).

### Run locally

```bash
nix develop -c cargo run          # reads the real /proc, /
# MCP:    http://127.0.0.1:8080/mcp
# health: http://127.0.0.1:8080/health
```

### Build the image

```bash
docker build -t docker-monitor-mcp .    # static musl binary on alpine
```

## Example calls

```bash
# list tools
curl -s -X POST http://127.0.0.1:8080/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'

# last 50 ERROR lines of a service
curl -s -X POST http://127.0.0.1:8080/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"docker_logs","arguments":{"name":"web","tail":50,"level":"ERROR"}}}'

# host metrics
curl -s -X POST http://127.0.0.1:8080/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"host_metrics","arguments":{}}}'
```

## Wiring into an MCP client

The server speaks MCP over HTTP, so point your client at the `/mcp` endpoint.
See [`.mcp.example.json`](.mcp.example.json).

**Claude Code:**

```bash
claude mcp add --transport http docker-monitor http://docker-monitor-mcp:8080/mcp
# with auth enabled (server started with AUTH_TOKEN):
claude mcp add --transport http docker-monitor http://docker-monitor-mcp:8080/mcp \
  --header "Authorization: Bearer <token>"
```

Use the Docker/Swarm service DNS name when the client runs in the same network
(e.g. `http://docker-monitor-mcp:8080/mcp`), or `http://127.0.0.1:8080/mcp`
locally.

## Configuration

All via environment variables (defaults target a local run):

| Variable | Default | Purpose |
|----------|---------|---------|
| `BIND_ADDR` | `0.0.0.0:8080` | MCP HTTP listen address |
| `DOCKER_SOCKET` | `/var/run/docker.sock` | Docker socket (mounted ro) |
| `HOST_PROC` | `/proc` | procfs root (container: `/host/proc`) |
| `HOST_ROOTFS` | `/` | host FS root for statvfs (container: `/host/rootfs`) |
| `AUTH_TOKEN` | — (off) | optional bearer token; when set, MCP endpoints require `Authorization: Bearer <token>` |
| `RUST_LOG` | `info` | log level (stderr) |

See [`docs/SPEC.md §4`](docs/SPEC.md#4-configuration-environment-variables) for
details, including host-namespace handling.

## Security

- **Read-only, but broad.** The server exposes logs and metrics for **every
  container on the node it runs on**, plus host metrics. Keep it on a private
  overlay; do not expose the endpoint to untrusted networks.
- **Authentication is optional and off by default.** Set `AUTH_TOKEN` to require
  `Authorization: Bearer <token>` on the MCP endpoints (`/mcp`, `/`); `/health`
  stays open for Docker HEALTHCHECK. The token is compared in constant time.
  With `AUTH_TOKEN` unset the perimeter is the network alone.
- Mounts (`docker.sock`, `/proc`, `/`) are all `:ro`.
- A Swarm service only sees the node it is scheduled on — pin `placement`.

## Development

```bash
nix develop          # rust toolchain + a pre-commit hook (fmt + clippy + test)
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

## License

[MIT](LICENSE) © Nick Nizovtsev
