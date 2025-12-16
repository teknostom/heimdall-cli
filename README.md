# Heimdall

A CLI tool for managing Kubernetes cluster configurations across multiple remote hosts.

## Overview

Heimdall simplifies the management of multiple Kubernetes clusters by storing cluster connection details and securely fetching kubeconfig files from remote hosts via SSH. It provides a centralized way to access, monitor, and manage your Kubernetes clusters.

## Features

- **Cluster Management**: Add, remove, and list Kubernetes cluster configurations
- **Secure Storage**: Encrypted storage of SSH credentials using AES-GCM
- **Remote Access**: Fetch kubeconfig files from remote hosts via SSH
- **Cluster Monitoring**: Watch cluster health metrics (nodes, CPU, memory)
- **Helm Integration**: List Helm releases across all clusters
- **Multi-Cluster Support**: Manage multiple clusters from a single interface
- **Shell Completions**: Generate completions for bash, zsh, fish, and PowerShell

## Installation

### From Source

```bash
git clone <repository-url>
cd heimdall
cargo build --release
sudo cp target/release/heimdall /usr/local/bin/
```

## Usage

### List Clusters

```bash
heimdall ls
```

### Add a Cluster

```bash
heimdall add my-cluster \
  --hostname 192.168.1.100 \
  --username admin \
  --password \
  --description "Production cluster"
```

### Get Kubeconfig

```bash
# Print to stdout
heimdall get my-cluster

# Save to file
heimdall get my-cluster -o ~/.kube/my-cluster-config
```

### Cluster Information

```bash
heimdall info my-cluster
```

### Monitor Cluster Health

```bash
# Watch all clusters
heimdall watch

# Watch specific cluster with custom interval
heimdall watch --cluster my-cluster --interval 10
```

### List Helm Releases

```bash
# All clusters
heimdall releases

# Specific cluster
heimdall releases --cluster my-cluster
```

### Sync Kubeconfigs

```bash
# Sync all clusters
heimdall sync

# Sync specific cluster
heimdall sync --cluster my-cluster
```

### Use a Cluster

Set the `KUBECONFIG` environment variable to use a specific cluster:

```bash
eval $(heimdall use my-cluster)
```

### Remove a Cluster

```bash
heimdall rm my-cluster
```

### Shell Completions

Generate completions for your shell:

```bash
# Bash
heimdall completions bash > /etc/bash_completion.d/heimdall

# Zsh
heimdall completions zsh > ~/.zsh/completions/_heimdall

# Fish
heimdall completions fish > ~/.config/fish/completions/heimdall.fish

# PowerShell
heimdall completions powershell > heimdall.ps1
```

## Configuration

Heimdall stores its configuration in `~/.config/heimdall/config.yaml`. Credentials are encrypted using a master password derived from your system's keyring.

### Configuration File Structure

```yaml
clusters:
  - name: my-cluster
    hostname: 192.168.1.100
    port: 22
    username: admin
    password: <encrypted>
    kubeconfig_path: ~/.kube/config
    description: Production cluster
    created_at: 2025-01-15T10:30:00Z
```

## Security

- SSH passwords are encrypted using AES-256-GCM
- Master encryption key is derived from system-level authentication
- Credentials are never stored in plain text
- All SSH connections use standard SSH protocols

## Requirements

- SSH access to remote Kubernetes hosts
- kubectl installed on remote hosts (optional, for monitoring features)

## License

[Add your license here]

## Contributing

Any and all improvements and feature implementations are appreciated.
