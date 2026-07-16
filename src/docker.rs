//! Read-only access to the Docker Engine API over a unix socket.
//!
//! Provides two tools:
//!   * [`get_logs`] — log snapshot for a service/container (F1);
//!   * [`get_container_metrics`] — per-container metrics (F2, the "per-container" part).
//!
//! Works uniformly for standalone and Swarm: Swarm services are matched by the
//! `com.docker.swarm.service.name` label.

use bollard::container::{
    ListContainersOptions, LogOutput, LogsOptions, MemoryStatsStats, Stats, StatsOptions,
};
use bollard::Docker;
use futures_util::stream::StreamExt;
use serde::Serialize;

/// Swarm label holding the name of the service a task container belongs to.
const SWARM_SERVICE_LABEL: &str = "com.docker.swarm.service.name";

/// Connects to the docker socket. `timeout` is the per-request timeout, in seconds.
pub fn connect(socket: &str) -> anyhow::Result<Docker> {
    let docker = Docker::connect_with_unix(socket, 120, bollard::API_DEFAULT_VERSION)?;
    Ok(docker)
}

/// A filter-matched container together with its identification.
#[derive(Debug, Clone)]
pub struct ResolvedContainer {
    pub id: String,
    pub name: String,
    pub service: Option<String>,
    pub state: String,
    pub status: String,
}

impl ResolvedContainer {
    /// Short (12-character) id.
    fn short_id(&self) -> String {
        self.id.chars().take(12).collect()
    }
}

/// Returns the containers matching `filter`.
///
/// `filter = None` → all containers. Otherwise matches by (in priority order):
/// Swarm service name, container name (substring), id prefix.
pub async fn resolve_containers(
    docker: &Docker,
    filter: Option<&str>,
) -> anyhow::Result<Vec<ResolvedContainer>> {
    let opts = ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    };
    let summaries = docker.list_containers(Some(opts)).await?;
    let mut out = Vec::new();
    for c in summaries {
        let id = c.id.unwrap_or_default();
        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_default();
        let service = c
            .labels
            .as_ref()
            .and_then(|l| l.get(SWARM_SERVICE_LABEL).cloned());
        let rc = ResolvedContainer {
            id,
            name,
            service,
            state: c.state.unwrap_or_default(),
            status: c.status.unwrap_or_default(),
        };
        if matches_filter(&rc, filter) {
            out.push(rc);
        }
    }
    Ok(out)
}

/// Checks whether a container matches the name filter.
fn matches_filter(c: &ResolvedContainer, filter: Option<&str>) -> bool {
    let needle = match filter {
        None => return true,
        Some("") => return true,
        Some(f) => f,
    };
    if let Some(svc) = &c.service {
        if svc == needle {
            return true;
        }
    }
    if c.name == needle || c.name.contains(needle) {
        return true;
    }
    c.id.starts_with(needle)
}

// ---------------------------------------------------------------------------
// F1: logs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct LogsResult {
    pub matched_containers: usize,
    pub count: usize,
    pub lines: Vec<LogLine>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct LogLine {
    pub container: String,
    pub stream: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    pub message: String,
}

/// Parameters for the `docker_logs` tool.
#[derive(Debug, Default)]
pub struct LogQuery {
    pub name: Option<String>,
    pub tail: u32,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub filter: Option<String>,
    pub level: Option<String>,
}

/// Collects logs from the filter-matched containers and applies post-filters.
pub async fn get_logs(docker: &Docker, q: &LogQuery) -> anyhow::Result<LogsResult> {
    let containers = resolve_containers(docker, q.name.as_deref()).await?;
    let mut lines = Vec::new();

    let opts = LogsOptions::<String> {
        follow: false,
        stdout: true,
        stderr: true,
        timestamps: true,
        tail: q.tail.to_string(),
        since: q.since.unwrap_or(0),
        until: q.until.unwrap_or(0),
    };

    for c in &containers {
        let mut stream = docker.logs(&c.id, Some(opts.clone()));
        while let Some(item) = stream.next().await {
            let (stream_name, raw) = match item? {
                LogOutput::StdOut { message } => ("stdout", message),
                LogOutput::StdErr { message } => ("stderr", message),
                LogOutput::Console { message } => ("console", message),
                LogOutput::StdIn { message } => ("stdin", message),
            };
            let text = String::from_utf8_lossy(&raw);
            for entry in text.split_inclusive('\n') {
                let entry = entry.trim_end_matches(['\n', '\r']);
                if entry.is_empty() {
                    continue;
                }
                let (timestamp, msg) = split_ts(entry);
                let level = detect_level(msg);

                if let Some(f) = &q.filter {
                    if !msg.to_lowercase().contains(&f.to_lowercase()) {
                        continue;
                    }
                }
                if let Some(want) = &q.level {
                    match &level {
                        Some(l) if l.eq_ignore_ascii_case(want) => {}
                        _ => continue,
                    }
                }
                lines.push(LogLine {
                    container: c.name.clone(),
                    stream: stream_name.to_string(),
                    timestamp,
                    level,
                    message: msg.to_string(),
                });
            }
        }
    }

    Ok(LogsResult {
        matched_containers: containers.len(),
        count: lines.len(),
        lines,
    })
}

/// Splits off the RFC3339 timestamp that Docker prepends when `timestamps: true`.
/// Returns `(Some(ts), rest)` or `(None, whole line)`.
pub fn split_ts(line: &str) -> (Option<String>, &str) {
    match line.split_once(' ') {
        Some((ts, rest)) if is_rfc3339(ts) => (Some(ts.to_string()), rest),
        _ => (None, line),
    }
}

/// Rough allocation-free "looks like RFC3339" check: `2026-07-08T14:30:00...`.
fn is_rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 20
        && b[4] == b'-'
        && b[7] == b'-'
        && (b[10] == b'T' || b[10] == b't')
        && b[13] == b':'
        && b[16] == b':'
}

/// Detects the log level from common tokens at the start of the message.
pub fn detect_level(msg: &str) -> Option<String> {
    const LEVELS: &[&str] = &[
        "TRACE", "DEBUG", "INFO", "WARNING", "WARN", "ERROR", "FATAL", "CRITICAL",
    ];
    // Look at the first few tokens (accounting for prefixes like "[INFO]", "INFO:").
    let head: String = msg.chars().take(64).collect();
    let upper = head.to_uppercase();
    for lvl in LEVELS {
        if token_present(&upper, lvl) {
            // Normalize synonyms.
            let norm = match *lvl {
                "WARNING" => "WARN",
                "CRITICAL" => "FATAL",
                other => other,
            };
            return Some(norm.to_string());
        }
    }
    None
}

/// Checks that the level appears as a standalone token (delimited by non-letters).
fn token_present(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0 || !haystack.as_bytes()[abs - 1].is_ascii_alphabetic();
        let after = abs + needle.len();
        let after_ok = after >= haystack.len() || !haystack.as_bytes()[after].is_ascii_alphabetic();
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle.len();
    }
    false
}

// ---------------------------------------------------------------------------
// F2: per-container metrics
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ContainerMetrics {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub state: String,
    pub status: String,
    pub restart_count: i64,
    pub cpu_percent: f64,
    pub memory: ContainerMemory,
    pub network: ContainerNetwork,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ContainerMemory {
    pub usage_bytes: u64,
    pub limit_bytes: u64,
    pub usage_percent: f64,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct ContainerNetwork {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
}

/// Collects metrics for the containers matching `filter` (None → all).
pub async fn get_container_metrics(
    docker: &Docker,
    filter: Option<&str>,
) -> anyhow::Result<Vec<ContainerMetrics>> {
    let containers = resolve_containers(docker, filter).await?;
    let mut out = Vec::new();
    for c in &containers {
        let restart_count = docker
            .inspect_container(&c.id, None)
            .await
            .ok()
            .and_then(|i| i.restart_count)
            .unwrap_or(0);

        // stats only make sense for running containers.
        let (cpu_percent, memory, network) = if c.state == "running" {
            match fetch_stats(docker, &c.id).await {
                Ok(s) => summarize_stats(&s),
                Err(_) => (0.0, ContainerMemory::empty(), ContainerNetwork::default()),
            }
        } else {
            (0.0, ContainerMemory::empty(), ContainerNetwork::default())
        };

        out.push(ContainerMetrics {
            id: c.short_id(),
            name: c.name.clone(),
            service: c.service.clone(),
            state: c.state.clone(),
            status: c.status.clone(),
            restart_count,
            cpu_percent,
            memory,
            network,
        });
    }
    Ok(out)
}

impl ContainerMemory {
    fn empty() -> Self {
        ContainerMemory {
            usage_bytes: 0,
            limit_bytes: 0,
            usage_percent: 0.0,
        }
    }
}

/// Takes a single stats snapshot (not a stream).
async fn fetch_stats(docker: &Docker, id: &str) -> anyhow::Result<Stats> {
    let opts = StatsOptions {
        stream: false,
        one_shot: false, // one_shot=false to get two CPU samples for the delta
    };
    let mut stream = docker.stats(id, Some(opts));
    match stream.next().await {
        Some(s) => Ok(s?),
        None => Err(anyhow::anyhow!("no stats data for {id}")),
    }
}

/// Computes CPU%, memory, and network from a docker stats snapshot.
/// The CPU formula is the standard Docker one (delta against precpu).
pub fn summarize_stats(s: &Stats) -> (f64, ContainerMemory, ContainerNetwork) {
    // CPU
    let cpu_delta = s
        .cpu_stats
        .cpu_usage
        .total_usage
        .saturating_sub(s.precpu_stats.cpu_usage.total_usage) as f64;
    let system_delta = s
        .cpu_stats
        .system_cpu_usage
        .unwrap_or(0)
        .saturating_sub(s.precpu_stats.system_cpu_usage.unwrap_or(0)) as f64;
    let ncpu = s
        .cpu_stats
        .online_cpus
        .or_else(|| {
            s.cpu_stats
                .cpu_usage
                .percpu_usage
                .as_ref()
                .map(|v| v.len() as u64)
        })
        .unwrap_or(1)
        .max(1) as f64;
    let cpu_percent = if system_delta > 0.0 && cpu_delta > 0.0 {
        round2((cpu_delta / system_delta) * ncpu * 100.0)
    } else {
        0.0
    };

    // Memory: used = usage - inactive_file (as in docker stats)
    let raw_usage = s.memory_stats.usage.unwrap_or(0);
    let inactive = match &s.memory_stats.stats {
        Some(MemoryStatsStats::V1(v1)) => v1.total_inactive_file,
        Some(MemoryStatsStats::V2(v2)) => v2.inactive_file,
        None => 0,
    };
    let usage = raw_usage.saturating_sub(inactive);
    let limit = s.memory_stats.limit.unwrap_or(0);
    let mem = ContainerMemory {
        usage_bytes: usage,
        limit_bytes: limit,
        usage_percent: if limit > 0 {
            round2(usage as f64 / limit as f64 * 100.0)
        } else {
            0.0
        },
    };

    // Network: sum across all interfaces.
    let mut net = ContainerNetwork::default();
    if let Some(nets) = &s.networks {
        for n in nets.values() {
            net.rx_bytes += n.rx_bytes;
            net.tx_bytes += n.tx_bytes;
            net.rx_errors += n.rx_errors;
            net.tx_errors += n.tx_errors;
        }
    }

    (cpu_percent, mem, net)
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(name: &str, service: Option<&str>, id: &str) -> ResolvedContainer {
        ResolvedContainer {
            id: id.to_string(),
            name: name.to_string(),
            service: service.map(|s| s.to_string()),
            state: "running".into(),
            status: "Up".into(),
        }
    }

    #[test]
    fn filter_none_matches_all() {
        assert!(matches_filter(&rc("web", None, "abc"), None));
        assert!(matches_filter(&rc("web", None, "abc"), Some("")));
    }

    #[test]
    fn filter_by_service_name() {
        let c = rc("monitoring.1.xyz", Some("monitoring_service"), "deadbeef00");
        assert!(matches_filter(&c, Some("monitoring_service")));
        assert!(!matches_filter(&c, Some("other_service")));
    }

    #[test]
    fn filter_by_container_name_substring() {
        let c = rc("app_web_1", None, "abc123");
        assert!(matches_filter(&c, Some("web")));
    }

    #[test]
    fn filter_by_id_prefix() {
        let c = rc("x", None, "deadbeefcafe");
        assert!(matches_filter(&c, Some("deadbeef")));
        assert!(!matches_filter(&c, Some("cafe")));
    }

    #[test]
    fn split_ts_extracts_rfc3339() {
        let (ts, msg) = split_ts("2026-07-08T14:30:00.123456789Z hello world");
        assert_eq!(ts.as_deref(), Some("2026-07-08T14:30:00.123456789Z"));
        assert_eq!(msg, "hello world");
    }

    #[test]
    fn split_ts_without_timestamp() {
        let (ts, msg) = split_ts("just a plain line");
        assert_eq!(ts, None);
        assert_eq!(msg, "just a plain line");
    }

    #[test]
    fn detect_level_variants() {
        assert_eq!(
            detect_level("ERROR something broke").as_deref(),
            Some("ERROR")
        );
        assert_eq!(detect_level("[INFO] started").as_deref(), Some("INFO"));
        assert_eq!(
            detect_level("time=... level=warn msg=x").as_deref(),
            Some("WARN")
        );
        assert_eq!(detect_level("WARNING: disk low").as_deref(), Some("WARN"));
        assert_eq!(detect_level("CRITICAL failure").as_deref(), Some("FATAL"));
        assert_eq!(detect_level("just a message"), None);
    }

    #[test]
    fn detect_level_no_false_positive_substring() {
        // "INFORMATION" must not match as INFO.
        assert_eq!(detect_level("INFORMATIONAL note"), None);
        // "ERRORS" is not an ERROR token.
        assert_eq!(detect_level("many ERRORS occurred"), None);
    }
}
