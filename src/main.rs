use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};

mod commands;
mod config;
mod crypto;
mod helm;
mod talos;

#[derive(Parser)]
#[command(name = "heimdall")]
#[command(about = "A CLI tool for managing Kubernetes cluster configurations", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all configured Kubernetes clusters
    Ls,

    /// Get kubeconfig for a specific cluster
    Get {
        /// Name of the cluster
        name: String,

        /// Write to file instead of stdout
        #[arg(short, long)]
        output: Option<String>,
    },

    /// Add a new cluster configuration
    Add {
        /// Name of the cluster
        name: String,

        /// SSH host(s), or Talos endpoint(s) with --talos (comma-separated for HA)
        #[arg(short = 'H', long)]
        hostname: Option<String>,

        /// SSH port (default: 22)
        #[arg(short, long)]
        port: Option<u16>,

        /// SSH username
        #[arg(short, long)]
        username: Option<String>,

        /// Prompt for SSH password (secure input)
        #[arg(long)]
        password: bool,

        /// Path to kubeconfig on remote host (default: ~/.kube/config)
        #[arg(short = 'k', long)]
        kubeconfig: Option<String>,

        /// Talos Linux cluster: fetch the kubeconfig with talosctl instead of SSH
        #[arg(long, conflicts_with_all = ["port", "username", "password", "kubeconfig"])]
        talos: bool,

        /// Path to the talosconfig (default: $TALOSCONFIG, else ~/.talos/config)
        #[arg(long, requires = "talos")]
        talosconfig: Option<String>,

        /// Context to use inside the talosconfig (default: its current context)
        #[arg(long = "talos-context", requires = "talos")]
        talos_context: Option<String>,

        /// Description of the cluster
        #[arg(short, long)]
        description: Option<String>,
    },

    /// Remove a cluster configuration
    Rm {
        /// Name of the cluster to remove
        name: String,
    },

    /// Show detailed information about a cluster
    Info {
        /// Name of the cluster
        name: String,
    },

    /// Monitor cluster health (nodes, CPU, memory)
    Watch {
        /// Specific cluster to watch (optional, defaults to all)
        #[arg(short, long)]
        cluster: Option<String>,

        /// Refresh interval in seconds (default: 30)
        #[arg(short = 'i', long)]
        interval: Option<u64>,
    },

    /// List Helm releases across all clusters
    Releases {
        /// Specific cluster to query (optional, defaults to all)
        #[arg(short, long)]
        cluster: Option<String>,
    },

    /// Sync kubeconfig(s) from remote hosts to local cache
    Sync {
        /// Specific cluster to sync (optional, defaults to all)
        #[arg(short, long)]
        cluster: Option<String>,
    },

    /// Set KUBECONFIG to use a specific cluster
    Use {
        /// Name of the cluster to use
        name: String,
    },

    /// Generate shell completions
    Completions {
        /// Shell type to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Ls => commands::list(),
        Commands::Get { name, output } => commands::get(&name, output.as_deref()),
        Commands::Add {
            name,
            hostname,
            port,
            username,
            password,
            kubeconfig,
            talos,
            talosconfig,
            talos_context,
            description,
        } => commands::add(commands::AddOptions {
            name: &name,
            hostname: hostname.as_deref(),
            ssh_port: port,
            username: username.as_deref(),
            prompt_password: password,
            kubeconfig_path: kubeconfig.as_deref(),
            description: description.as_deref(),
            talos,
            talosconfig_path: talosconfig.as_deref(),
            talos_context: talos_context.as_deref(),
        }),
        Commands::Rm { name } => commands::remove(&name),
        Commands::Info { name } => commands::info(&name),
        Commands::Watch { cluster, interval } => commands::watch(cluster.as_deref(), interval),
        Commands::Releases { cluster } => commands::releases(cluster.as_deref()),
        Commands::Sync { cluster } => commands::sync(cluster.as_deref()),
        Commands::Use { name } => commands::use_cluster(&name),
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            let bin_name = cmd.get_name().to_string();
            generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
            Ok(())
        }
    }
}
