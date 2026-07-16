//! MCP layer: JSON-RPC 2.0 over HTTP (Streamable HTTP transport, sessionless).
//!
//! The service is stateless: no MCP sessions are kept, every POST is self-contained.
//! Requests get an `application/json` response; notifications (`notifications/*`) are acked with 202.
//!
//! Supported methods: `initialize`, `ping`, `tools/list`, `tools/call`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::config::Config;
use crate::docker::{self, LogQuery};
use crate::metrics;

/// MCP protocol version supported by the server (fallback when the client sends none).
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Shared state: the Docker client and config. Immutable.
pub struct AppState {
    pub docker: bollard::Docker,
    pub config: Config,
}

pub type SharedState = Arc<AppState>;

/// Builds the MCP HTTP router.
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", post(mcp_handler))
        .route("/mcp", post(mcp_handler))
        .route("/health", get(health))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Single Streamable HTTP entry point.
async fn mcp_handler(State(state): State<SharedState>, body: Bytes) -> Response {
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return Json(error_response(Value::Null, -32700, "Parse error")).into_response();
        }
    };

    match parsed {
        Value::Array(items) => {
            let mut responses = Vec::new();
            for item in items {
                if let Some(resp) = process_single(&state, item).await {
                    responses.push(resp);
                }
            }
            if responses.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(responses)).into_response()
            }
        }
        obj @ Value::Object(_) => match process_single(&state, obj).await {
            Some(resp) => Json(resp).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
        _ => Json(error_response(Value::Null, -32600, "Invalid Request")).into_response(),
    }
}

/// Handles a single JSON-RPC object. Returns `None` for notifications (no `id`).
async fn process_single(state: &SharedState, msg: Value) -> Option<Value> {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let is_notification = id.is_none();

    // Notifications (initialized, etc.) require no response.
    if is_notification {
        return None;
    }
    let id = id.unwrap_or(Value::Null);

    let result = dispatch(state, method, params).await;
    Some(match result {
        Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
        Err(rpc) => error_response(id, rpc.code, &rpc.message),
    })
}

/// A JSON-RPC-level error.
struct RpcError {
    code: i64,
    message: String,
}

fn rpc_err(code: i64, msg: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: msg.into(),
    }
}

/// Routes MCP methods.
async fn dispatch(state: &SharedState, method: &str, params: Value) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => tools_call(state, params).await,
        other => Err(rpc_err(-32601, format!("Method not found: {other}"))),
    }
}

fn initialize_result(params: &Value) -> Value {
    let protocol = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "docker-monitor-mcp",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Read-only monitoring of the host and its Docker containers. Tools: docker_logs, host_metrics, container_metrics."
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

// ---------------------------------------------------------------------------
// Tool definitions (tools/list)
// ---------------------------------------------------------------------------

/// JSON schemas for all MCP tools. Keep in sync with `docs/SPEC.md`.
pub fn tool_definitions() -> Value {
    json!([
        {
            "name": "docker_logs",
            "description": "Snapshot of logs for a service/container (no live-follow). Filters: tail, time window, substring, level.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Swarm service name, container name (substring) or id prefix. If omitted — all containers."},
                    "tail": {"type": "integer", "description": "How many trailing lines to return.", "default": 100, "minimum": 1, "maximum": 1000},
                    "since": {"type": "string", "description": "Window start, ISO 8601 (e.g. 2026-07-08T14:00:00Z)."},
                    "until": {"type": "string", "description": "Window end, ISO 8601."},
                    "filter": {"type": "string", "description": "Substring (case-insensitive) to filter messages."},
                    "level": {"type": "string", "description": "Log level: TRACE|DEBUG|INFO|WARN|ERROR|FATAL (when recognized in the stream)."}
                }
            }
        },
        {
            "name": "host_metrics",
            "description": "Host metrics: CPU (load avg + per-core usage), memory+swap, disks (usage + I/O), network (rx/tx + errors/drops). Pre-aggregated JSON.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "container_metrics",
            "description": "Per-container metrics: CPU%, memory usage/limit, network I/O, status, restart count.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Swarm service name, container name (substring) or id prefix. If omitted — all containers."}
                }
            }
        }
    ])
}

// ---------------------------------------------------------------------------
// tools/call
// ---------------------------------------------------------------------------

async fn tools_call(state: &SharedState, params: Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| rpc_err(-32602, "tools/call: missing 'name' field"))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let outcome = match name {
        "docker_logs" => tool_docker_logs(state, &args).await,
        "host_metrics" => tool_host_metrics(state).await,
        "container_metrics" => tool_container_metrics(state, &args).await,
        other => return Err(rpc_err(-32602, format!("Unknown tool: {other}"))),
    };

    Ok(match outcome {
        Ok(value) => tool_success(value),
        // Tool execution errors are returned as isError (per the MCP contract),
        // not as a JSON-RPC error, so the agent sees the problem text.
        Err(e) => tool_error(&e.to_string()),
    })
}

/// Wraps a tool result in MCP content + structuredContent.
fn tool_success(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": false
    })
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": message}],
        "isError": true
    })
}

async fn tool_docker_logs(state: &SharedState, args: &Value) -> anyhow::Result<Value> {
    let q = LogQuery {
        name: opt_string(args, "name"),
        tail: parse_tail(args),
        since: opt_iso_to_unix(args, "since")?,
        until: opt_iso_to_unix(args, "until")?,
        filter: opt_string(args, "filter"),
        level: opt_string(args, "level"),
    };
    let result = docker::get_logs(&state.docker, &q).await?;
    Ok(serde_json::to_value(result)?)
}

async fn tool_host_metrics(state: &SharedState) -> anyhow::Result<Value> {
    let m = metrics::collect_host_metrics(&state.config).await?;
    Ok(serde_json::to_value(m)?)
}

async fn tool_container_metrics(state: &SharedState, args: &Value) -> anyhow::Result<Value> {
    let name = opt_string(args, "name");
    let list = docker::get_container_metrics(&state.docker, name.as_deref()).await?;
    Ok(json!({ "count": list.len(), "containers": list }))
}

// ---- argument parsing ----

fn opt_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// `tail`: default 100, max 1000, min 1.
pub fn parse_tail(args: &Value) -> u32 {
    let raw = args.get("tail").and_then(|v| v.as_u64()).unwrap_or(100);
    raw.clamp(1, 1000) as u32
}

/// Parses an ISO 8601 string into unix seconds (for docker logs since/until).
fn opt_iso_to_unix(args: &Value, key: &str) -> anyhow::Result<Option<i64>> {
    match opt_string(args, key) {
        None => Ok(None),
        Some(s) => Ok(Some(iso_to_unix(&s)?)),
    }
}

/// Parses RFC3339 / ISO 8601 into unix seconds.
pub fn iso_to_unix(s: &str) -> anyhow::Result<i64> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|e| anyhow::anyhow!("invalid date '{s}' (ISO 8601 required): {e}"))?;
    Ok(dt.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_default_and_clamp() {
        assert_eq!(parse_tail(&json!({})), 100);
        assert_eq!(parse_tail(&json!({"tail": 50})), 50);
        assert_eq!(parse_tail(&json!({"tail": 5000})), 1000);
        assert_eq!(parse_tail(&json!({"tail": 0})), 1);
    }

    #[test]
    fn iso_parses_to_unix() {
        assert_eq!(iso_to_unix("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(iso_to_unix("2026-07-08T00:00:00Z").unwrap(), 1783468800);
        assert!(iso_to_unix("not-a-date").is_err());
    }

    #[test]
    fn opt_string_ignores_empty() {
        assert_eq!(
            opt_string(&json!({"name": "web"}), "name"),
            Some("web".into())
        );
        assert_eq!(opt_string(&json!({"name": ""}), "name"), None);
        assert_eq!(opt_string(&json!({}), "name"), None);
    }

    #[test]
    fn tool_definitions_lists_three_tools() {
        let defs = tool_definitions();
        let arr = defs.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        let names: Vec<&str> = arr.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"docker_logs"));
        assert!(names.contains(&"host_metrics"));
        assert!(names.contains(&"container_metrics"));
    }

    #[test]
    fn initialize_echoes_protocol_version() {
        let r = initialize_result(&json!({"protocolVersion": "2024-11-05"}));
        assert_eq!(r["protocolVersion"], "2024-11-05");
        assert_eq!(r["serverInfo"]["name"], "docker-monitor-mcp");
        assert!(r["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_defaults_protocol_version() {
        let r = initialize_result(&json!({}));
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn tool_success_has_content_and_structured() {
        let v = tool_success(json!({"a": 1}));
        assert_eq!(v["isError"], false);
        assert_eq!(v["structuredContent"]["a"], 1);
        assert_eq!(v["content"][0]["type"], "text");
    }

    #[test]
    fn error_response_shape() {
        let e = error_response(json!(7), -32601, "nope");
        assert_eq!(e["id"], 7);
        assert_eq!(e["error"]["code"], -32601);
        assert_eq!(e["error"]["message"], "nope");
    }
}
