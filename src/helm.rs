use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HelmRelease {
    pub name: String,
    pub namespace: String,
    pub revision: String,
    pub updated: String,
    pub status: String,
    pub chart: String,
    pub app_version: String,
}

pub fn fetch_helm_releases(kubeconfig_path: &str) -> Result<Vec<HelmRelease>> {
    let output = Command::new("helm")
        .args(["list", "--all-namespaces", "--output", "json"])
        .env("KUBECONFIG", kubeconfig_path)
        .output()
        .context("Failed to execute helm command. Is helm installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Helm command failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let releases: Vec<HelmRelease> =
        serde_json::from_str(&stdout).context("Failed to parse helm output")?;

    Ok(releases)
}
