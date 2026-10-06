use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Default path to the kubeconfig on an SSH-managed host
pub const DEFAULT_REMOTE_KUBECONFIG: &str = "~/.kube/config";

/// Expand a leading `~/` to the current user's home directory
pub fn expand_tilde(path: &str) -> Result<PathBuf> {
    if let Some(stripped) = path.strip_prefix("~/") {
        let home = dirs::home_dir().context("Failed to determine home directory")?;
        Ok(home.join(stripped))
    } else if path.starts_with('~') {
        Err(anyhow::anyhow!(
            "Paths like ~user are not supported, use full path or ~/"
        ))
    } else {
        Ok(PathBuf::from(path))
    }
}

/// How a cluster's kubeconfig is obtained
#[derive(Debug, Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClusterProvider {
    /// Copy the kubeconfig off a remote host with scp
    #[default]
    Ssh,
    /// Ask a Talos Linux control plane node for one with talosctl
    Talos,
}

impl ClusterProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClusterProvider::Ssh => "ssh",
            ClusterProvider::Talos => "talos",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClusterConfig {
    pub name: String,
    #[serde(default)]
    pub provider: ClusterProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>, // Deprecated: kept for backward compatibility
    #[serde(default)]
    pub hostnames: Vec<String>, // Multiple SSH/Talos endpoints for HA
    #[serde(default)]
    pub discovered_node_ips: Vec<String>, // Auto-discovered node IPs from Kubernetes API
    pub ssh_port: Option<u16>,
    pub username: Option<String>,
    pub password_encrypted: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubeconfig_path: Option<String>, // Path on remote host (SSH only), e.g., ~/.kube/config
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub talosconfig_path: Option<String>, // Local talosconfig, defaults to talosctl's own lookup
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub talos_context: Option<String>, // Context to select inside the talosconfig
    pub description: Option<String>,
    pub added_at: String,
    pub last_synced: Option<String>, // Last time kubeconfig was synced
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_working_hostname: Option<String>, // Cache last successful endpoint
}

impl ClusterConfig {
    pub fn new(
        name: String,
        hostnames: Vec<String>,
        ssh_port: Option<u16>,
        username: Option<String>,
        password_encrypted: Option<String>,
        kubeconfig_path: String,
        description: Option<String>,
    ) -> Self {
        Self {
            name,
            provider: ClusterProvider::Ssh,
            hostname: None, // Deprecated field
            hostnames,
            discovered_node_ips: Vec::new(),
            ssh_port,
            username,
            password_encrypted,
            kubeconfig_path: Some(kubeconfig_path),
            talosconfig_path: None,
            talos_context: None,
            description,
            added_at: chrono::Utc::now().to_rfc3339(),
            last_synced: None,
            last_working_hostname: None,
        }
    }

    /// Create a Talos cluster, reached with talosctl instead of SSH.
    ///
    /// `endpoints` may be empty, in which case talosctl falls back to the
    /// endpoints defined in the talosconfig itself.
    pub fn new_talos(
        name: String,
        endpoints: Vec<String>,
        talosconfig_path: Option<String>,
        talos_context: Option<String>,
        description: Option<String>,
    ) -> Self {
        Self {
            name,
            provider: ClusterProvider::Talos,
            hostname: None,
            hostnames: endpoints,
            discovered_node_ips: Vec::new(),
            ssh_port: None,
            username: None,
            password_encrypted: None,
            kubeconfig_path: None,
            talosconfig_path,
            talos_context,
            description,
            added_at: chrono::Utc::now().to_rfc3339(),
            last_synced: None,
            last_working_hostname: None,
        }
    }

    pub fn is_talos(&self) -> bool {
        self.provider == ClusterProvider::Talos
    }

    /// Path to the kubeconfig on the remote host (SSH clusters only)
    pub fn remote_kubeconfig_path(&self) -> &str {
        self.kubeconfig_path
            .as_deref()
            .unwrap_or(DEFAULT_REMOTE_KUBECONFIG)
    }

    /// Get all hostnames for this cluster (handles backward compatibility)
    pub fn get_hostnames(&self) -> Vec<String> {
        if !self.hostnames.is_empty() {
            self.hostnames.clone()
        } else if let Some(ref hostname) = self.hostname {
            // Backward compatibility: convert old hostname field to vec
            vec![hostname.clone()]
        } else {
            vec![]
        }
    }

    /// Get all available endpoints (configured + discovered), prioritized
    pub fn get_prioritized_hostnames(&self) -> Vec<String> {
        let mut hostnames = self.get_hostnames();

        // Add discovered node IPs that aren't already in the configured list
        for discovered_ip in &self.discovered_node_ips {
            if !hostnames.contains(discovered_ip) {
                hostnames.push(discovered_ip.clone());
            }
        }

        // If we have a last working hostname, move it to the front
        if let Some(ref last_working) = self.last_working_hostname
            && let Some(pos) = hostnames.iter().position(|h| h == last_working)
        {
            hostnames.remove(pos);
            hostnames.insert(0, last_working.clone());
        }

        hostnames
    }

    /// Update the last working hostname after a successful connection
    pub fn update_last_working_hostname(&mut self, hostname: String) {
        self.last_working_hostname = Some(hostname);
    }

    /// Update discovered node IPs from Kubernetes API
    pub fn update_discovered_node_ips(&mut self, ips: Vec<String>) {
        self.discovered_node_ips = ips;
    }

    /// Get the local cached kubeconfig path for this cluster
    pub fn local_kubeconfig_path(&self) -> Result<PathBuf> {
        let home_dir = dirs::home_dir().context("Failed to determine home directory")?;
        let kubeconfig_dir = home_dir.join(".kube").join("heimdall");
        Ok(kubeconfig_dir.join(format!("{}.yaml", self.name)))
    }

    /// Check if a cached kubeconfig exists locally
    pub fn has_cached_kubeconfig(&self) -> bool {
        self.local_kubeconfig_path()
            .map(|path| path.exists())
            .unwrap_or(false)
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    pub clusters: HashMap<String, ClusterConfig>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path()?;

        if !config_path.exists() {
            return Ok(Config::default());
        }

        let content = fs::read_to_string(&config_path).context("Failed to read config file")?;

        let config: Config =
            serde_yaml::from_str(&content).context("Failed to parse config file")?;

        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path()?;

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent).context("Failed to create config directory")?;
        }

        let content = serde_yaml::to_string(self).context("Failed to serialize config")?;

        fs::write(&config_path, content).context("Failed to write config file")?;

        Ok(())
    }

    pub fn add_cluster(&mut self, cluster: ClusterConfig) {
        self.clusters.insert(cluster.name.clone(), cluster);
    }

    pub fn remove_cluster(&mut self, name: &str) -> Option<ClusterConfig> {
        self.clusters.remove(name)
    }

    pub fn get_cluster(&self, name: &str) -> Option<&ClusterConfig> {
        self.clusters.get(name)
    }

    pub fn list_clusters(&self) -> Vec<&ClusterConfig> {
        let mut clusters: Vec<&ClusterConfig> = self.clusters.values().collect();
        clusters.sort_by(|a, b| a.name.cmp(&b.name));
        clusters
    }

    fn config_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir().context("Failed to determine config directory")?;
        Ok(config_dir.join("heimdall").join("config.yaml"))
    }
}
