use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use cliclack::{confirm, input, intro, outro, outro_cancel, outro_note, select, spinner};
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
        name: Option<String>,
        /// Public hostname to route (e.g. webhook.example.com)
        hostname: Option<String>,
        /// Local service to forward to (e.g. 3000, http://localhost:3000)
        local: Option<String>,
    },
    /// List tunnels managed by cftun
    #[command(visible_aliases = ["ls"])]
    List,
    /// Show full status from cloudflared, including non-cftun tunnels
    #[command(name = "status")]
    Status,
    /// Run a tunnel by name
    Run {
        /// Name of the tunnel to run. Omit to pick from a filterable list.
        name: Option<String>,
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
        /// Local service to forward to (e.g. 3000, http://localhost:3000)
        local: String,
    },
    /// Update a tunnel's hostname or local service
    Update {
        /// Name of the tunnel to update
        name: Option<String>,
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
        name: Option<String>,
        /// Also delete the local config file and DNS route
        #[arg(long)]
        cleanup: bool,
    },
}

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

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

#[derive(Debug, Serialize, Deserialize, Default)]
struct Metadata {
    #[serde(default)]
    tunnels: BTreeMap<String, TunnelMeta>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct TunnelMeta {
    uuid: String,
    hostname: String,
    service: String,
    config_path: PathBuf,
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

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();

    // ensure cloudflared is installed before doing anything
    which::which("cloudflared").map_err(|_| {
        anyhow!(
            "cloudflared not found in PATH. Install it first:\n  macOS: brew install cloudflared\n  Windows: winget install --id Cloudflare.cloudflared\n  Linux: see https://github.com/cloudflare/cloudflared/releases"
        )
    })?;

    // Intercept Ctrl-C so cliclack prompts cancel gracefully (like ESC)
    // instead of killing the process with the cursor hidden.
    ctrlc::set_handler(|| {}).map_err(|e| anyhow!("setting Ctrl-C handler: {e}"))?;

    match cli.command {
        Commands::Create { name, hostname, local } => {
            create(name.as_deref(), hostname.as_deref(), local.as_deref())
        }
        Commands::List => list(),
        Commands::Show { name } => show(&name),
        Commands::Import { name, hostname, local } => import(&name, &hostname, &local),
        Commands::Update { name, hostname, local } => {
            update(name.as_deref(), hostname.as_deref(), local.as_deref())
        }
        Commands::Status => status(),
        Commands::Run { name } => run(name.as_deref()),
        Commands::Delete { name, cleanup } => delete(name.as_deref(), cleanup),
    }
}

// ---------------------------------------------------------------------------
// Path & metadata helpers
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Service & cloudflared helpers
// ---------------------------------------------------------------------------

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

fn parse_uuid(output: &str) -> Option<String> {
    // cloudflared prints: Created tunnel my-tunnel with id aaafc451-1eea-47a4-81e2-3c02b3907c37
    output
        .lines()
        .find(|l| l.contains("with id"))
        .and_then(|l| l.split_whitespace().rev().next())
        .map(|s| s.to_string())
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

/// Build a TunnelConfig and write it to disk, insert into metadata, and return the meta entry.
fn write_tunnel_config(
    meta: &mut Metadata,
    name: &str,
    uuid: &str,
    hostname: &str,
    local: &str,
) -> Result<TunnelMeta> {
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

    let tunnel_meta = TunnelMeta {
        uuid: uuid.to_string(),
        hostname: hostname.to_string(),
        service: service.clone(),
        config_path: config_path.clone(),
    };
    meta.tunnels.insert(name.to_string(), tunnel_meta.clone());
    save_metadata(meta)?;

    Ok(tunnel_meta)
}

/// Rewrite a tunnel's config YAML (used by update).
fn rewrite_config(path: &PathBuf, uuid: &str, hostname: &str, service: &str) -> Result<()> {
    let cfg = TunnelConfig {
        tunnel: uuid.to_string(),
        credentials_file: credentials_file(uuid),
        ingress: vec![
            IngressRule {
                hostname: hostname.to_string(),
                service: service.to_string(),
            },
            IngressRule {
                hostname: String::new(),
                service: "http_status:404".to_string(),
            },
        ],
    };
    let yaml = serde_yaml::to_string(&cfg).context("serializing tunnel config")?;
    fs::write(path, yaml).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn tunnel_select_prompt(message: &str, meta: &Metadata) -> Result<String> {
    if meta.tunnels.is_empty() {
        anyhow::bail!("no tunnels managed by cftun. create one with `cftun create`");
    }
    let mut sel = select(message).filter_mode().max_rows(10);
    for (n, t) in &meta.tunnels {
        sel = sel.item(
            n.clone(),
            format!("{}  https://{}  {}", n.bold(), t.hostname, t.service.dimmed()),
            "",
        );
    }
    Ok(sel.interact()?)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn create(name: Option<&str>, hostname: Option<&str>, local: Option<&str>) -> Result<()> {
    let interactive = name.is_none() || hostname.is_none() || local.is_none();

    let mut meta = load_metadata()?;
    let cf_tunnels = fetch_cloudflared_tunnels().unwrap_or_default();

    if interactive {
        intro("Create a new tunnel")?;
    }

    // --- Name ---
    let name = match name {
        Some(n) => n.to_string(),
        None => {
            let managed: HashSet<String> = meta.tunnels.keys().cloned().collect();
            let cf_names: HashSet<String> = cf_tunnels.iter().map(|t| t.name.clone()).collect();
            input("Tunnel name")
                .placeholder("e.g. my-dev-tunnel")
                .validate(move |s: &String| {
                    let s = s.trim();
                    if s.is_empty() {
                        Err("Name is required".to_string())
                    } else if managed.contains(s) {
                        Err("Already managed by cftun".to_string())
                    } else if cf_names.contains(s) {
                        Err("Exists in cloudflared — import it instead".to_string())
                    } else {
                        Ok(())
                    }
                })
                .interact()?
        }
    };

    // Conflict checks (both paths)
    if cf_tunnels.iter().any(|t| t.name == name) {
        anyhow::bail!(
            "a cloudflared tunnel named '{}' already exists. pick a different name or import it with `cftun import {} <hostname> <port/url>`",
            name,
            name
        );
    }
    if meta.tunnels.contains_key(&name) {
        anyhow::bail!(
            "tunnel '{}' already exists in cftun. run `cftun list` to see it",
            name
        );
    }

    // --- Hostname ---
    let hostname = match hostname {
        Some(h) => h.to_string(),
        None => {
            input("Public hostname")
                .placeholder("e.g. webhook.example.com")
                .validate(|s: &String| {
                    let s = s.trim();
                    if s.is_empty() {
                        Err("Hostname is required".to_string())
                    } else if !s.contains('.') {
                        Err("Must be a valid domain name".to_string())
                    } else {
                        Ok(())
                    }
                })
                .interact()?
        }
    };

    // --- Local service ---
    let local = match local {
        Some(l) => l.to_string(),
        None => {
            input("Local service")
                .placeholder("e.g. 3000 or http://localhost:3000")
                .autocomplete(vec![
                    "3000".to_string(),
                    "8080".to_string(),
                    "8443".to_string(),
                    "http://localhost:3000".to_string(),
                    "http://localhost:8080".to_string(),
                ])
                .interact()?
        }
    };

    // --- Confirm ---
    if interactive {
        let service = normalize_service(&local);
        let yes = confirm(format!(
            "Create tunnel '{}' → https://{} → {}?",
            name, hostname, service
        ))
        .interact()?;
        if !yes {
            outro_cancel("Cancelled.")?;
            return Ok(());
        }
    }

    // --- Execute ---
    if interactive {
        let sp = spinner();
        sp.start(format!("Creating tunnel '{}'...", name));
        let output = cloudflared_ok(&["tunnel", "create", &name])?;
        let uuid = parse_uuid(&output)
            .ok_or_else(|| anyhow!("could not parse tunnel UUID from cloudflared output"))?;
        sp.stop(format!("Created: {}", uuid));

        let sp = spinner();
        sp.start(format!("Routing DNS {} → {}...", hostname, name));
        cloudflared_ok(&["tunnel", "route", "dns", &name, &hostname])?;
        sp.stop("DNS route created");

        let tm = write_tunnel_config(&mut meta, &name, &uuid, &hostname, &local)?;

        outro_note(
            format!("Tunnel '{}' is ready!", name),
            format!(
                "Public URL:   https://{}\nLocal target: {}\nRun it with:  cftun run {}",
                tm.hostname, tm.service, name
            ),
        )?;
    } else {
        println!("{} Creating cloudflared tunnel '{}'...", "→".cyan(), name);
        let output = cloudflared_ok(&["tunnel", "create", &name])?;
        let uuid = parse_uuid(&output)
            .ok_or_else(|| anyhow!("could not parse tunnel UUID from cloudflared output"))?;
        println!("  Created tunnel UUID: {}", uuid.dimmed());

        println!("{} Routing {} → {}...", "→".cyan(), hostname, name);
        cloudflared_ok(&["tunnel", "route", "dns", &name, &hostname])?;

        let tm = write_tunnel_config(&mut meta, &name, &uuid, &hostname, &local)?;

        println!("{} Tunnel '{}' ready.", "✓".green(), name);
        println!(
            "  Public URL:   {}",
            format!("https://{}", tm.hostname).green().underline()
        );
        println!("  Local target: {}", tm.service.dimmed());
        println!("  Run it with:  {}", format!("cftun run {}", name).cyan());
    }

    Ok(())
}

fn update(name: Option<&str>, hostname: Option<&str>, local: Option<&str>) -> Result<()> {
    let mut meta = load_metadata()?;
    let interactive = name.is_none() || (hostname.is_none() && local.is_none());

    if interactive {
        intro("Update a tunnel")?;
    }

    // --- Select tunnel ---
    let name = match name {
        Some(n) => n.to_string(),
        None => {
            if meta.tunnels.is_empty() {
                outro_cancel("No tunnels to update. Create one with `cftun create`.")?;
                return Ok(());
            }
            tunnel_select_prompt("Select a tunnel to update", &meta)?
        }
    };

    let t = meta.tunnels.get(&name).ok_or_else(|| {
        anyhow!(
            "tunnel '{}' not found. run `cftun list` to see available tunnels (or import it with `cftun import <name> <hostname> <port/url>`)",
            name
        )
    })?;
    let old_hostname = t.hostname.clone();
    let old_service = t.service.clone();
    let uuid = t.uuid.clone();
    let config_path = t.config_path.clone();

    // --- New hostname ---
    let hostname = match hostname {
        Some(h) => Some(h.to_string()),
        None => {
            if interactive {
                let h: String = input("New hostname")
                    .placeholder("e.g. webhook.example.com")
                    .default_input(&old_hostname)
                    .interact()?;
                if h == old_hostname {
                    None
                } else {
                    Some(h)
                }
            } else {
                None
            }
        }
    };

    // --- New local service ---
    let local = match local {
        Some(l) => Some(l.to_string()),
        None => {
            if interactive {
                let l: String = input("New local service")
                    .placeholder("e.g. 3000 or http://localhost:3000")
                    .default_input(&old_service)
                    .autocomplete(vec![
                        "3000".to_string(),
                        "8080".to_string(),
                        "8443".to_string(),
                        "http://localhost:3000".to_string(),
                        "http://localhost:8080".to_string(),
                    ])
                    .interact()?;
                if l == old_service {
                    None
                } else {
                    Some(l)
                }
            } else {
                None
            }
        }
    };

    // Validate
    if hostname.is_none() && local.is_none() {
        if interactive {
            outro_cancel("No changes to make.")?;
            return Ok(());
        } else {
            anyhow::bail!("provide at least one of --hostname or --local");
        }
    }

    let new_hostname = hostname.unwrap_or_else(|| old_hostname.clone());
    let new_service = local.as_deref().map(normalize_service).unwrap_or_else(|| old_service.clone());
    let hostname_changed = new_hostname != old_hostname;

    // --- Confirm ---
    if interactive {
        let yes = confirm(format!(
            "Update tunnel '{}'?  https://{} → {}",
            name, new_hostname, new_service
        ))
        .interact()?;
        if !yes {
            outro_cancel("Cancelled.")?;
            return Ok(());
        }
    }

    // --- Execute ---
    if interactive {
        if hostname_changed {
            let sp = spinner();
            sp.start(format!("Updating DNS route: {} → {}...", old_hostname, new_hostname));
            let _ = run_cloudflared(&["tunnel", "route", "dns", "delete", &old_hostname]);
            cloudflared_ok(&["tunnel", "route", "dns", &name, &new_hostname])?;
            sp.stop("DNS route updated");
        }

        let sp = spinner();
        sp.start("Writing config...");

        rewrite_config(&config_path, &uuid, &new_hostname, &new_service)?;
        let t = meta.tunnels.get_mut(&name).unwrap();
        t.hostname = new_hostname.clone();
        t.service = new_service.clone();
        save_metadata(&meta)?;

        sp.stop("Config updated");
        outro_note(
            format!("Tunnel '{}' updated!", name),
            format!("Public URL:   https://{}\nLocal target: {}", new_hostname, new_service),
        )?;
    } else {
        if hostname_changed {
            println!(
                "{} Updating DNS route: {} → {}...",
                "→".cyan(),
                old_hostname,
                new_hostname
            );
            let _ = run_cloudflared(&["tunnel", "route", "dns", "delete", &old_hostname]);
            cloudflared_ok(&["tunnel", "route", "dns", &name, &new_hostname])?;
        }

        rewrite_config(&config_path, &uuid, &new_hostname, &new_service)?;
        let t = meta.tunnels.get_mut(&name).unwrap();
        t.hostname = new_hostname.clone();
        t.service = new_service.clone();
        save_metadata(&meta)?;

        println!("{} Tunnel '{}' updated.", "✓".green(), name);
        println!(
            "  Public URL:   {}",
            format!("https://{}", new_hostname).green().underline()
        );
        println!("  Local target: {}", new_service.dimmed());
    }

    Ok(())
}

fn delete(name: Option<&str>, cleanup_arg: bool) -> Result<()> {
    let mut meta = load_metadata()?;
    let interactive = name.is_none();

    if interactive {
        intro("Delete a tunnel")?;
    }

    // --- Select tunnel ---
    let name = match name {
        Some(n) => n.to_string(),
        None => {
            if meta.tunnels.is_empty() {
                outro_cancel("No tunnels managed by cftun.")?;
                return Ok(());
            }
            tunnel_select_prompt("Select a tunnel to delete", &meta)?
        }
    };

    // Get tunnel info (don't remove yet — wait for confirmation in interactive mode)
    let t = meta.tunnels.get(&name).ok_or_else(|| {
        anyhow!(
            "tunnel '{}' not found in cftun metadata. run `cftun list` to see available tunnels (or import it with `cftun import {} <hostname> <port/url>`)",
            name,
            name
        )
    })?;
    let uuid = t.uuid.clone();
    let hostname = t.hostname.clone();
    let config_path = t.config_path.clone();

    // --- Determine cleanup mode ---
    let cleanup = if interactive {
        let choice = select("Cleanup mode")
            .item("local", "Remove from cftun only", "keep the tunnel in Cloudflare")
            .item(
                "full",
                "Full cleanup",
                "delete tunnel, DNS route, and config file",
            )
            .interact()?;

        let verb = if choice == "full" { "Delete" } else { "Remove" };
        let yes = confirm(format!("{verb} tunnel '{}'?", name)).interact()?;
        if !yes {
            outro_cancel("Cancelled.")?;
            return Ok(());
        }
        choice == "full"
    } else {
        cleanup_arg
    };

    // --- Execute ---
    meta.tunnels.remove(&name);

    if interactive {
        if cleanup {
            let sp = spinner();
            sp.start(format!("Deleting tunnel '{}' from Cloudflare...", name));
            let _ = run_cloudflared(&["tunnel", "delete", &uuid]);
            sp.stop("Deleted");

            sp.start(format!("Removing DNS route {}...", hostname));
            let _ = run_cloudflared(&["tunnel", "route", "dns", "delete", &hostname]);
            sp.stop("Removed");

            if config_path.exists() {
                sp.start("Removing config file...");
                fs::remove_file(&config_path)
                    .with_context(|| format!("deleting {}", config_path.display()))?;
                sp.stop("Removed");
            }

            save_metadata(&meta)?;
            outro(format!("Tunnel '{}' fully deleted.", name))?;
        } else {
            save_metadata(&meta)?;
            outro(format!(
                "Removed '{}' from cftun. Still exists in Cloudflare.",
                name
            ))?;
        }
    } else {
        if cleanup {
            println!("{} Deleting cloudflared tunnel '{}'...", "→".cyan(), name);
            let _ = run_cloudflared(&["tunnel", "delete", &uuid]); // ignore failures if already gone

            println!("{} Removing DNS route {}...", "→".cyan(), hostname);
            let _ = run_cloudflared(&["tunnel", "route", "dns", "delete", &hostname]);

            if config_path.exists() {
                fs::remove_file(&config_path)
                    .with_context(|| format!("deleting {}", config_path.display()))?;
            }
            save_metadata(&meta)?;
            println!("{} Tunnel '{}' removed.", "✓".green(), name);
        } else {
            save_metadata(&meta)?;
            println!(
                "{} Tunnel '{}' removed from cftun metadata. Use {} to also delete from Cloudflare.",
                "✓".green(),
                name,
                "--cleanup".cyan()
            );
        }
    }

    Ok(())
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
            let online = if t.connections.is_empty() {
                "offline".dimmed()
            } else {
                "online".green()
            };
            println!("  {} {} ({}) [{}]", "•".cyan(), t.name.bold(), t.id.dimmed(), online);
        }
        println!();
    }

    println!(
        "  Create one with: {}",
        "cftun create".cyan()
    );
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
        let status = if online {
            "online".green().bold()
        } else {
            "offline".dimmed()
        };
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
    let content =
        fs::read_to_string(&t.config_path).with_context(|| format!("reading {}", t.config_path.display()))?;
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
        .ok_or_else(|| {
            anyhow!(
                "no cloudflared tunnel named '{}' found. run `cftun list` to see existing tunnels",
                name
            )
        })?;

    // Re-route DNS to the new hostname
    println!("{} Routing DNS {} → {}...", "→".cyan(), hostname, name);
    let _ = run_cloudflared(&["tunnel", "route", "dns", "delete", hostname]);
    cloudflared_ok(&["tunnel", "route", "dns", name, hostname])?;

    let tm = write_tunnel_config(&mut meta, name, &existing.id, hostname, local)?;

    println!("{} Tunnel '{}' imported.", "✓".green(), name);
    println!(
        "  Public URL:   {}",
        format!("https://{}", tm.hostname).green().underline()
    );
    println!("  Local target: {}", tm.service.dimmed());
    println!("  Run it with:  {}", format!("cftun run {}", name).cyan());
    Ok(())
}

fn run(name: Option<&str>) -> Result<()> {
    let meta = load_metadata()?;
    let name = match name {
        Some(name) => name.to_string(),
        None => tunnel_select_prompt("Select a tunnel to run", &meta)?,
    };
    let t = meta.tunnels.get(&name).ok_or_else(|| {
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
