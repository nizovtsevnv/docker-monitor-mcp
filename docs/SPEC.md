# `docker-monitor-mcp` specification

A stateless, read-only MCP server that gives AI agents (and any MCP client)
programmatic access to Docker container logs and host/container metrics over
HTTP. This document is the source of truth and is kept in sync with the code
(`src/`) — any code↔spec divergence is a bug.

## 1. Purpose and scope

- One programmatic entry point for reading host and container state — container
  logs and server metrics — without shelling into the host.
- **Read-only.** The service never changes anything in the environment.
- Non-goals: container lifecycle management (start/stop/restart), metrics
  history and alerting, live log follow, `docker system info`.

## 2. Transport: MCP over HTTP

- Protocol — **MCP** (Model Context Protocol), wire format — **JSON-RPC 2.0**.
- Transport — **Streamable HTTP** without sessions (the service is stateless):
  every request is self-contained, `Mcp-Session-Id` is not used.
- Endpoints:
  - `POST /` and `POST /mcp` — accept JSON-RPC (object or batch array).
    - Request (with `id`) → `200 application/json` with a JSON-RPC result/error.
    - Notification (no `id`, e.g. `notifications/initialized`) → `202 Accepted`, no body.
  - `GET /health` → `200 ok` (for Docker HEALTHCHECK).
- Supported MCP methods: `initialize`, `ping`, `tools/list`, `tools/call`.
- `serverInfo.name` = `docker-monitor-mcp`. `protocolVersion` is echoed back from
  the client's request (fallback `2025-06-18`).

A successful `tools/call` returns:
```json
{
  "content": [{"type": "text", "text": "<pretty-printed JSON result>"}],
  "structuredContent": { ... },
  "isError": false
}
```
A tool execution failure is returned as `isError: true` with the reason in
`content[0].text` (not as a JSON-RPC error) so the agent can see the cause.

**Authentication (optional).** If `AUTH_TOKEN` is set, `POST /` and `POST /mcp`
require the header `Authorization: Bearer <AUTH_TOKEN>`; a missing or wrong token
yields `401` with `WWW-Authenticate: Bearer`. The token is compared in constant
time. `GET /health` is always open (for Docker HEALTHCHECK). With `AUTH_TOKEN`
unset (default) no authentication is performed and the perimeter is the network.

## 3. Tools

### 3.1 `docker_logs`

Snapshot of logs for a service/container. **No** live-follow/stream.

Arguments:

| Field    | Type    | Default    | Description |
|----------|---------|------------|-------------|
| `name`   | string  | — (all)    | Swarm service name, container name (substring) or id prefix |
| `tail`   | integer | `100`      | Number of trailing lines; clamped to `1..1000` |
| `since`  | string  | —          | Window start, ISO 8601 (e.g. `2026-07-08T14:00:00Z`) |
| `until`  | string  | —          | Window end, ISO 8601 |
| `filter` | string  | —          | Substring, **case-insensitive**, matched against the message text |
| `level`  | string  | —          | Level: `TRACE\|DEBUG\|INFO\|WARN\|ERROR\|FATAL`, when recognized in the stream |

`name` resolution (uniform standalone/Swarm): match by Swarm service name
(label `com.docker.swarm.service.name`), then by container name (substring),
then by id prefix. For a service with several tasks, logs are aggregated across
all matched containers; every line is tagged with its container name.

`level` is detected heuristically from a token at the start of the message
(`ERROR`, `[INFO]`, `level=warn`, …); `WARNING`→`WARN`, `CRITICAL`→`FATAL`.

Result (`structuredContent`):
```json
{
  "matched_containers": 1,
  "count": 2,
  "lines": [
    {"container": "web", "stream": "stdout", "timestamp": "2026-07-08T14:30:00.1Z", "level": "INFO", "message": "started"},
    {"container": "web", "stream": "stderr", "timestamp": "2026-07-08T14:30:01.2Z", "level": "ERROR", "message": "boom"}
  ]
}
```
`timestamp` and `level` are omitted when unavailable.

### 3.2 `host_metrics`

Host metrics. No parameters. CPU `usage_percent` is computed from a `/proc/stat`
delta over a short interval (~200 ms).

Result (`structuredContent`):
```json
{
  "timestamp": "2026-07-08T14:30:00Z",
  "host": "node-1",
  "cpu": {
    "load_avg": [1.2, 0.8, 0.5],
    "usage_percent": 35.2,
    "cores": 4,
    "per_core_usage_percent": [40.0, 30.0, 35.0, 35.8]
  },
  "memory": {
    "total_bytes": 0, "used_bytes": 0, "free_bytes": 0, "available_bytes": 0,
    "swap_total_bytes": 0, "swap_used_bytes": 0, "usage_percent": 0.0
  },
  "disk": [
    {"mount": "/", "device": "/dev/sda1", "fstype": "ext4",
     "total_bytes": 0, "used_bytes": 0, "available_bytes": 0, "usage_percent": 0.0,
     "io_read_ops": 0, "io_write_ops": 0}
  ],
  "network": {
    "eth0": {"rx_bytes": 0, "tx_bytes": 0, "rx_errors": 0, "tx_errors": 0, "rx_drops": 0, "tx_drops": 0}
  }
}
```
Format is pre-aggregated: raw fields (`*_bytes`, `*_ops`) alongside
human-readable ones (`usage_percent`). Pseudo-filesystems (tmpfs, proc, cgroup,
overlay, …) are excluded from `disk`; devices are de-duplicated. The `lo`
interface is excluded from `network`.

### 3.3 `container_metrics`

Per-container metrics. `name` — same semantics as `docker_logs` (optional;
without it — all containers). For stopped containers metrics are zero but
`status`/`restart_count` are populated.

CPU% uses the standard Docker formula (delta against `precpu`). Memory is
`usage` minus `inactive_file` (as in `docker stats`). Network is summed across
interfaces.

Result (`structuredContent`):
```json
{
  "count": 1,
  "containers": [
    {
      "id": "deadbeef0000", "name": "web", "service": "app_web",
      "state": "running", "status": "Up 2 hours", "restart_count": 0,
      "cpu_percent": 12.5,
      "memory": {"usage_bytes": 0, "limit_bytes": 0, "usage_percent": 0.0},
      "network": {"rx_bytes": 0, "tx_bytes": 0, "rx_errors": 0, "tx_errors": 0}
    }
  ]
}
```

## 4. Configuration (environment variables)

| Variable        | Default                 | Purpose |
|-----------------|-------------------------|---------|
| `BIND_ADDR`     | `0.0.0.0:8080`          | MCP HTTP listen address |
| `DOCKER_SOCKET` | `/var/run/docker.sock`  | Docker socket path (mounted ro) |
| `HOST_PROC`     | `/proc`                 | procfs root (in a container — `/host/proc`) |
| `HOST_ROOTFS`   | `/`                     | Host filesystem root for statvfs (in a container — `/host/rootfs`) |
| `AUTH_TOKEN`    | — (unset → auth off)    | Optional bearer token required on `/mcp` and `/` |
| `RUST_LOG`      | `info`                  | Log level (stderr) |

Defaults target a local run/tests (real `/proc`, `/`). In a container, host
paths are mounted and overridden via env (see `deploy/`).

**Host namespace.** `mounts` and `net/dev` are namespace-dependent: the service
first reads `{HOST_PROC}/1/{file}` (PID 1's view = the host, when the host
`/proc` is mounted), falling back to `{HOST_PROC}/{file}`. CPU/mem/diskstats are
system-global.

## 5. Deployment

- Runs as a container; a `docker compose` example and a Docker Swarm example are
  in `deploy/`.
- Read-only mounts: the docker socket, `/proc`, and `/` (rootfs). The service
  never writes.
- Authentication is optional (`AUTH_TOKEN`, off by default). Regardless, keep the
  endpoint on a private network — do not expose it to untrusted networks.
- A Swarm service only observes the docker socket / procfs of the **node it runs
  on**; pin `placement` accordingly.
- The image is a static musl binary on top of alpine; CI publishes it to GHCR.

## 6. Testing

`cargo test`. Covered by pure unit tests: procfs parsers
(stat/loadavg/meminfo/net/mounts/diskstats), CPU/disk assembly, container
resolution and filters, timestamp and log-level parsing, MCP argument parsing,
JSON-RPC response/error construction. Docker/procfs I/O is isolated behind
reader functions and is not exercised by the unit tests.
