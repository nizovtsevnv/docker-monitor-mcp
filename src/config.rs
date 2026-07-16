//! Service configuration from environment variables.
//!
//! All procfs/sysfs/rootfs paths are configurable so that:
//!   * inside a container we read the mounted host paths (`/host/proc`, ...);
//!   * in tests and local runs we read the real `/proc`, `/sys`, `/`.

use std::env;

/// Parsed process configuration. Immutable after startup (the service is stateless).
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the MCP HTTP endpoint listens on.
    pub bind_addr: String,
    /// Path to the docker socket (mounted read-only).
    pub docker_socket: String,
    /// procfs root (default `/proc`; in a container — `/host/proc`).
    pub proc_path: String,
    /// Host filesystem root for disk statvfs
    /// (default `/`; in a container — `/host/rootfs`).
    pub rootfs_path: String,
    /// Optional bearer token. `None` (env unset or empty) disables auth.
    /// When set, MCP endpoints require `Authorization: Bearer <token>`.
    pub auth_token: Option<String>,
}

impl Config {
    /// Builds the config from the environment, applying defaults.
    pub fn from_env() -> Self {
        Config {
            bind_addr: env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            docker_socket: env::var("DOCKER_SOCKET")
                .unwrap_or_else(|_| "/var/run/docker.sock".into()),
            proc_path: env::var("HOST_PROC").unwrap_or_else(|_| "/proc".into()),
            rootfs_path: env::var("HOST_ROOTFS").unwrap_or_else(|_| "/".into()),
            auth_token: env::var("AUTH_TOKEN").ok().filter(|t| !t.is_empty()),
        }
    }
}
