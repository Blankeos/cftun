use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A lightweight CLI for managing Cloudflare Tunnels as persistent webhook endpoints.
#[derive(Parser)]
#[command(name = "cftun")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "Cloudflare Tunnel as a free ngrok alternative for webhooks")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new tunnel and point a subdomain to it
    Create {
        /// Name for this tunnel (e.g. my-dev-tunnel)
        name: String,
        /// Public hostname to route (e.g. webhook.example.com)
        hostname: String,
        /// Local service to forward to (e.g. 3000, http://localhost:3000, https://localhost:8443)
        local: String,
    },
    /// List tunnels managed by cftun
    #[command(visible_aliases = ["ls"])]
    List,
    /// Show full status from cloudflared, including non-cftun tunnels
    #[command(name = "status")]
    Status,
    /// Run a tunnel by name
    Run {
        /// Name of the tunnel to run
        name: String,
    },
    /// Show the config for a tunnel
    Show {
        /// Name of the tunnel to show
        name: String,
    },
    /// Import an existing cloudflared tunnel into cftun
    Import {
        /// Name of the existing tunnel (as shown in `cftun list`)
        name: String,
        /// Public hostname to route (e.g. webhook.example.com)
        hostname: String,
        /// Local service to forward to (e.g. 3000, http://localhost:3000, https://localhost:8443)
        local: String,
    },
    /// Update a tunnel's hostname or local service
    Update {
        /// Name of the tunnel to update
        name: String,
        /// New public hostname (subdomain)
        #[arg(long)]
        hostname: Option<String>,
        /// New local service (e.g. 3000 or http://localhost:3000)
        #[arg(long)]
        local: Option<String>,
    },
    /// Delete a tunnel and its DNS route
    Delete {
        /// Name of the tunnel to delete
        name: String,
        /// Also delete the local config file and DNS route
        #[arg(long)]
        cleanup: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct TunnelConfig {
    tunnel: String,
    #[serde(rename = "credentials-file")]
    credentials_file: PathBuf,
    #[serde(default)]
    ingress: Vec<IngressRule>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IngressRule {
    hostname: String,
    service: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // ensure cloudflared is installed before doing anything
    which::which("cloudflared").map_err(|_| {
        anyhow!(
            "cloudflared not found in PATH. Install it first:\n  macOS: brew install cloudflared\n  Windows: winget install --id Cloudflare.cloudflared\n  Linux: see https://github.com/cloudflare/cloudflared/releases"
        )
    })?;

    match cli.command {
        Commands::Create { name, hostname, local } => create(&name, &hostname, &local),
        Commands::List => list(),
        Commands::Show { name } => show(&name),
        Commands::Import { name, hostname, local } => import(&name, &hostname, &local),
        Commands::Update { name, hostname, local } => update(&name, hostname.as_deref(), local.as_deref()),
        Commands::Status => status(),
        Commands::Run { name } => run(&name),
        Commands::Delete { name, cleanup } => delete(&name, cleanup),
    }
}

fn cftun_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("could not find home directory"))?
        .join(".cloudflared")
        .join("cftun");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn metadata_path() -> Result<PathBuf> {
    Ok(cftun_dir()?.join("tunnels.yaml"))
}

fn credentials_file(uuid: &str) -> PathBuf {
    dirs::home_dir()
        .expect("home dir")
        .join(".cloudflared")
        .join(format!("{uuid}.json"))
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Metadata {
    #[serde(default)]
    tunnels: BTreeMap<String, TunnelMeta>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TunnelMeta {
    uuid: String,
    hostname: String,
    service: String,
    config_path: PathBuf,
}

fn load_metadata() -> Result<Metadata> {
    let path = metadata_path()?;
    if !path.exists() {
        return Ok(Metadata::default());
    }
    let content = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_yaml::from_str(&content).with_context(|| format!("parsing {}", path.display()))
}

fn save_metadata(meta: &Metadata) -> Result<()> {
    let path = metadata_path()?;
    let content = serde_yaml::to_string(meta).context("serializing metadata")?;
    fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn normalize_service(local: &str) -> String {
    if local.starts_with("http://") || local.starts_with("https://") {
        local.to_string()
    } else if let Some(port) = local.parse::<u16>().ok() {
        let scheme = if port == 443 || port == 8443 {
            "https"
        } else {
            "http"
        };
        format!("{scheme}://localhost:{port}")
    } else {
        format!("http://localhost:{local}")
    }
}

fn run_cloudflared(args: &[&str]) -> Result<std::process::Output> {
    let output = Command::new("cloudflared")
        .args(args)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .with_context(|| format!("running cloudflared {}", args.join(" ")))?;
    Ok(output)
}

fn cloudflared_ok(args: &[&str]) -> Result<String> {
    let output = run_cloudflared(args)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!(
            "cloudflared {} failed ({}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status,
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(stdout.into_owned())
}

fn create(name: &str, hostname: &str, local: &str) -> Result<()> {
    // check cloudflared-level conflicts
    let cf_tunnels = fetch_cloudflared_tunnels().unwrap_or_default();
    if cf_tunnels.iter().any(|t| t.name == name) {
        anyhow::bail!(
            "a cloudflared tunnel named '{}' already exists. pick a different name or import it with `cftun import {} <hostname> <port/url>`",
            name, name
        );
    }

    let mut meta = load_metadata()?;
    if meta.tunnels.contains_key(name) {
        anyhow::bail!(
            "tunnel '{}' already exists in cftun. run `cftun list` to see it",
            name
        );
    }

    // create tunnel
    println!("{} Creating cloudflared tunnel '{}'...", "→".cyan(), name);
    let output = cloudflared_ok(&["tunnel", "create", name])?;
    let uuid = parse_uuid(&output)
        .ok_or_else(|| anyhow!("could not parse tunnel UUID from cloudflared output"))?;
    println!("  Created tunnel UUID: {}", uuid.dimmed());

    // route dns
    println!("{} Routing {} → {}...", "→".cyan(), hostname, name);
    cloudflared_ok(&["tunnel", "route", "dns", name, hostname])?;

    write_tunnel_config(&mut meta, name, &uuid, hostname, local)
}

fn write_tunnel_config(
    meta: &mut Metadata,
    name: &str,
    uuid: &str,
    hostname: &str,
    local: &str,
) -> Result<()> {
    let service = normalize_service(local);
    let config_path = cftun_dir()?.join(format!("{name}.yaml"));
    let cfg = TunnelConfig {
        tunnel: uuid.to_string(),
        credentials_file: credentials_file(uuid),
        ingress: vec![
            IngressRule {
                hostname: hostname.to_string(),
                service: service.clone(),
            },
            IngressRule {
                hostname: String::new(),
                service: "http_status:404".to_string(),
            },
        ],
    };
    let yaml = serde_yaml::to_string(&cfg).context("serializing tunnel config")?;
    fs::write(&config_path, yaml).with_context(|| format!("writing {}", config_path.display()))?;

    meta.tunnels.insert(
        name.to_string(),
        TunnelMeta {
            uuid: uuid.to_string(),
            hostname: hostname.to_string(),
            service: service.clone(),
            config_path: config_path.clone(),
        },
    );
    save_metadata(meta)?;

    println!("{} Tunnel '{}' ready.", "✓".green(), name);
    println!("  Public URL:   {}", format!("https://{}", hostname).green().underline());
    println!("  Local target: {}", service.dimmed());
    println!("  Run it with:  {}", format!("cftun run {}", name).cyan());

    Ok(())
}

fn parse_uuid(output: &str) -> Option<String> {
    // cloudflared prints: Created tunnel my-tunnel with id aaafc451-1eea-47a4-81e2-3c02b3907c37
    output
        .lines()
        .find(|l| l.contains("with id"))
        .and_then(|l| l.split_whitespace().rev().next())
        .map(|s| s.to_string())
}

#[derive(Debug, Deserialize)]
struct CloudflaredTunnel {
    id: String,
    name: String,
    #[serde(rename = "created_at")]
    created_at: String,
    #[serde(rename = "deleted_at", default)]
    #[allow(dead_code)]
    deleted_at: String,
    #[serde(default)]
    connections: Vec<serde_yaml::Value>,
}

fn fetch_cloudflared_tunnels() -> Result<Vec<CloudflaredTunnel>> {
    let output = run_cloudflared(&["tunnel", "list", "-o", "json"])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("cloudflared tunnel list failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // cloudflared may append JSON log lines after the array; only parse the first array
    let array_start = stdout.find('[').unwrap_or(0);
    let array_end = stdout.rfind(']').map(|i| i + 1).unwrap_or(stdout.len());
    let json = &stdout[array_start..array_end];
    serde_json::from_str(json).context("parsing cloudflared tunnel list JSON")
}

fn list() -> Result<()> {
    let cf = fetch_cloudflared_tunnels().unwrap_or_default();
    let meta = load_metadata()?;
    let known: HashSet<&String> = meta.tunnels.keys().collect();

    println!("{} Tunnels managed by cftun:", "●".cyan());
    if meta.tunnels.is_empty() {
        println!("  (none)");
    } else {
        for (name, t) in &meta.tunnels {
            let online = cf.iter().any(|c| c.id == t.uuid && !c.connections.is_empty());
            let status = if online { "online".green() } else { "offline".dimmed() };
            println!(
                "  {} {} → {} ({}) [{}]",
                "•".cyan(),
                name.bold(),
                format!("https://{}", t.hostname).green(),
                t.service.dimmed(),
                status
            );
        }
    }
    println!();

    let others: Vec<&CloudflaredTunnel> = cf.iter().filter(|c| !known.contains(&c.name)).collect();
    if !others.is_empty() {
        println!("{} Other cloudflared tunnels:", "●".cyan());
        for t in &others {
            let online = if t.connections.is_empty() { "offline".dimmed() } else { "online".green() };
            println!("  {} {} ({}) [{}]", "•".cyan(), t.name.bold(), t.id.dimmed(), online);
        }
        println!();
    }

    println!("  Create one with: {}", "cftun create <name> <hostname> <port/url>".cyan());
    if !others.is_empty() {
        println!(
            "  Or adopt an existing tunnel with: {}",
            "cftun import <name> <hostname> <port/url>".cyan()
        );
    }
    Ok(())
}

fn status() -> Result<()> {
    let cf = fetch_cloudflared_tunnels()?;
    let meta = load_metadata()?;
    println!("{}", "Cloudflared tunnel status".bold());
    println!("  {}{}", "Total tunnels: ".dimmed(), cf.len());
    println!("  {}{}", "Managed by cftun: ".dimmed(), meta.tunnels.len());
    println!(
        "  {}{}",
        "Currently online: ".dimmed(),
        cf.iter().filter(|t| !t.connections.is_empty()).count()
    );
    println!();
    for t in &cf {
        let online = !t.connections.is_empty();
        let status = if online { "online".green().bold() } else { "offline".dimmed() };
        let managed = meta.tunnels.get(&t.name);
        println!("  {} {} - {}", status, t.name.bold(), t.id.dimmed());
        if let Some(m) = managed {
            println!(
                "    {} → {}",
                format!("https://{}", m.hostname).green(),
                m.service.dimmed()
            );
        }
        println!(
            "    Created: {} | Connections: {}",
            t.created_at.dimmed(),
            t.connections.len()
        );
    }
    Ok(())
}

fn show(name: &str) -> Result<()> {
    let meta = load_metadata()?;
    let t = meta.tunnels.get(name).ok_or_else(|| {
        anyhow!(
            "tunnel '{}' not found. run `cftun list` to see available tunnels (or import it with `cftun import <name> <hostname> <port/url>`)",
            name
        )
    })?;
    let content = fs::read_to_string(&t.config_path)
        .with_context(|| format!("reading {}", t.config_path.display()))?;
    println!(
        "{} Config for '{}' ({}):",
        "●".cyan(),
        name.bold(),
        t.config_path.display()
    );
    println!("{}", content);
    Ok(())
}

fn import(name: &str, hostname: &str, local: &str) -> Result<()> {
    let mut meta = load_metadata()?;
    if meta.tunnels.contains_key(name) {
        anyhow::bail!("tunnel '{}' is already managed by cftun", name);
    }

    let cf = fetch_cloudflared_tunnels().context("fetching cloudflared tunnels")?;
    let existing = cf
        .into_iter()
        .find(|t| t.name == name)
        .ok_or_else(|| anyhow!("no cloudflared tunnel named '{}' found. run `cftun list` to see existing tunnels", name))?;

    // Re-route DNS to the new hostname
    println!("{} Routing DNS {} → {}...", "→".cyan(), hostname, name);
    let _ = run_cloudflared(&["tunnel", "route", "dns", "delete", hostname]);
    cloudflared_ok(&["tunnel", "route", "dns", name, hostname])?;

    write_tunnel_config(&mut meta, name, &existing.id, hostname, local)
}

fn update(name: &str, hostname: Option<&str>, local: Option<&str>) -> Result<()> {
    let mut meta = load_metadata()?;
    let t = meta.tunnels.get_mut(name).ok_or_else(|| {
        anyhow!(
            "tunnel '{}' not found. run `cftun list` to see available tunnels (or import it with `cftun import <name> <hostname> <port/url>`)",
            name
        )
    })?;

    if hostname.is_none() && local.is_none() {
        anyhow::bail!("provide at least one of --hostname or --local");
    }

    let new_hostname = hostname.unwrap_or(&t.hostname).to_string();
    let new_service = local.map(normalize_service).unwrap_or_else(|| t.service.clone());
    let hostname_changed = new_hostname != t.hostname;

    // Re-route DNS if hostname changed
    if hostname_changed {
        println!("{} Updating DNS route: {} → {}...", "→".cyan(), t.hostname, new_hostname);
        let _ = run_cloudflared(&["tunnel", "route", "dns", "delete", &t.hostname]);
        cloudflared_ok(&["tunnel", "route", "dns", name, &new_hostname])?;
    }

    // Rewrite config file
    let cfg = TunnelConfig {
        tunnel: t.uuid.clone(),
        credentials_file: credentials_file(&t.uuid),
        ingress: vec![
            IngressRule {
                hostname: new_hostname.clone(),
                service: new_service.clone(),
            },
            IngressRule {
                hostname: String::new(),
                service: "http_status:404".to_string(),
            },
        ],
    };
    let yaml = serde_yaml::to_string(&cfg).context("serializing tunnel config")?;
    fs::write(&t.config_path, yaml).with_context(|| format!("writing {}", t.config_path.display()))?;

    // Update metadata
    t.hostname = new_hostname.clone();
    t.service = new_service.clone();
    save_metadata(&meta)?;

    println!("{} Tunnel '{}' updated.", "✓".green(), name);
    println!("  Public URL:   {}", format!("https://{}", new_hostname).green().underline());
    println!("  Local target: {}", new_service.dimmed());
    Ok(())
}

fn run(name: &str) -> Result<()> {
    let meta = load_metadata()?;
    let t = meta.tunnels.get(name).ok_or_else(|| {
        anyhow!(
            "tunnel '{}' not found. run `cftun list` to see available tunnels (or import it with `cftun import <name> <hostname> <port/url>`)",
            name
        )
    })?;

    println!(
        "{} Starting tunnel '{}' → {} → {}\n",
        "→".cyan(),
        name,
        format!("https://{}", t.hostname).green(),
        t.service.dimmed()
    );

    // Run cloudflared interactively, letting it keep stdout/stderr connected to the terminal.
    let mut child = Command::new("cloudflared")
        .args(&["tunnel", "--config", &t.config_path.to_string_lossy(), "run"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| "running cloudflared tunnel")?;

    let status = child.wait().context("waiting for cloudflared process")?;
    if !status.success() {
        anyhow::bail!("cloudflared exited with {}", status);
    }
    Ok(())
}

fn delete(name: &str, cleanup: bool) -> Result<()> {
    let mut meta = load_metadata()?;
    let t = meta.tunnels.remove(name).ok_or_else(|| {
        anyhow!(
            "tunnel '{}' not found in cftun metadata. run `cftun list` to see available tunnels (or import it with `cftun import {} <hostname> <port/url>`)",
            name,
            name
        )
    })?;

    if cleanup {
        println!("{} Deleting cloudflared tunnel '{}'...", "→".cyan(), name);
        let _ = run_cloudflared(&["tunnel", "delete", &t.uuid]); // ignore failures if already gone

        println!("{} Removing DNS route {}...", "→".cyan(), t.hostname);
        let _ = run_cloudflared(&["tunnel", "route", "dns", "delete", &t.hostname]);

        if t.config_path.exists() {
            fs::remove_file(&t.config_path)
                .with_context(|| format!("deleting {}", t.config_path.display()))?;
        }
        save_metadata(&meta)?;
        println!("{} Tunnel '{}' removed.", "✓".green(), name);
    } else {
        // just forget locally
        save_metadata(&meta)?;
        println!(
            "{} Tunnel '{}' removed from cftun metadata. Use {} to also delete from Cloudflare.",
            "✓".green(),
            name,
            "--cleanup".cyan()
        );
    }
    Ok(())
}
