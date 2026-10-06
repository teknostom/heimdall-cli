use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::config::{
    ClusterConfig, ClusterProvider, Config, DEFAULT_REMOTE_KUBECONFIG, expand_tilde,
};
use crate::helm::HelmRelease;

// Global cache for master password during session
use std::sync::Mutex;

static MASTER_PASSWORD: Mutex<Option<String>> = Mutex::new(None);

pub fn list() -> Result<()> {
    let config = Config::load()?;
    let clusters = config.list_clusters();

    if clusters.is_empty() {
        println!("No clusters configured.");
        println!("\nUse 'heimdall add <name> --hostname <host>' to add a cluster.");
        return Ok(());
    }

    // Get terminal width, default to 80 if unavailable
    let term_width = terminal_size::terminal_size()
        .map(|(terminal_size::Width(w), _)| w as usize)
        .unwrap_or(80);

    println!("Configured Kubernetes clusters:\n");
    println!("{:-<width$}", "", width = term_width);
    println!(
        "{:<20} {:<7} {:<28} {:<12} {:<6} {:<7} DESCRIPTION",
        "NAME", "TYPE", "HOSTNAME", "USERNAME", "PORT", "SYNCED"
    );
    println!("{:-<width$}", "", width = term_width);

    for cluster in clusters {
        let description = cluster.description.as_deref().unwrap_or("-");

        // SSH credentials are meaningless for providers that do not use SSH
        let username = if cluster.is_talos() {
            "-"
        } else {
            cluster.username.as_deref().unwrap_or("-")
        };
        let port = if cluster.is_talos() {
            "-".to_string()
        } else {
            cluster
                .ssh_port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "22".to_string())
        };
        let synced = if cluster.has_cached_kubeconfig() {
            "✓"
        } else {
            "-"
        };

        // Display hostnames - show first one, or count if multiple
        let hostnames = cluster.get_hostnames();
        let hostname_display = if hostnames.is_empty() {
            "-".to_string()
        } else if hostnames.len() == 1 {
            hostnames[0].clone()
        } else {
            format!("{} (+{})", hostnames[0], hostnames.len() - 1)
        };

        println!(
            "{:<20} {:<7} {:<28} {:<12} {:<6} {:<7} {}",
            cluster.name,
            cluster.provider.as_str(),
            hostname_display,
            username,
            port,
            synced,
            description
        );
    }

    println!("{:-<width$}", "", width = term_width);
    println!("\nTotal: {} cluster(s)", config.clusters.len());
    println!("✓ = Kubeconfig cached locally");

    Ok(())
}

pub fn get(name: &str, output: Option<&str>) -> Result<()> {
    let mut config = Config::load()?;

    let cluster = config
        .get_cluster(name)
        .ok_or_else(|| anyhow!("Cluster '{}' not found", name))?
        .clone();

    // Fetch kubeconfig via SCP
    let (kubeconfig, working_hostname, discovered_ips) = fetch_kubeconfig(&cluster)?;

    // Update last working hostname and discovered IPs if we got them
    if let Some(cluster_mut) = config.clusters.get_mut(name) {
        if let Some(hostname) = working_hostname {
            cluster_mut.update_last_working_hostname(hostname);
        }
        if !discovered_ips.is_empty() {
            cluster_mut.update_discovered_node_ips(discovered_ips);
        }
        config.save()?;
    }

    if let Some(output_path) = output {
        fs::write(output_path, &kubeconfig).context("Failed to write output file")?;
        println!("Kubeconfig written to: {}", output_path);
    } else {
        println!("{}", kubeconfig);
    }

    Ok(())
}

pub struct AddOptions<'a> {
    pub name: &'a str,
    pub hostname: Option<&'a str>,
    pub ssh_port: Option<u16>,
    pub username: Option<&'a str>,
    pub prompt_password: bool,
    pub kubeconfig_path: Option<&'a str>,
    pub description: Option<&'a str>,
    pub talos: bool,
    pub talosconfig_path: Option<&'a str>,
    pub talos_context: Option<&'a str>,
}

pub fn add(options: AddOptions<'_>) -> Result<()> {
    let mut config = Config::load()?;
    let name = options.name;

    if config.get_cluster(name).is_some() {
        return Err(anyhow!(
            "Cluster '{}' already exists. Use 'heimdall rm {}' first to replace it.",
            name,
            name
        ));
    }

    // Parse hostnames - support comma-separated list
    let hostnames: Vec<String> = options
        .hostname
        .unwrap_or_default()
        .split(',')
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .collect();

    let cluster = if options.talos {
        add_talos_cluster(&options, hostnames.clone())?
    } else {
        add_ssh_cluster(&options, hostnames.clone())?
    };

    config.add_cluster(cluster);
    config.save()?;

    if hostnames.len() > 1 {
        println!(
            "Cluster '{}' added successfully with {} endpoints for high availability.",
            name,
            hostnames.len()
        );
        println!("Endpoints: {}", hostnames.join(", "));
    } else {
        println!("Cluster '{}' added successfully.", name);
    }

    Ok(())
}

fn add_ssh_cluster(options: &AddOptions<'_>, hostnames: Vec<String>) -> Result<ClusterConfig> {
    if hostnames.is_empty() {
        return Err(anyhow!("At least one hostname must be provided"));
    }

    let password_encrypted = if options.prompt_password {
        let ssh_password = rpassword::prompt_password("Enter SSH password: ")
            .context("Failed to read SSH password")?;

        let master_password =
            rpassword::prompt_password("Enter master password (for encryption): ")
                .context("Failed to read master password")?;

        let encrypted = crate::crypto::encrypt_password(&ssh_password, &master_password)
            .context("Failed to encrypt password")?;

        Some(encrypted)
    } else {
        None
    };

    Ok(ClusterConfig::new(
        options.name.to_string(),
        hostnames,
        options.ssh_port,
        options.username.map(String::from),
        password_encrypted,
        options
            .kubeconfig_path
            .unwrap_or(DEFAULT_REMOTE_KUBECONFIG)
            .to_string(),
        options.description.map(String::from),
    ))
}

/// Talos nodes have no SSH: the kubeconfig comes from talosctl, authenticated by
/// the client certificate in the talosconfig, so there is no credential to store.
fn add_talos_cluster(options: &AddOptions<'_>, endpoints: Vec<String>) -> Result<ClusterConfig> {
    warn_about_talos_prerequisites(options, &endpoints);

    Ok(ClusterConfig::new_talos(
        options.name.to_string(),
        endpoints,
        options.talosconfig_path.map(String::from),
        options.talos_context.map(String::from),
        options.description.map(String::from),
    ))
}

/// Point out a setup that will not work later, without refusing the add - the
/// cluster may simply not be reachable from here yet.
fn warn_about_talos_prerequisites(options: &AddOptions<'_>, endpoints: &[String]) {
    if Command::new("talosctl")
        .arg("version")
        .arg("--client")
        .output()
        .is_err()
    {
        println!("Warning: talosctl was not found in PATH. Install it to use this cluster.");
    }

    if let Some(path) = options.talosconfig_path {
        match expand_tilde(path) {
            Ok(expanded) if !expanded.exists() => {
                println!("Warning: talosconfig '{}' does not exist yet.", path);
            }
            Err(e) => println!("Warning: talosconfig path '{}': {}", path, e),
            _ => {}
        }
    }

    if endpoints.is_empty() {
        println!("No endpoints given: falling back to the ones in the talosconfig.");
    }
}

pub fn remove(name: &str) -> Result<()> {
    let mut config = Config::load()?;

    match config.remove_cluster(name) {
        Some(_) => {
            config.save()?;
            println!("Cluster '{}' removed successfully.", name);
            Ok(())
        }
        None => Err(anyhow!("Cluster '{}' not found", name)),
    }
}

pub fn info(name: &str) -> Result<()> {
    let config = Config::load()?;

    let cluster = config
        .get_cluster(name)
        .ok_or_else(|| anyhow!("Cluster '{}' not found", name))?;

    let hostnames = cluster.get_hostnames();

    let endpoint_label = if cluster.is_talos() {
        "Endpoint"
    } else {
        "Hostname"
    };

    println!("Cluster Information:");
    println!("{:-<60}", "");
    println!("Name:           {}", cluster.name);
    println!("Type:           {}", cluster.provider.as_str());

    if hostnames.len() > 1 {
        println!(
            "{:<16}{} (HA: {} endpoints)",
            format!("{}s:", endpoint_label),
            hostnames.join(", "),
            hostnames.len()
        );
        if let Some(ref last_working) = cluster.last_working_hostname {
            println!("Last Working:   {}", last_working);
        }
    } else if !hostnames.is_empty() {
        println!("{:<16}{}", format!("{}:", endpoint_label), hostnames[0]);
    } else if cluster.is_talos() {
        println!("Endpoints:      (from talosconfig)");
    }

    // Show discovered node IPs if any
    if !cluster.discovered_node_ips.is_empty() {
        println!(
            "Discovered:     {} node IP(s): {}",
            cluster.discovered_node_ips.len(),
            cluster.discovered_node_ips.join(", ")
        );
    }

    if cluster.is_talos() {
        println!(
            "Talosconfig:    {}",
            cluster.talosconfig_path.as_deref().unwrap_or("(default)")
        );
        println!(
            "Talos Context:  {}",
            cluster.talos_context.as_deref().unwrap_or("(current)")
        );
    } else {
        println!("SSH Port:       {}", cluster.ssh_port.unwrap_or(22));
        println!(
            "SSH Username:   {}",
            cluster.username.as_deref().unwrap_or("-")
        );
        println!(
            "SSH Password:   {}",
            if cluster.password_encrypted.is_some() {
                "****** (encrypted)"
            } else {
                "Not set"
            }
        );
        println!("Kubeconfig:     {}", cluster.remote_kubeconfig_path());
    }

    println!(
        "Description:    {}",
        cluster.description.as_deref().unwrap_or("-")
    );
    println!("Added at:       {}", cluster.added_at);
    println!("{:-<60}", "");

    if cluster.is_talos() {
        display_talos_nodes(cluster);
    }

    Ok(())
}

/// Show what the Talos machine API reports about each node. Best-effort: an
/// unreachable cluster just means no section, since `info` is offline-usable.
fn display_talos_nodes(cluster: &ClusterConfig) {
    let versions = crate::talos::node_versions(cluster).unwrap_or_default();
    let statuses = crate::talos::machine_status(cluster).unwrap_or_default();

    if versions.is_empty() && statuses.is_empty() {
        return;
    }

    println!("\nTalos Nodes:");
    println!("{:-<60}", "");
    println!(
        "{:<20} {:<12} {:<12} {:<10}",
        "NODE", "VERSION", "STAGE", "READY"
    );

    // Nodes may report a version, a status, or both
    let mut nodes: Vec<&str> = versions.iter().map(|v| v.node.as_str()).collect();
    for status in &statuses {
        if !nodes.contains(&status.node.as_str()) {
            nodes.push(&status.node);
        }
    }

    for node in nodes {
        let version = versions
            .iter()
            .find(|v| v.node == node)
            .map(|v| v.version.as_str())
            .unwrap_or("-");
        let status = statuses.iter().find(|s| s.node == node);

        println!(
            "{:<20} {:<12} {:<12} {:<10}",
            node,
            version,
            status.map(|s| s.stage.as_str()).unwrap_or("-"),
            match status {
                Some(s) if s.ready => "✓",
                Some(_) => "✗",
                None => "-",
            }
        );
    }

    println!("{:-<60}", "");
}

pub fn releases(cluster_filter: Option<&str>) -> Result<()> {
    let config = Config::load()?;

    let clusters_to_check: Vec<&ClusterConfig> = match cluster_filter {
        Some(name) => {
            let cluster = config
                .get_cluster(name)
                .ok_or_else(|| anyhow!("Cluster '{}' not found", name))?;
            vec![cluster]
        }
        None => config.list_clusters(),
    };

    if clusters_to_check.is_empty() {
        println!("No clusters configured. Use 'heimdall add' to add a cluster.");
        return Ok(());
    }

    println!("Fetching Helm releases from clusters...\n");

    let mut all_releases: HashMap<String, Vec<HelmRelease>> = HashMap::new();

    for cluster in &clusters_to_check {
        print!("Fetching from {} ... ", cluster.name);

        // Get kubeconfig path (cached or fetch)
        let kubeconfig_result = get_kubeconfig_path(cluster);

        let (kubeconfig_path, needs_cleanup, _working_hostname, _discovered_ips) =
            match kubeconfig_result {
                Ok(result) => result,
                Err(e) => {
                    println!("✗ Failed: {}", e);
                    continue;
                }
            };

        match crate::helm::fetch_helm_releases(kubeconfig_path.to_str().unwrap()) {
            Ok(releases) => {
                println!("✓ Found {} release(s)", releases.len());
                all_releases.insert(cluster.name.clone(), releases);
            }
            Err(e) => {
                println!("✗ Failed: {}", e);
            }
        }

        // Clean up temp file if needed
        if needs_cleanup {
            let _ = fs::remove_file(kubeconfig_path);
        }
    }

    println!();

    // Display releases
    display_releases(&all_releases)?;

    Ok(())
}

fn display_releases(all_releases_map: &HashMap<String, Vec<HelmRelease>>) -> Result<()> {
    let term_width = terminal_size::terminal_size()
        .map(|(terminal_size::Width(w), _)| w as usize)
        .unwrap_or(120);

    if all_releases_map.is_empty() {
        println!("No Helm releases found across configured clusters.");
        return Ok(());
    }

    // Collect all releases with their cluster name
    let mut all_releases: Vec<(String, &HelmRelease)> = Vec::new();

    for (cluster_name, releases) in all_releases_map {
        for release in releases {
            all_releases.push((cluster_name.clone(), release));
        }
    }

    if all_releases.is_empty() {
        println!("No Helm releases found across configured clusters.");
        return Ok(());
    }

    // Group releases by chart name to detect version differences
    let mut chart_versions: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    for (cluster_name, release) in &all_releases {
        let chart_name = extract_chart_name(&release.chart);
        chart_versions
            .entry(chart_name)
            .or_default()
            .entry(release.chart.clone())
            .or_default()
            .push(cluster_name.clone());
    }

    // Sort releases by cluster, then by name
    all_releases.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));

    println!("Helm Releases Across All Realms:\n");
    println!("{:-<width$}", "", width = term_width);
    println!(
        "{:<15} {:<25} {:<15} {:<30} {:<10}",
        "REALM", "RELEASE", "NAMESPACE", "CHART", "STATUS"
    );
    println!("{:-<width$}", "", width = term_width);

    let mut current_cluster = "";
    for (cluster_name, release) in &all_releases {
        if current_cluster != cluster_name {
            if !current_cluster.is_empty() {
                println!();
            }
            current_cluster = cluster_name;
        }

        let chart_name = extract_chart_name(&release.chart);
        let has_version_diff = chart_versions
            .get(&chart_name)
            .map(|versions| versions.len() > 1)
            .unwrap_or(false);

        let chart_display = if has_version_diff {
            format!("{} ⚠", release.chart)
        } else {
            release.chart.clone()
        };

        println!(
            "{:<15} {:<25} {:<15} {:<30} {:<10}",
            cluster_name, release.name, release.namespace, chart_display, release.status
        );
    }

    println!("{:-<width$}", "", width = term_width);
    println!(
        "\nTotal: {} release(s) across {} realm(s)",
        all_releases.len(),
        all_releases_map.len()
    );
    println!("⚠ = Version differs across clusters");

    Ok(())
}

fn extract_chart_name(chart: &str) -> String {
    // Extract chart name from "chart-version" format
    chart
        .rsplit_once('-')
        .map(|(name, _)| name)
        .unwrap_or(chart)
        .to_string()
}

pub fn watch(cluster_filter: Option<&str>, interval: Option<u64>) -> Result<()> {
    use std::io::Write;

    let config = Config::load()?;

    let clusters_to_check: Vec<&ClusterConfig> = match cluster_filter {
        Some(name) => {
            let cluster = config
                .get_cluster(name)
                .ok_or_else(|| anyhow!("Cluster '{}' not found", name))?;
            vec![cluster]
        }
        None => config.list_clusters(),
    };

    if clusters_to_check.is_empty() {
        println!("No clusters configured. Use 'heimdall add' to add a cluster.");
        return Ok(());
    }

    // Default to 30 seconds if no interval specified
    let refresh_interval = interval.unwrap_or(30);

    // Setup terminal: enter alternate screen, hide cursor
    print!("\x1B[?1049h"); // Enter alternate screen buffer
    print!("\x1B[?25l"); // Hide cursor
    std::io::stdout().flush().unwrap();

    // Setup Ctrl+C handler for cleanup
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, std::sync::atomic::Ordering::SeqCst);
    })
    .context("Failed to set Ctrl+C handler")?;

    let result = watch_loop(&clusters_to_check, refresh_interval, running);

    // Cleanup terminal: show cursor, exit alternate screen
    print!("\x1B[?25h"); // Show cursor
    print!("\x1B[?1049l"); // Exit alternate screen buffer
    std::io::stdout().flush().unwrap();

    result
}

struct ClusterHealthData {
    name: String,
    health: Option<ClusterHealth>,
    error: Option<String>,
    connected_endpoint: Option<String>,
    talos_version: Option<String>,
}

fn watch_loop(
    clusters_to_check: &[&ClusterConfig],
    refresh_interval: u64,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    use std::io::Write;

    loop {
        if !running.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }

        // Fetch all cluster health data FIRST (this is the slow part)
        let health_data = fetch_all_cluster_health(clusters_to_check);

        // THEN clear and display atomically (no flicker)
        print!("\x1B[2J\x1B[H");
        std::io::stdout().flush().unwrap();

        display_cluster_health_data(&health_data)?;

        // Sleep in small intervals so we can check for Ctrl+C
        for _ in 0..(refresh_interval * 10) {
            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    Ok(())
}

fn fetch_all_cluster_health(clusters: &[&ClusterConfig]) -> Vec<ClusterHealthData> {
    let mut health_data = Vec::new();

    for cluster in clusters {
        let kubeconfig_result = get_kubeconfig_path(cluster);

        let cluster_data = match kubeconfig_result {
            Ok((kubeconfig_path, needs_cleanup, working_hostname, _discovered_ips)) => {
                let health_result = fetch_cluster_health(kubeconfig_path.to_str().unwrap());

                // Clean up temp file if needed
                if needs_cleanup {
                    let _ = fs::remove_file(kubeconfig_path);
                }

                // The kubeconfig fetch already proved the machine API answers,
                // so this extra call cannot hang on an unreachable cluster
                let talos_version = if cluster.is_talos() {
                    crate::talos::node_versions(cluster)
                        .ok()
                        .and_then(|versions| crate::talos::summarize_versions(&versions))
                } else {
                    None
                };

                match health_result {
                    Ok(health) => ClusterHealthData {
                        name: cluster.name.clone(),
                        health: Some(health),
                        error: None,
                        connected_endpoint: working_hostname,
                        talos_version,
                    },
                    Err(e) => ClusterHealthData {
                        name: cluster.name.clone(),
                        health: None,
                        error: Some(e.to_string()),
                        connected_endpoint: working_hostname,
                        talos_version,
                    },
                }
            }
            Err(e) => ClusterHealthData {
                name: cluster.name.clone(),
                health: None,
                error: Some(e.to_string()),
                connected_endpoint: None,
                talos_version: None,
            },
        };

        health_data.push(cluster_data);
    }

    health_data
}

/// CLUSTER(20) NODES(8) CPU(8) MEMORY(8) STATUS(16) ENDPOINT(15) TALOS(10), single-space separated
const ROW_CONTENT_WIDTH: usize = 20 + 1 + 8 + 1 + 8 + 1 + 8 + 1 + 16 + 1 + 15 + 1 + 10;

fn display_cluster_health_data(health_data: &[ClusterHealthData]) -> Result<usize> {
    use std::io::Write;

    let (term_width, term_height) = terminal_size::terminal_size()
        .map(|(terminal_size::Width(w), terminal_size::Height(h))| (w as usize, h as usize))
        .unwrap_or((120, 24));

    let mut line_count = 0;

    // Updated color scheme - darker, more sophisticated background
    let bg_color = "\x1B[48;2;15;15;25m"; // Very dark blue-gray #0f0f19
    let reset = "\x1B[0m";
    let bold = "\x1B[1m";
    let dim = "\x1B[2m";

    // Text colors - brighter and more vibrant
    let title_text = "\x1B[38;2;120;180;255m"; // Bright blue #78b4ff
    let white_text = "\x1B[38;2;230;230;240m"; // Slightly blue-tinted white #e6e6f0
    let muted_text = "\x1B[38;2;140;140;160m"; // Muted gray for secondary info #8c8ca0

    // Status badge colors with better contrast
    let green = "\x1B[38;2;80;220;100m"; // Brighter green #50dc64
    let green_bg = "\x1B[48;2;20;80;30m"; // Dark green background for badge
    let yellow = "\x1B[38;2;255;215;50m"; // Brighter yellow #ffd732
    let yellow_bg = "\x1B[48;2;80;70;10m"; // Dark yellow/brown background
    let red = "\x1B[38;2;255;100;100m"; // Brighter red #ff6464
    let red_bg = "\x1B[48;2;80;20;20m"; // Dark red background

    let left_padding = "  "; // 2 spaces of left padding
    let right_padding = "  "; // 2 spaces of right padding
    let total_padding = 4; // left (2) + right (2)

    // Title with background and icon
    print!("{}{}{}", bg_color, bold, title_text);
    print!("{}", left_padding);
    let title = "◈ HEIMDALL CLUSTER MONITOR";
    print!("{}", title);
    print!(
        "{}",
        " ".repeat(term_width.saturating_sub(title.len() + total_padding))
    );
    print!("{}", right_padding);
    println!("{}", reset);
    line_count += 1;

    // Separator with box drawing
    print!("{}{}", bg_color, muted_text);
    print!("{}", left_padding);
    print!("{}", "━".repeat(term_width.saturating_sub(total_padding)));
    print!("{}", right_padding);
    println!("{}", reset);
    line_count += 1;

    // Header line with bright text
    // Column widths: CLUSTER(20) NODES(8) CPU(8) MEMORY(8) STATUS(16) ENDPOINT(15) TALOS(10)
    print!("{}{}{}", bg_color, bold, muted_text);
    print!("{}", left_padding);
    print!(
        "{:<20} {:<8} {:<8} {:<8} {:<16} {:<15} {:<10}",
        "CLUSTER", "NODES", "CPU", "MEMORY", "STATUS", "ENDPOINT", "TALOS"
    );
    print!(
        "{}",
        " ".repeat(term_width.saturating_sub(ROW_CONTENT_WIDTH + total_padding))
    );
    print!("{}", right_padding);
    println!("{}", reset);
    line_count += 1;

    // Separator
    print!("{}{}", bg_color, muted_text);
    print!("{}", left_padding);
    print!("{}", "─".repeat(term_width.saturating_sub(total_padding)));
    print!("{}", right_padding);
    println!("{}", reset);
    line_count += 1;

    // Display all cluster data
    for data in health_data {
        match (&data.health, &data.error) {
            (Some(health), None) => {
                let (status_text, status_fg, status_bg_color) =
                    if health.ready_nodes == health.total_nodes {
                        ("● HEALTHY", green, green_bg)
                    } else {
                        ("◐ DEGRADED", yellow, yellow_bg)
                    };

                let cpu_color = get_vibrant_metric_color(health.cpu_usage_percent);
                let mem_color = get_vibrant_metric_color(health.memory_usage_percent);

                // Print entire line with main background, only switching colors for text
                print!("{}", bg_color);
                print!("{}", left_padding);

                // Cluster name (bright white) - 20 chars
                print!("{}{}{:<20}", bold, white_text, data.name);
                print!(" ");

                // Nodes (muted) - 8 chars
                print!(
                    "{}{:<8}",
                    muted_text,
                    format!("{}/{}", health.ready_nodes, health.total_nodes)
                );
                print!(" ");

                // CPU with bar - 8 chars (e.g., " 45% ▃▃▃")
                let cpu_bar = create_mini_bar(health.cpu_usage_percent);
                let cpu_display = format!("{:>3}% {}", health.cpu_usage_percent, cpu_bar);
                print!("{}{:<8}", cpu_color, cpu_display);
                print!(" ");

                // Memory with bar - 8 chars
                let mem_bar = create_mini_bar(health.memory_usage_percent);
                let mem_display = format!("{:>3}% {}", health.memory_usage_percent, mem_bar);
                print!("{}{:<8}", mem_color, mem_display);
                print!(" ");

                // Status badge - 16 chars with its own background
                print!(
                    "{}{}{:<16}{}",
                    status_bg_color, status_fg, status_text, bg_color
                );
                print!(" ");

                // Endpoint (muted) - 15 chars
                let endpoint = data.connected_endpoint.as_deref().unwrap_or("-");
                print!("{}{:<15}", muted_text, endpoint);
                print!(" ");

                // Talos version (muted) - 10 chars, blank for other providers
                let talos_version = data.talos_version.as_deref().unwrap_or("-");
                print!("{}{:<10}", muted_text, talos_version);

                // Fill rest of line with background
                print!(
                    "{}",
                    " ".repeat(term_width.saturating_sub(ROW_CONTENT_WIDTH + total_padding))
                );
                print!("{}", right_padding);
                println!("{}", reset);
                line_count += 1;
            }
            (None, Some(_error)) => {
                print!("{}", bg_color);
                print!("{}", left_padding);
                print!("{}{}{:<20}", bold, white_text, data.name);
                print!(" ");
                print!("{}{:<8}", muted_text, "-");
                print!(" ");
                print!("{}{:<8}", muted_text, "-");
                print!(" ");
                print!("{}{:<8}", muted_text, "-");
                print!(" ");
                print!("{}{}{:<16}{}", red_bg, red, "○ DOWN", bg_color);
                print!(" ");
                let endpoint = data.connected_endpoint.as_deref().unwrap_or("-");
                print!("{}{:<15}", muted_text, endpoint);
                print!(" ");
                print!("{}{:<10}", muted_text, "-");
                print!(
                    "{}",
                    " ".repeat(term_width.saturating_sub(ROW_CONTENT_WIDTH + total_padding))
                );
                print!("{}", right_padding);
                println!("{}", reset);
                line_count += 1;
            }
            _ => {}
        }
    }

    // Separator
    print!("{}{}", bg_color, muted_text);
    print!("{}", left_padding);
    print!("{}", "─".repeat(term_width.saturating_sub(total_padding)));
    print!("{}", right_padding);
    println!("{}", reset);
    line_count += 1;

    // Footer with timestamp and keyboard hints
    print!("{}{}{}", bg_color, dim, muted_text);
    print!("{}", left_padding);
    let footer = format!(
        "⟳ {} │ Press Ctrl+C to exit",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    print!("{}", footer);
    // Use char count instead of byte length for proper Unicode handling
    let footer_char_count = footer.chars().count();
    print!(
        "{}",
        " ".repeat(term_width.saturating_sub(footer_char_count + total_padding))
    );
    print!("{}", right_padding);
    println!("{}", reset);
    line_count += 1;

    // Fill remaining screen with background color
    for _ in line_count..term_height {
        print!("{}", bg_color);
        print!("{}", " ".repeat(term_width));
        println!("{}", reset);
    }

    std::io::stdout().flush().unwrap();

    Ok(line_count)
}

// Helper function to create a mini progress bar
fn create_mini_bar(percentage: u32) -> String {
    let blocks = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let index = ((percentage as f32 / 100.0) * 7.0).round() as usize;
    let block = blocks[index.min(7)];
    format!("{}{}{}", block, block, block)
}

// Helper function to get vibrant metric colors
fn get_vibrant_metric_color(percentage: u32) -> &'static str {
    match percentage {
        0..=50 => "\x1B[38;2;80;220;100m",  // Bright green
        51..=75 => "\x1B[38;2;255;215;50m", // Bright yellow
        76..=90 => "\x1B[38;2;255;150;50m", // Bright orange
        _ => "\x1B[38;2;255;100;100m",      // Bright red
    }
}

#[derive(Debug)]
struct ClusterHealth {
    total_nodes: usize,
    ready_nodes: usize,
    cpu_usage_percent: u32,
    memory_usage_percent: u32,
}

fn fetch_cluster_health(kubeconfig_path: &str) -> Result<ClusterHealth> {
    use std::process::Command;

    // Get node information with timeout
    let nodes_output = Command::new("kubectl")
        .args(["get", "nodes", "-o", "json", "--request-timeout=5s"])
        .env("KUBECONFIG", kubeconfig_path)
        .output()
        .context("Failed to execute kubectl. Is kubectl installed?")?;

    if !nodes_output.status.success() {
        let stderr = String::from_utf8_lossy(&nodes_output.stderr);
        return Err(anyhow::anyhow!(
            "kubectl get nodes failed: {}",
            stderr.trim()
        ));
    }

    let nodes_json: serde_json::Value = serde_json::from_slice(&nodes_output.stdout)?;
    let empty_vec = vec![];
    let nodes = nodes_json["items"].as_array().unwrap_or(&empty_vec);

    let total_nodes = nodes.len();
    let ready_nodes = nodes
        .iter()
        .filter(|node| {
            node["status"]["conditions"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .any(|condition| condition["type"] == "Ready" && condition["status"] == "True")
        })
        .count();

    // Calculate total allocatable resources across all nodes
    let mut total_cpu_allocatable = 0u64;
    let mut total_mem_allocatable = 0u64;

    for node in nodes {
        if let Some(allocatable) = node["status"]["allocatable"].as_object() {
            if let Some(cpu) = allocatable.get("cpu") {
                total_cpu_allocatable += parse_cpu_value(cpu.as_str().unwrap_or("0"));
            }
            if let Some(mem) = allocatable.get("memory") {
                total_mem_allocatable += parse_memory_value(mem.as_str().unwrap_or("0"));
            }
        }
    }

    // Get actual usage from metrics-server (kubectl top nodes)
    let metrics_output = Command::new("kubectl")
        .args(["top", "nodes", "--no-headers", "--use-protocol-buffers"])
        .env("KUBECONFIG", kubeconfig_path)
        .output()
        .context("Failed to execute kubectl top nodes")?;

    let mut total_cpu_usage = 0u64;
    let mut total_mem_usage = 0u64;

    if metrics_output.status.success() {
        let metrics_text = String::from_utf8_lossy(&metrics_output.stdout);
        for line in metrics_text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 {
                // Format: NAME CPU(cores) CPU% MEMORY(bytes) MEMORY%
                // We want parts[1] (CPU cores like "364m") and parts[3] (memory like "4552Mi")
                if let Some(cpu_str) = parts.get(1) {
                    total_cpu_usage += parse_cpu_value(cpu_str);
                }
                if let Some(mem_str) = parts.get(3) {
                    total_mem_usage += parse_memory_value(mem_str);
                }
            }
        }
    } else {
        // Fall back to showing 0% if metrics-server is not available
        return Ok(ClusterHealth {
            total_nodes,
            ready_nodes,
            cpu_usage_percent: 0,
            memory_usage_percent: 0,
        });
    }

    let cpu_usage_percent = if total_cpu_allocatable > 0 {
        ((total_cpu_usage as f64 / total_cpu_allocatable as f64) * 100.0) as u32
    } else {
        0
    };

    let memory_usage_percent = if total_mem_allocatable > 0 {
        ((total_mem_usage as f64 / total_mem_allocatable as f64) * 100.0) as u32
    } else {
        0
    };

    Ok(ClusterHealth {
        total_nodes,
        ready_nodes,
        cpu_usage_percent,
        memory_usage_percent,
    })
}

fn parse_cpu_value(value: &str) -> u64 {
    // Parse CPU values like "4", "4000m", "0.5"
    if value.ends_with('m') {
        value.trim_end_matches('m').parse::<u64>().unwrap_or(0)
    } else {
        (value.parse::<f64>().unwrap_or(0.0) * 1000.0) as u64
    }
}

fn parse_memory_value(value: &str) -> u64 {
    // Parse memory values like "8Gi", "8192Mi", "8589934592"
    let value = value.trim();
    if value.ends_with("Ki") {
        value.trim_end_matches("Ki").parse::<u64>().unwrap_or(0) * 1024
    } else if value.ends_with("Mi") {
        value.trim_end_matches("Mi").parse::<u64>().unwrap_or(0) * 1024 * 1024
    } else if value.ends_with("Gi") {
        value.trim_end_matches("Gi").parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024
    } else if value.ends_with("Ti") {
        value.trim_end_matches("Ti").parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024 * 1024
    } else {
        value.parse::<u64>().unwrap_or(0)
    }
}

fn get_master_password() -> Result<String> {
    let mut cached = MASTER_PASSWORD.lock().unwrap();

    if let Some(ref password) = *cached {
        return Ok(password.clone());
    }

    // Prompt for master password
    let master_password = rpassword::prompt_password("Enter master password: ")
        .context("Failed to read master password")?;

    *cached = Some(master_password.clone());

    Ok(master_password)
}

/// Try to fetch kubeconfig from a specific hostname with retry logic
fn try_fetch_from_hostname(
    cluster: &ClusterConfig,
    hostname: &str,
    username: &str,
    ssh_port: u16,
    ssh_password: Option<&str>,
    max_retries: u32,
) -> Result<String> {
    let remote_path = cluster.remote_kubeconfig_path();
    let temp_dir = std::env::temp_dir();
    let temp_kubeconfig = temp_dir.join(format!("heimdall-{}-kubeconfig", cluster.name));

    let mut last_error = None;

    for attempt in 0..max_retries {
        if attempt > 0 {
            // Exponential backoff: 1s, 2s, 4s
            let delay = std::time::Duration::from_secs(2u64.pow(attempt - 1));
            std::thread::sleep(delay);
        }

        let output = if let Some(password) = ssh_password {
            // Use password authentication with sshpass
            Command::new("sshpass")
                .args([
                    "-p",
                    password,
                    "scp",
                    "-o",
                    "StrictHostKeyChecking=no",
                    "-o",
                    "UserKnownHostsFile=/dev/null",
                    "-o",
                    "ConnectTimeout=10",
                    "-P",
                    &ssh_port.to_string(),
                    &format!("{}@{}:{}", username, hostname, remote_path),
                    temp_kubeconfig.to_str().unwrap(),
                ])
                .output()
        } else {
            // Use cert-based authentication (no password)
            Command::new("scp")
                .args([
                    "-o",
                    "StrictHostKeyChecking=no",
                    "-o",
                    "UserKnownHostsFile=/dev/null",
                    "-o",
                    "ConnectTimeout=10",
                    "-P",
                    &ssh_port.to_string(),
                    &format!("{}@{}:{}", username, hostname, remote_path),
                    temp_kubeconfig.to_str().unwrap(),
                ])
                .output()
        };

        match output {
            Ok(output) if output.status.success() => {
                // Success! Read the fetched kubeconfig
                let kubeconfig_content = fs::read_to_string(&temp_kubeconfig)
                    .context("Failed to read fetched kubeconfig")?;

                // Clean up
                let _ = fs::remove_file(temp_kubeconfig);

                return Ok(rewrite_localhost_server(kubeconfig_content, hostname));
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                last_error = Some(anyhow!("SCP failed: {}", stderr));
            }
            Err(e) => {
                last_error = Some(anyhow!("Failed to execute scp: {}", e));
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| anyhow!("Failed to fetch kubeconfig after {} retries", max_retries)))
}

/// Point a kubeconfig at `host` when its API server address is a loopback one,
/// which is what we get from a control plane that only knows itself as localhost
fn rewrite_localhost_server(kubeconfig_content: String, host: &str) -> String {
    if is_localhost(host) {
        return kubeconfig_content;
    }

    kubeconfig_content
        .replace("https://127.0.0.1:", &format!("https://{}:", host))
        .replace("https://localhost:", &format!("https://{}:", host))
        .replace("https://::1:", &format!("https://{}:", host))
}

fn is_localhost(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

/// How long to wait when checking whether an API server address answers
const REACHABILITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Talos advertises the control plane endpoint the cluster was built with, which
/// is often on a network the client cannot reach. When that address does not
/// answer, swap in a Talos endpoint that does - they front the same control
/// plane. A server that answers is left alone, so a working kubeconfig is never
/// rewritten into a broken one.
fn rewrite_unreachable_server(kubeconfig_content: String, endpoints: &[String]) -> String {
    if endpoints.is_empty() {
        return kubeconfig_content;
    }

    let Some(server) = kubeconfig_server(&kubeconfig_content) else {
        return kubeconfig_content;
    };
    let Some((host, port)) = split_host_port(&server) else {
        return kubeconfig_content;
    };

    if is_reachable(host, port) {
        return kubeconfig_content;
    }

    for endpoint in endpoints {
        let candidate = endpoint_host(endpoint);

        if candidate != host && is_reachable(candidate, port) {
            return kubeconfig_content.replace(
                &format!("://{}:{}", host, port),
                &format!("://{}:{}", candidate, port),
            );
        }
    }

    kubeconfig_content
}

/// The API server URL of the first cluster entry in a kubeconfig
fn kubeconfig_server(kubeconfig_content: &str) -> Option<String> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(kubeconfig_content).ok()?;

    parsed
        .get("clusters")?
        .as_sequence()?
        .first()?
        .get("cluster")?
        .get("server")?
        .as_str()
        .map(String::from)
}

/// Split `https://host:port` into its host and port, IPv6 literals included
fn split_host_port(server: &str) -> Option<(&str, u16)> {
    let authority = server.split_once("://").map(|(_, rest)| rest)?;
    let authority = authority.split('/').next()?;

    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, rest) = rest.split_once(']')?;
        (host, rest.strip_prefix(':')?)
    } else {
        authority.rsplit_once(':')?
    };

    Some((host, port.parse().ok()?))
}

/// Strip the Talos API port off an endpoint, leaving the bare host
fn endpoint_host(endpoint: &str) -> &str {
    if let Some(rest) = endpoint.strip_prefix('[') {
        return rest
            .split_once(']')
            .map(|(host, _)| host)
            .unwrap_or(endpoint);
    }

    match endpoint.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => host,
        _ => endpoint,
    }
}

fn is_reachable(host: &str, port: u16) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};

    let Ok(addresses) = (host, port).to_socket_addrs() else {
        return false;
    };

    addresses
        .into_iter()
        .any(|address| TcpStream::connect_timeout(&address, REACHABILITY_TIMEOUT).is_ok())
}

/// Discover node IPs from a kubeconfig file by running kubectl get nodes
fn discover_node_ips(kubeconfig_content: &str) -> Vec<String> {
    // Write kubeconfig to temp file
    let temp_dir = std::env::temp_dir();
    let temp_kubeconfig = temp_dir.join(format!("heimdall-discovery-{}", std::process::id()));

    if fs::write(&temp_kubeconfig, kubeconfig_content).is_err() {
        return Vec::new();
    }

    // Run kubectl get nodes to discover IPs
    let output = Command::new("kubectl")
        .args(["get", "nodes", "-o", "json", "--request-timeout=5s"])
        .env("KUBECONFIG", temp_kubeconfig.to_str().unwrap())
        .output();

    // Clean up temp file
    let _ = fs::remove_file(temp_kubeconfig);

    let output = match output {
        Ok(out) if out.status.success() => out,
        _ => return Vec::new(),
    };

    // Parse JSON to extract node IPs
    let nodes_json: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(json) => json,
        Err(_) => return Vec::new(),
    };

    let mut node_ips = Vec::new();
    let empty_vec = Vec::new();
    let nodes = nodes_json["items"].as_array().unwrap_or(&empty_vec);

    for node in nodes {
        if let Some(addresses) = node["status"]["addresses"].as_array() {
            for address in addresses {
                if address["type"] == "InternalIP"
                    && let Some(ip) = address["address"].as_str()
                {
                    node_ips.push(ip.to_string());
                }
            }
        }
    }

    node_ips
}

/// Fetch a cluster's kubeconfig, returning it along with the endpoint that
/// served it (when known) and the node IPs discovered through it
fn fetch_kubeconfig(cluster: &ClusterConfig) -> Result<(String, Option<String>, Vec<String>)> {
    match cluster.provider {
        ClusterProvider::Ssh => fetch_kubeconfig_via_scp(cluster),
        ClusterProvider::Talos => fetch_kubeconfig_via_talosctl(cluster),
    }
}

/// Fetch a Talos cluster's kubeconfig with talosctl.
///
/// talosctl picks the endpoint itself, but it fetches a kubeconfig from exactly
/// one node, so the node it used is what we report back as the live one.
fn fetch_kubeconfig_via_talosctl(
    cluster: &ClusterConfig,
) -> Result<(String, Option<String>, Vec<String>)> {
    let context = crate::talos::context_info(cluster);
    let (kubeconfig_content, serving_node) = crate::talos::kubeconfig(cluster, &context.nodes)?;

    let kubeconfig_content = match serving_node {
        Some(ref node) => rewrite_localhost_server(kubeconfig_content, node),
        None => kubeconfig_content,
    };

    // Explicitly configured endpoints win over the talosconfig's own
    let endpoints = match cluster.get_hostnames() {
        configured if !configured.is_empty() => configured,
        _ => context.endpoints,
    };
    let kubeconfig_content = rewrite_unreachable_server(kubeconfig_content, &endpoints);

    let discovered_ips = discover_node_ips(&kubeconfig_content);

    Ok((kubeconfig_content, serving_node, discovered_ips))
}

fn fetch_kubeconfig_via_scp(
    cluster: &ClusterConfig,
) -> Result<(String, Option<String>, Vec<String>)> {
    let hostnames = cluster.get_prioritized_hostnames();

    if hostnames.is_empty() {
        return Err(anyhow!("No hostnames configured for cluster"));
    }

    // Check if this is localhost - use direct file copy instead of SCP
    if is_localhost(&hostnames[0]) {
        // For localhost, just read the file directly
        let expanded_path = expand_tilde(cluster.remote_kubeconfig_path())?;

        let kubeconfig_content = fs::read_to_string(&expanded_path).context(format!(
            "Failed to read kubeconfig from {}",
            expanded_path.display()
        ))?;

        // Discover node IPs for localhost too
        let discovered_ips = discover_node_ips(&kubeconfig_content);
        return Ok((kubeconfig_content, None, discovered_ips));
    }

    // For remote hosts, use SCP with failover
    let username = cluster
        .username
        .as_ref()
        .ok_or_else(|| anyhow!("No username configured for cluster"))?;

    let ssh_port = cluster.ssh_port.unwrap_or(22);

    // Decrypt password once if present
    let ssh_password = if let Some(ref password_encrypted) = cluster.password_encrypted {
        let master_password = get_master_password()?;
        Some(
            crate::crypto::decrypt_password(password_encrypted, &master_password)
                .context("Failed to decrypt SSH password - wrong master password?")?,
        )
    } else {
        None
    };

    let max_retries = 3; // Retries per hostname
    let mut all_errors = Vec::new();

    // Try each hostname in order (prioritized list)
    for hostname in &hostnames {
        match try_fetch_from_hostname(
            cluster,
            hostname,
            username,
            ssh_port,
            ssh_password.as_deref(),
            max_retries,
        ) {
            Ok(content) => {
                // Success! Discover node IPs from the cluster
                let discovered_ips = discover_node_ips(&content);
                return Ok((content, Some(hostname.clone()), discovered_ips));
            }
            Err(e) => {
                all_errors.push(format!("{}: {}", hostname, e));
            }
        }
    }

    // All hostnames failed
    Err(anyhow!(
        "Failed to fetch kubeconfig from all endpoints:\n{}",
        all_errors.join("\n")
    ))
}

/// Get kubeconfig path for a cluster - always fetches fresh to use failover
fn get_kubeconfig_path(
    cluster: &ClusterConfig,
) -> Result<(PathBuf, bool, Option<String>, Vec<String>)> {
    // Always fetch fresh to ensure failover works and we get latest config
    let (kubeconfig_content, working_hostname, discovered_ips) = fetch_kubeconfig(cluster)?;

    // Write to temp file
    let temp_dir = std::env::temp_dir();
    let temp_kubeconfig = temp_dir.join(format!("heimdall-{}.yaml", cluster.name));

    fs::write(&temp_kubeconfig, kubeconfig_content).context("Failed to write kubeconfig")?;

    Ok((temp_kubeconfig, true, working_hostname, discovered_ips)) // needs cleanup
}

pub fn sync(cluster_filter: Option<&str>) -> Result<()> {
    let mut config = Config::load()?;

    let clusters_to_sync: Vec<String> = match cluster_filter {
        Some(name) => {
            if config.get_cluster(name).is_none() {
                return Err(anyhow!("Cluster '{}' not found", name));
            }
            vec![name.to_string()]
        }
        None => config
            .list_clusters()
            .iter()
            .map(|c| c.name.clone())
            .collect(),
    };

    if clusters_to_sync.is_empty() {
        println!("No clusters configured. Use 'heimdall add' to add a cluster.");
        return Ok(());
    }

    println!("Syncing kubeconfig(s) to ~/.kube/heimdall/\n");

    let mut synced_count = 0;
    let mut failed_count = 0;

    for cluster_name in &clusters_to_sync {
        // Clone the cluster to avoid borrow issues
        let cluster = config.clusters.get(cluster_name).unwrap().clone();

        print!("Syncing {} ... ", cluster.name);

        // Fetch kubeconfig via SCP
        match fetch_kubeconfig(&cluster) {
            Ok((kubeconfig_content, working_hostname, discovered_ips)) => {
                // Ensure the heimdall directory exists
                let local_path = cluster.local_kubeconfig_path()?;
                if let Some(parent) = local_path.parent() {
                    fs::create_dir_all(parent).context("Failed to create kubeconfig directory")?;
                }

                // Write to local cache
                fs::write(&local_path, kubeconfig_content)
                    .context("Failed to write kubeconfig to local cache")?;

                // Update last synced timestamp, working hostname, and discovered IPs on the mutable config
                if let Some(cluster_mut) = config.clusters.get_mut(cluster_name) {
                    cluster_mut.last_synced = Some(chrono::Utc::now().to_rfc3339());
                    if let Some(hostname) = working_hostname {
                        cluster_mut.update_last_working_hostname(hostname);
                    }
                    if !discovered_ips.is_empty() {
                        cluster_mut.update_discovered_node_ips(discovered_ips.clone());
                        println!(
                            "✓ Synced to {} (discovered {} node IP(s))",
                            local_path.display(),
                            discovered_ips.len()
                        );
                    } else {
                        println!("✓ Synced to {}", local_path.display());
                    }
                }

                synced_count += 1;
            }
            Err(e) => {
                println!("✗ Failed: {}", e);
                failed_count += 1;
            }
        }
    }

    // Save updated config with sync timestamps
    config.save()?;

    println!(
        "\nSync complete: {} succeeded, {} failed",
        synced_count, failed_count
    );

    Ok(())
}

pub fn use_cluster(name: &str) -> Result<()> {
    let mut config = Config::load()?;

    let cluster = config
        .get_cluster(name)
        .ok_or_else(|| anyhow!("Cluster '{}' not found", name))?;

    let local_path = cluster.local_kubeconfig_path()?;

    // If kubeconfig not synced, fetch it automatically (without saving to cache)
    if !local_path.exists() {
        println!("Kubeconfig not cached locally. Fetching from remote...");

        // Fetch kubeconfig and save to local cache
        let cluster_clone = cluster.clone();
        match fetch_kubeconfig(&cluster_clone) {
            Ok((kubeconfig_content, working_hostname, discovered_ips)) => {
                // Ensure the heimdall directory exists
                if let Some(parent) = local_path.parent() {
                    fs::create_dir_all(parent).context("Failed to create kubeconfig directory")?;
                }

                // Write to local cache
                fs::write(&local_path, kubeconfig_content)
                    .context("Failed to write kubeconfig to local cache")?;

                // Update last synced timestamp, working hostname, and discovered IPs
                if let Some(cluster_mut) = config.clusters.get_mut(name) {
                    cluster_mut.last_synced = Some(chrono::Utc::now().to_rfc3339());
                    if let Some(hostname) = working_hostname {
                        cluster_mut.update_last_working_hostname(hostname);
                    }
                    if !discovered_ips.is_empty() {
                        cluster_mut.update_discovered_node_ips(discovered_ips);
                    }
                    config.save()?;
                }

                println!("✓ Kubeconfig fetched and cached\n");
            }
            Err(e) => {
                return Err(anyhow!("Failed to fetch kubeconfig: {}", e));
            }
        }
    }

    // Merge the cluster's kubeconfig into ~/.kube/config
    merge_kubeconfig_into_default(&local_path, name)?;

    println!("✓ Cluster '{}' is now active in ~/.kube/config", name);
    println!("\nYou can now use kubectl, helm, k9s, etc. directly:");
    println!("  kubectl get nodes");
    println!("  helm list");
    println!("  k9s");

    Ok(())
}

/// Merge a kubeconfig file into the default ~/.kube/config
fn merge_kubeconfig_into_default(source_path: &PathBuf, _cluster_name: &str) -> Result<()> {
    let home_dir = dirs::home_dir().context("Failed to determine home directory")?;
    let default_kubeconfig = home_dir.join(".kube").join("config");

    // Ensure ~/.kube directory exists
    if let Some(parent) = default_kubeconfig.parent() {
        fs::create_dir_all(parent).context("Failed to create .kube directory")?;
    }

    // Read the source kubeconfig (cluster we want to use)
    let source_content =
        fs::read_to_string(source_path).context("Failed to read cluster kubeconfig")?;
    let source_config: serde_yaml::Value =
        serde_yaml::from_str(&source_content).context("Failed to parse cluster kubeconfig")?;

    // Read existing default kubeconfig (if it exists)
    let mut default_config: serde_yaml::Value = if default_kubeconfig.exists() {
        let default_content =
            fs::read_to_string(&default_kubeconfig).context("Failed to read default kubeconfig")?;
        serde_yaml::from_str(&default_content).context("Failed to parse default kubeconfig")?
    } else {
        // Create a new empty kubeconfig structure
        serde_yaml::from_str(
            "apiVersion: v1\nkind: Config\nclusters: []\ncontexts: []\nusers: []\n",
        )
        .context("Failed to create default kubeconfig structure")?
    };

    // Merge clusters, contexts, and users
    merge_kubeconfig_section(&mut default_config, &source_config, "clusters")?;
    merge_kubeconfig_section(&mut default_config, &source_config, "contexts")?;
    merge_kubeconfig_section(&mut default_config, &source_config, "users")?;

    // Set the current context to the first context from the source config
    if let Some(source_context) = source_config.get("current-context") {
        default_config["current-context"] = source_context.clone();
    } else if let Some(contexts) = source_config.get("contexts").and_then(|c| c.as_sequence())
        && let Some(first_context) = contexts.first()
        && let Some(context_name) = first_context.get("name")
    {
        default_config["current-context"] = context_name.clone();
    }

    // Write the merged config back to ~/.kube/config
    let merged_content =
        serde_yaml::to_string(&default_config).context("Failed to serialize merged kubeconfig")?;
    fs::write(&default_kubeconfig, merged_content).context("Failed to write merged kubeconfig")?;

    Ok(())
}

/// Merge a specific section (clusters, contexts, or users) from source into destination
fn merge_kubeconfig_section(
    dest: &mut serde_yaml::Value,
    source: &serde_yaml::Value,
    section: &str,
) -> Result<()> {
    // Get or create the destination section as an array
    let dest_section = dest
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("Destination kubeconfig is not a mapping"))?
        .entry(serde_yaml::Value::String(section.to_string()))
        .or_insert_with(|| serde_yaml::Value::Sequence(vec![]));

    let dest_array = dest_section
        .as_sequence_mut()
        .ok_or_else(|| anyhow!("Destination {} section is not an array", section))?;

    // Get source section
    if let Some(source_section) = source.get(section).and_then(|s| s.as_sequence()) {
        for source_item in source_section {
            if let Some(source_name) = source_item.get("name") {
                // Remove existing item with the same name
                dest_array.retain(|item| item.get("name") != Some(source_name));

                // Add the new item
                dest_array.push(source_item.clone());
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_server_urls_into_host_and_port() {
        assert_eq!(
            split_host_port("https://172.20.42.21:6443"),
            Some(("172.20.42.21", 6443))
        );
        assert_eq!(
            split_host_port("https://api.example.com:6443/"),
            Some(("api.example.com", 6443))
        );
        assert_eq!(split_host_port("https://[::1]:6443"), Some(("::1", 6443)));
        assert_eq!(split_host_port("https://no-port"), None);
    }

    #[test]
    fn strips_the_talos_port_from_endpoints() {
        assert_eq!(endpoint_host("172.21.32.85"), "172.21.32.85");
        assert_eq!(endpoint_host("172.21.32.85:50000"), "172.21.32.85");
        assert_eq!(endpoint_host("[fd00::1]:50000"), "fd00::1");
        assert_eq!(endpoint_host("fd00::1"), "fd00::1");
    }

    #[test]
    fn reads_the_server_from_a_kubeconfig() {
        let kubeconfig = "apiVersion: v1\nkind: Config\nclusters:\n- name: talos-dev\n  cluster:\n    server: https://172.20.42.21:6443\n";

        assert_eq!(
            kubeconfig_server(kubeconfig).as_deref(),
            Some("https://172.20.42.21:6443")
        );
        assert_eq!(kubeconfig_server("clusters: []\n"), None);
    }

    #[test]
    fn leaves_the_kubeconfig_alone_when_there_are_no_endpoints() {
        let kubeconfig =
            "clusters:\n- cluster:\n    server: https://172.20.42.21:6443\n".to_string();

        assert_eq!(
            rewrite_unreachable_server(kubeconfig.clone(), &[]),
            kubeconfig
        );
    }

    #[test]
    fn points_a_loopback_server_at_the_host_it_came_from() {
        let kubeconfig = "    server: https://127.0.0.1:6443\n".to_string();

        assert_eq!(
            rewrite_localhost_server(kubeconfig, "10.0.0.5"),
            "    server: https://10.0.0.5:6443\n"
        );
    }
}
