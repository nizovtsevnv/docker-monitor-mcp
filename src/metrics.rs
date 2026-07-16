//! Collects host server metrics from procfs/sysfs (Linux).
//!
//! All raw parsers (`parse_*`) are pure functions over file contents, so they
//! can be unit-tested against fixtures without access to a real `/proc`.
//! The reader functions (`read_*` / [`collect_host_metrics`]) touch the filesystem.
//!
//! Output format is pre-aggregated JSON: raw fields (`*_bytes`, `*_ops`) alongside
//! human-readable ones (`usage_percent`). See the spec in `docs/SPEC.md`.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::path::Path;

use chrono::Utc;
use serde::Serialize;

use crate::config::Config;

// ---------------------------------------------------------------------------
// Output types (serialized into the host_metrics tool's JSON response)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, PartialEq)]
pub struct HostMetrics {
    /// Snapshot timestamp, ISO 8601 UTC.
    pub timestamp: String,
    pub host: String,
    pub cpu: CpuMetrics,
    pub memory: MemoryMetrics,
    pub disk: Vec<DiskMetrics>,
    pub network: BTreeMap<String, NetworkMetrics>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct CpuMetrics {
    /// Load average over 1/5/15 minutes.
    pub load_avg: [f64; 3],
    /// Total CPU usage in percent (from the delta between two snapshots).
    pub usage_percent: f64,
    /// Number of logical cores.
    pub cores: usize,
    /// Per-core usage, in percent.
    pub per_core_usage_percent: Vec<f64>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct MemoryMetrics {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub usage_percent: f64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct DiskMetrics {
    pub mount: String,
    pub device: String,
    pub fstype: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub usage_percent: f64,
    pub io_read_ops: u64,
    pub io_write_ops: u64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct NetworkMetrics {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_drops: u64,
    pub tx_drops: u64,
}

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

/// Counter values from a `cpu*` line of `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

/// Parses `/proc/stat`, returning the aggregate (`"cpu"`) and per-core (`"cpu0"`…) counters.
pub fn parse_proc_stat(content: &str) -> BTreeMap<String, CpuTimes> {
    let mut out = BTreeMap::new();
    for line in content.lines() {
        if !line.starts_with("cpu") {
            continue;
        }
        let mut it = line.split_whitespace();
        let label = match it.next() {
            Some(l) => l,
            None => continue,
        };
        // Fields: user nice system idle iowait irq softirq steal guest guest_nice
        let vals: Vec<u64> = it.filter_map(|v| v.parse::<u64>().ok()).collect();
        if vals.len() < 4 {
            continue;
        }
        let idle = vals[3] + vals.get(4).copied().unwrap_or(0); // idle + iowait
        let total: u64 = vals.iter().sum();
        out.insert(label.to_string(), CpuTimes { total, idle });
    }
    out
}

/// Computes usage percent from the counter delta between two snapshots.
fn cpu_usage_from_delta(prev: CpuTimes, cur: CpuTimes) -> f64 {
    let total_d = cur.total.saturating_sub(prev.total);
    let idle_d = cur.idle.saturating_sub(prev.idle);
    if total_d == 0 {
        return 0.0;
    }
    let usage = 1.0 - (idle_d as f64 / total_d as f64);
    round2((usage.clamp(0.0, 1.0)) * 100.0)
}

/// Parses `/proc/loadavg` → `[1m, 5m, 15m]`.
pub fn parse_loadavg(content: &str) -> [f64; 3] {
    let mut it = content.split_whitespace();
    let a = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let b = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let c = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    [a, b, c]
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// Parses `/proc/meminfo` (values in kB) into a memory struct (bytes).
pub fn parse_meminfo(content: &str) -> MemoryMetrics {
    let mut map: BTreeMap<&str, u64> = BTreeMap::new();
    for line in content.lines() {
        let mut parts = line.split(':');
        let key = match parts.next() {
            Some(k) => k.trim(),
            None => continue,
        };
        let val = parts.next().unwrap_or("").trim();
        // "12345 kB" → 12345
        if let Some(num) = val.split_whitespace().next() {
            if let Ok(kb) = num.parse::<u64>() {
                map.insert(key, kb * 1024);
            }
        }
    }
    let total = map.get("MemTotal").copied().unwrap_or(0);
    let free = map.get("MemFree").copied().unwrap_or(0);
    // MemAvailable exists on kernels ≥ 3.14; otherwise approximate as free+buffers+cached.
    let available = map.get("MemAvailable").copied().unwrap_or_else(|| {
        free + map.get("Buffers").copied().unwrap_or(0) + map.get("Cached").copied().unwrap_or(0)
    });
    let swap_total = map.get("SwapTotal").copied().unwrap_or(0);
    let swap_free = map.get("SwapFree").copied().unwrap_or(0);
    let used = total.saturating_sub(available);
    let usage_percent = if total > 0 {
        round2(used as f64 / total as f64 * 100.0)
    } else {
        0.0
    };
    MemoryMetrics {
        total_bytes: total,
        used_bytes: used,
        free_bytes: free,
        available_bytes: available,
        swap_total_bytes: swap_total,
        swap_used_bytes: swap_total.saturating_sub(swap_free),
        usage_percent,
    }
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// Parses `/proc/net/dev`. The `lo` interface is skipped.
pub fn parse_net_dev(content: &str) -> BTreeMap<String, NetworkMetrics> {
    let mut out = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        // Table headers don't contain ':'.
        let (iface, rest) = match line.split_once(':') {
            Some(v) => v,
            None => continue,
        };
        let iface = iface.trim();
        if iface == "lo" || iface.is_empty() {
            continue;
        }
        let f: Vec<u64> = rest
            .split_whitespace()
            .map(|v| v.parse::<u64>().unwrap_or(0))
            .collect();
        // net/dev field order:
        // rx: bytes packets errs drop fifo frame compressed multicast (0..8)
        // tx: bytes packets errs drop fifo colls carrier compressed (8..16)
        if f.len() < 16 {
            continue;
        }
        out.insert(
            iface.to_string(),
            NetworkMetrics {
                rx_bytes: f[0],
                rx_errors: f[2],
                rx_drops: f[3],
                tx_bytes: f[8],
                tx_errors: f[10],
                tx_drops: f[11],
            },
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Disk
// ---------------------------------------------------------------------------

/// A single `/proc/mounts` entry: device, mount point, filesystem type.
#[derive(Debug, Clone, PartialEq)]
pub struct MountEntry {
    pub device: String,
    pub mount_point: String,
    pub fstype: String,
}

/// Pseudo filesystems that don't represent real disk space.
const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "tmpfs",
    "devtmpfs",
    "devpts",
    "cgroup",
    "cgroup2",
    "mqueue",
    "overlay",
    "squashfs",
    "debugfs",
    "tracefs",
    "securityfs",
    "pstore",
    "bpf",
    "configfs",
    "fusectl",
    "hugetlbfs",
    "autofs",
    "binfmt_misc",
    "ramfs",
    "nsfs",
    "rpc_pipefs",
];

/// Parses `/proc/mounts`, keeping only real (disk-backed) filesystems.
pub fn parse_mounts(content: &str) -> Vec<MountEntry> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for line in content.lines() {
        let mut it = line.split_whitespace();
        let device = it.next().unwrap_or("");
        let mount_point = it.next().unwrap_or("");
        let fstype = it.next().unwrap_or("");
        if fstype.is_empty() || PSEUDO_FS.contains(&fstype) {
            continue;
        }
        // Count each physical device only once.
        if !seen.insert(device.to_string()) {
            continue;
        }
        out.push(MountEntry {
            device: unescape_mount(device),
            mount_point: unescape_mount(mount_point),
            fstype: fstype.to_string(),
        });
    }
    out
}

/// In `/proc/mounts`, spaces/tabs are escaped as octal codes (`\040` etc.).
fn unescape_mount(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let Ok(code) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(code as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Completed read/write operation counts keyed by device name (`sda`, `nvme0n1`).
/// Parses `/proc/diskstats` (fields 4 and 8 are reads/writes completed).
pub fn parse_diskstats(content: &str) -> BTreeMap<String, (u64, u64)> {
    let mut out = BTreeMap::new();
    for line in content.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 {
            continue;
        }
        let name = f[2].to_string();
        let reads = f[3].parse::<u64>().unwrap_or(0);
        let writes = f[7].parse::<u64>().unwrap_or(0);
        out.insert(name, (reads, writes));
    }
    out
}

/// Maps a mount point to its device name in diskstats.
/// `/dev/sda1` → `sda1`; for mapper/paths, the last path component is used.
fn device_basename(device: &str) -> String {
    device.rsplit('/').next().unwrap_or(device).to_string()
}

/// statvfs result: (total, available to user, free) in bytes.
fn statvfs_bytes(path: &str) -> Option<(u64, u64)> {
    let c = CString::new(path).ok()?;
    // SAFETY: we pass a valid C-string path and a zeroed struct to write into.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    let bsize = stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * bsize;
    let avail = stat.f_bavail as u64 * bsize;
    Some((total, avail))
}

// ---------------------------------------------------------------------------
// Snapshot assembly
// ---------------------------------------------------------------------------

fn read_file(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// Reads a namespace-dependent procfs file (`mounts`, `net/dev`), preferring the
/// PID 1 view. When the host's `/proc` is mounted into the container, `{proc}/1/{rel}`
/// gives the host's mount/net namespace; otherwise fall back to `{proc}/{rel}` (container view).
fn read_proc_ns(proc: &Path, rel: &str) -> String {
    let pid1 = proc.join("1").join(rel);
    if let Ok(s) = read_file(&pid1) {
        if !s.trim().is_empty() {
            return s;
        }
    }
    read_file(&proc.join(rel)).unwrap_or_default()
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Takes a full snapshot of host metrics.
///
/// CPU usage is computed from a `/proc/stat` delta over a short sleep interval,
/// so the function is async and adds ~`SAMPLE_MS` ms of latency.
pub async fn collect_host_metrics(cfg: &Config) -> anyhow::Result<HostMetrics> {
    const SAMPLE_MS: u64 = 200;
    let proc = Path::new(&cfg.proc_path);

    // --- CPU: two /proc/stat snapshots with a pause in between ---
    let stat1 = read_file(&proc.join("stat"))?;
    let snap1 = parse_proc_stat(&stat1);
    tokio::time::sleep(std::time::Duration::from_millis(SAMPLE_MS)).await;
    let stat2 = read_file(&proc.join("stat"))?;
    let snap2 = parse_proc_stat(&stat2);

    let load_avg = parse_loadavg(&read_file(&proc.join("loadavg")).unwrap_or_default());
    let cpu = build_cpu(&snap1, &snap2, load_avg);

    // --- Memory ---
    let memory = parse_meminfo(&read_file(&proc.join("meminfo"))?);

    // --- Network (namespace-dependent: prefer the host's view via PID 1) ---
    let network = parse_net_dev(&read_proc_ns(proc, "net/dev"));

    // --- Disk (mounts are namespace-dependent too) ---
    let mounts = parse_mounts(&read_proc_ns(proc, "mounts"));
    let diskstats = parse_diskstats(&read_file(&proc.join("diskstats")).unwrap_or_default());
    let disk = build_disks(&mounts, &diskstats, &cfg.rootfs_path);

    // --- Hostname ---
    let host = read_file(&proc.join("sys/kernel/hostname"))
        .unwrap_or_default()
        .trim()
        .to_string();

    Ok(HostMetrics {
        timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        host,
        cpu,
        memory,
        disk,
        network,
    })
}

/// Builds [`CpuMetrics`] from two `/proc/stat` snapshots. Extracted for testability.
pub fn build_cpu(
    snap1: &BTreeMap<String, CpuTimes>,
    snap2: &BTreeMap<String, CpuTimes>,
    load_avg: [f64; 3],
) -> CpuMetrics {
    let usage_percent = match (snap1.get("cpu"), snap2.get("cpu")) {
        (Some(a), Some(b)) => cpu_usage_from_delta(*a, *b),
        _ => 0.0,
    };
    let mut per_core = Vec::new();
    let mut idx = 0;
    loop {
        let key = format!("cpu{idx}");
        match (snap1.get(&key), snap2.get(&key)) {
            (Some(a), Some(b)) => per_core.push(cpu_usage_from_delta(*a, *b)),
            _ => break,
        }
        idx += 1;
    }
    CpuMetrics {
        load_avg,
        usage_percent,
        cores: per_core.len(),
        per_core_usage_percent: per_core,
    }
}

/// Builds the list of [`DiskMetrics`], calling statvfs under the rootfs prefix.
pub fn build_disks(
    mounts: &[MountEntry],
    diskstats: &BTreeMap<String, (u64, u64)>,
    rootfs: &str,
) -> Vec<DiskMetrics> {
    let mut out = Vec::new();
    for m in mounts {
        // Read the host mount point through the mounted rootfs prefix.
        let probe = join_rootfs(rootfs, &m.mount_point);
        let (total, avail) = match statvfs_bytes(&probe) {
            Some(v) => v,
            None => continue,
        };
        if total == 0 {
            continue;
        }
        let used = total.saturating_sub(avail);
        let (io_read_ops, io_write_ops) = diskstats
            .get(&device_basename(&m.device))
            .copied()
            .unwrap_or((0, 0));
        out.push(DiskMetrics {
            mount: m.mount_point.clone(),
            device: m.device.clone(),
            fstype: m.fstype.clone(),
            total_bytes: total,
            used_bytes: used,
            available_bytes: avail,
            usage_percent: round2(used as f64 / total as f64 * 100.0),
            io_read_ops,
            io_write_ops,
        });
    }
    out
}

/// Joins the rootfs prefix and mount point without doubling `/`.
fn join_rootfs(rootfs: &str, mount_point: &str) -> String {
    if rootfs == "/" || rootfs.is_empty() {
        return mount_point.to_string();
    }
    let base = rootfs.trim_end_matches('/');
    if mount_point == "/" {
        return base.to_string();
    }
    format!("{base}{mount_point}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loadavg_parses_three_values() {
        assert_eq!(
            parse_loadavg("1.20 0.80 0.50 1/234 5678"),
            [1.20, 0.80, 0.50]
        );
        assert_eq!(parse_loadavg(""), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn proc_stat_aggregate_and_cores() {
        let s = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 50 0 50 400 0 0 0 0 0 0\ncpu1 50 0 50 400 0 0 0 0 0 0\nintr 12345\n";
        let m = parse_proc_stat(s);
        assert_eq!(m.len(), 3);
        let agg = m.get("cpu").unwrap();
        assert_eq!(agg.total, 1000);
        assert_eq!(agg.idle, 800); // idle + iowait
    }

    #[test]
    fn cpu_usage_delta_is_percent() {
        // Over the interval total grew by 100, idle by 20 → 80% usage.
        let prev = CpuTimes {
            total: 1000,
            idle: 800,
        };
        let cur = CpuTimes {
            total: 1100,
            idle: 820,
        };
        assert_eq!(cpu_usage_from_delta(prev, cur), 80.0);
    }

    #[test]
    fn cpu_usage_zero_delta_safe() {
        let t = CpuTimes {
            total: 500,
            idle: 400,
        };
        assert_eq!(cpu_usage_from_delta(t, t), 0.0);
    }

    #[test]
    fn build_cpu_uses_per_core() {
        let s1 = parse_proc_stat(
            "cpu 1000 0 0 1000 0 0 0 0\ncpu0 500 0 0 500 0 0 0 0\ncpu1 500 0 0 500 0 0 0 0\n",
        );
        let s2 = parse_proc_stat(
            "cpu 1100 0 0 1080 0 0 0 0\ncpu0 550 0 0 540 0 0 0 0\ncpu1 550 0 0 540 0 0 0 0\n",
        );
        let cpu = build_cpu(&s1, &s2, [1.0, 2.0, 3.0]);
        assert_eq!(cpu.cores, 2);
        assert_eq!(cpu.load_avg, [1.0, 2.0, 3.0]);
        // agg: total_d = (1100+1080)-(1000+1000) = 180, idle_d = 1080-1000 = 80
        // usage = 1 - 80/180 = 55.56%
        assert_eq!(cpu.usage_percent, 55.56);
        assert_eq!(cpu.per_core_usage_percent, vec![55.56, 55.56]);
    }

    #[test]
    fn meminfo_computes_used_and_percent() {
        let s = "MemTotal:       1000 kB\nMemFree:         200 kB\nMemAvailable:    400 kB\nBuffers:          50 kB\nCached:          100 kB\nSwapTotal:       500 kB\nSwapFree:        300 kB\n";
        let m = parse_meminfo(s);
        assert_eq!(m.total_bytes, 1000 * 1024);
        assert_eq!(m.available_bytes, 400 * 1024);
        assert_eq!(m.used_bytes, 600 * 1024); // total - available
        assert_eq!(m.free_bytes, 200 * 1024);
        assert_eq!(m.swap_used_bytes, 200 * 1024); // 500 - 300
        assert_eq!(m.usage_percent, 60.0);
    }

    #[test]
    fn meminfo_falls_back_without_available() {
        let s = "MemTotal: 1000 kB\nMemFree: 200 kB\nBuffers: 50 kB\nCached: 100 kB\n";
        let m = parse_meminfo(s);
        // available ≈ free + buffers + cached = 350
        assert_eq!(m.available_bytes, 350 * 1024);
    }

    #[test]
    fn net_dev_skips_lo_and_headers() {
        let s = "Inter-|   Receive                                                |  Transmit\n \
                 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
                 lo: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0\n  \
                 eth0: 5000 50 1 2 0 0 0 0 8000 80 3 4 0 0 0 0\n";
        let m = parse_net_dev(s);
        assert!(!m.contains_key("lo"));
        let e = m.get("eth0").unwrap();
        assert_eq!(e.rx_bytes, 5000);
        assert_eq!(e.rx_errors, 1);
        assert_eq!(e.rx_drops, 2);
        assert_eq!(e.tx_bytes, 8000);
        assert_eq!(e.tx_errors, 3);
        assert_eq!(e.tx_drops, 4);
    }

    #[test]
    fn mounts_filters_pseudo_and_dedups() {
        let s = "proc /proc proc rw 0 0\n\
                 /dev/sda1 / ext4 rw 0 0\n\
                 tmpfs /run tmpfs rw 0 0\n\
                 /dev/sda1 /mnt/bind ext4 rw 0 0\n\
                 /dev/sdb1 /data xfs rw 0 0\n";
        let m = parse_mounts(s);
        assert_eq!(m.len(), 2); // sda1 (dedup) + sdb1
        assert_eq!(m[0].device, "/dev/sda1");
        assert_eq!(m[0].mount_point, "/");
        assert_eq!(m[1].fstype, "xfs");
    }

    #[test]
    fn mounts_unescape_spaces() {
        let s = "/dev/sda1 /mnt/my\\040disk ext4 rw 0 0\n";
        let m = parse_mounts(s);
        assert_eq!(m[0].mount_point, "/mnt/my disk");
    }

    #[test]
    fn diskstats_reads_and_writes() {
        let s = "   8       0 sda 100 0 0 0 200 0 0 0 0 0 0\n   8       1 sda1 10 0 0 0 20 0 0 0 0 0 0\n";
        let m = parse_diskstats(s);
        assert_eq!(m.get("sda"), Some(&(100, 200)));
        assert_eq!(m.get("sda1"), Some(&(10, 20)));
    }

    #[test]
    fn device_basename_strips_path() {
        assert_eq!(device_basename("/dev/sda1"), "sda1");
        assert_eq!(device_basename("/dev/mapper/vg-root"), "vg-root");
        assert_eq!(device_basename("sda"), "sda");
    }

    #[test]
    fn join_rootfs_variants() {
        assert_eq!(join_rootfs("/", "/data"), "/data");
        assert_eq!(join_rootfs("/host/rootfs", "/"), "/host/rootfs");
        assert_eq!(join_rootfs("/host/rootfs", "/data"), "/host/rootfs/data");
        assert_eq!(join_rootfs("/host/rootfs/", "/data"), "/host/rootfs/data");
    }
}
