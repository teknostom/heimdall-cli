use anyhow::{Result, anyhow};
use std::io::ErrorKind;
use std::process::Command;

use crate::config::{ClusterConfig, expand_tilde};

/// Talos version reported by a single node
#[derive(Debug, Clone)]
pub struct NodeVersion {
    pub node: String,
    pub version: String,
}

/// Machine status of a single Talos node
#[derive(Debug, Clone)]
pub struct NodeStatus {
    pub node: String,
    pub stage: String,
    pub ready: bool,
}

/// Fetch the admin kubeconfig from the cluster, along with the node that served it.
///
/// `--merge=false` with a local-path of `-` makes talosctl write the kubeconfig
/// to stdout. Every endpoint is handed to talosctl at once so its own client
/// does the endpoint failover, but the *node* has to be pinned: talosctl refuses
/// this command outright when it resolves to more than one node ("requires
/// exactly one node"), which is the normal case for an HA context. So when the
/// context lists several nodes we try them in turn.
pub fn kubeconfig(cluster: &ClusterConfig, nodes: &[String]) -> Result<(String, Option<String>)> {
    if nodes.len() < 2 {
        // Zero or one node: talosctl already resolves to a single target
        let stdout = fetch_kubeconfig_from(cluster, None)?;
        return Ok((stdout, nodes.first().cloned()));
    }

    let mut errors = Vec::new();

    for node in nodes {
        match fetch_kubeconfig_from(cluster, Some(node)) {
            Ok(stdout) => return Ok((stdout, Some(node.clone()))),
            Err(e) => errors.push(format!("{}: {}", node, e)),
        }
    }

    Err(anyhow!(
        "Failed to fetch kubeconfig from all Talos nodes:\n{}",
        errors.join("\n")
    ))
}

fn fetch_kubeconfig_from(cluster: &ClusterConfig, node: Option<&str>) -> Result<String> {
    let stdout = run(cluster, &["kubeconfig", "-", "--merge=false"], node)?;
    validate_kubeconfig(&stdout)?;
    Ok(stdout)
}

/// What the talosconfig context targets
#[derive(Debug, Default)]
pub struct ContextInfo {
    pub nodes: Vec<String>,
    pub endpoints: Vec<String>,
}

/// Read the context's nodes and endpoints. talosctl resolves this locally, so
/// it costs no round trip to the cluster.
pub fn context_info(cluster: &ClusterConfig) -> ContextInfo {
    let Ok(stdout) = run(cluster, &["config", "info", "-o", "json"], None) else {
        return ContextInfo::default();
    };

    let Ok(info) = serde_json::from_str::<serde_json::Value>(&stdout) else {
        return ContextInfo::default();
    };

    ContextInfo {
        nodes: string_list(&info["nodes"]),
        endpoints: string_list(&info["endpoints"]),
    }
}

fn string_list(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Talos version of each node in the context (best-effort, needs a reachable node)
pub fn node_versions(cluster: &ClusterConfig) -> Result<Vec<NodeVersion>> {
    let stdout = run(cluster, &["version"], None)?;
    Ok(parse_versions(&stdout))
}

/// Machine status (boot stage, readiness) of each node in the context
pub fn machine_status(cluster: &ClusterConfig) -> Result<Vec<NodeStatus>> {
    let stdout = run(cluster, &["get", "machinestatus", "-o", "json"], None)?;
    Ok(parse_machine_status(&stdout))
}

/// Condense per-node versions into one cell, flagging drift the way
/// `heimdall releases` flags differing chart versions
pub fn summarize_versions(versions: &[NodeVersion]) -> Option<String> {
    let first = versions.first()?;

    if versions.iter().any(|v| v.version != first.version) {
        Some(format!("{} ⚠", first.version))
    } else {
        Some(first.version.clone())
    }
}

fn run(cluster: &ClusterConfig, args: &[&str], node: Option<&str>) -> Result<String> {
    let mut command = Command::new("talosctl");
    command.args(args);

    if let Some(ref path) = cluster.talosconfig_path {
        let expanded = expand_tilde(path)?;
        command.arg("--talosconfig").arg(expanded);
    }

    if let Some(ref context) = cluster.talos_context {
        command.args(["--context", context]);
    }

    // Only what was configured explicitly: a Talos API endpoint and a Kubernetes
    // node IP are often on different networks, so auto-discovered node IPs must
    // not end up here. No endpoints at all means talosctl uses the talosconfig's.
    let endpoints = cluster.get_hostnames();
    if !endpoints.is_empty() {
        command.args(["--endpoints", &endpoints.join(",")]);
    }

    // Left unset, the nodes from the talosconfig context apply
    if let Some(node) = node {
        command.args(["--nodes", node]);
    }

    let output = command.output().map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            anyhow!("talosctl not found in PATH. Install talosctl to manage Talos clusters.")
        } else {
            anyhow!("Failed to execute talosctl: {}", e)
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "talosctl {} failed: {}",
            args[0],
            stderr.trim().lines().next().unwrap_or("unknown error")
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Guard against a talosconfig context that targets several nodes, which makes
/// talosctl emit one kubeconfig per node instead of a single usable document
fn validate_kubeconfig(content: &str) -> Result<()> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(content).map_err(|e| {
        anyhow!(
            "talosctl did not return a single usable kubeconfig ({}). \
             If the talos context targets several nodes, pick one with --talos-context \
             or set explicit endpoints on the cluster.",
            e
        )
    })?;

    if parsed.get("clusters").is_none() {
        return Err(anyhow!(
            "talosctl output does not look like a kubeconfig (no 'clusters' key)"
        ));
    }

    Ok(())
}

/// Parse the `Server:` section of `talosctl version`, which has no JSON output.
/// Each node contributes a `NODE:` line followed by a `Tag:` line.
fn parse_versions(stdout: &str) -> Vec<NodeVersion> {
    let mut versions = Vec::new();
    let mut in_server_section = false;
    let mut node: Option<String> = None;

    for line in stdout.lines() {
        let line = line.trim();

        if line == "Server:" {
            in_server_section = true;
            continue;
        } else if line == "Client:" {
            in_server_section = false;
            continue;
        }

        if !in_server_section {
            continue;
        }

        if let Some(value) = line.strip_prefix("NODE:") {
            node = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Tag:") {
            versions.push(NodeVersion {
                node: node.take().unwrap_or_else(|| "-".to_string()),
                version: value.trim().to_string(),
            });
        }
    }

    versions
}

/// Parse `talosctl get -o json`, which streams one object per node rather than
/// a single JSON array
fn parse_machine_status(stdout: &str) -> Vec<NodeStatus> {
    serde_json::Deserializer::from_str(stdout)
        .into_iter::<serde_json::Value>()
        .filter_map(|value| value.ok())
        .map(|value| NodeStatus {
            node: value["node"]
                .as_str()
                .or_else(|| value["metadata"]["node"].as_str())
                .unwrap_or("-")
                .to_string(),
            stage: value["spec"]["stage"].as_str().unwrap_or("-").to_string(),
            ready: value["spec"]["status"]["ready"].as_bool().unwrap_or(false),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_server_versions_and_ignores_the_client_section() {
        let output = "Client:\n\tTag:         v1.13.0\n\tSHA:         abc123\n\nServer:\n\tNODE:        10.0.0.11\n\tTag:         v1.14.0\n\tSHA:         def456\n\tNODE:        10.0.0.12\n\tTag:         v1.14.1\n\tSHA:         def456\n";

        let versions = parse_versions(output);

        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].node, "10.0.0.11");
        assert_eq!(versions[0].version, "v1.14.0");
        assert_eq!(versions[1].node, "10.0.0.12");
        assert_eq!(versions[1].version, "v1.14.1");
        assert_eq!(summarize_versions(&versions).unwrap(), "v1.14.0 ⚠");
    }

    #[test]
    fn summarizes_matching_versions_without_a_drift_marker() {
        let versions = vec![
            NodeVersion {
                node: "10.0.0.11".to_string(),
                version: "v1.14.0".to_string(),
            },
            NodeVersion {
                node: "10.0.0.12".to_string(),
                version: "v1.14.0".to_string(),
            },
        ];

        assert_eq!(summarize_versions(&versions).unwrap(), "v1.14.0");
        assert!(summarize_versions(&[]).is_none());
    }

    #[test]
    fn parses_concatenated_machine_status_objects() {
        let output = r#"{"node":"10.0.0.11","spec":{"stage":"running","status":{"ready":true}}}
{"metadata":{"node":"10.0.0.12"},"spec":{"stage":"booting","status":{"ready":false}}}"#;

        let statuses = parse_machine_status(output);

        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].node, "10.0.0.11");
        assert_eq!(statuses[0].stage, "running");
        assert!(statuses[0].ready);
        assert_eq!(statuses[1].node, "10.0.0.12");
        assert_eq!(statuses[1].stage, "booting");
        assert!(!statuses[1].ready);
    }

    #[test]
    fn rejects_output_that_is_not_a_single_kubeconfig() {
        assert!(validate_kubeconfig("apiVersion: v1\nkind: Config\nclusters: []\n").is_ok());
        assert!(validate_kubeconfig("apiVersion: v1\nkind: Config\n").is_err());
        assert!(validate_kubeconfig("clusters: []\n---\nclusters: []\n").is_err());
    }
}
