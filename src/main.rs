mod config;
mod gossip;
mod io;
mod metrics;
mod mux;
mod node;
mod pki;
mod state;
mod subscribe;
mod tls;

use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing::info;

use config::{Config, Service};
use gossip::messages::{AdminErrCode, AdminOp, AdminResult, Message};
use state::SharedState;

/// Maximum time to wait for in-flight connections to drain on shutdown.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(name = "stouter", about = "Light-weight service discovery and tunneling")]
struct Cli {
    #[arg(short, long, global = true, default_value = "stouter.json")]
    config: String,
    /// Directory holding `ca.crt`, `node.crt`, `node.key` for the CLI's
    /// mTLS client identity. Used with `--remote`. Falls back to the local
    /// config's `tlsDir` when not set.
    #[arg(long, global = true)]
    cert_dir: Option<String>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Add this host to the cluster
    Init,
    /// Show the status of a running daemon (defaults to this host)
    Status {
        /// Address `host:port` of a remote daemon to query.
        #[arg(long)]
        remote: Option<String>,
    },
    /// Validate the configuration file
    Check,
    /// Manage services
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Manage TLS certificates
    Cert {
        #[command(subcommand)]
        action: CertAction,
    },
}

#[derive(Subcommand)]
enum CertAction {
    /// Issue a leaf certificate for a node, signed by the cluster CA
    Issue {
        /// Node ID to issue the certificate for
        #[arg(long)]
        node: String,
        /// Directory to write ca.crt, node.crt, and node.key into
        #[arg(long, default_value = ".")]
        out: String,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Add a service to a node
    Add {
        #[arg(long)]
        group: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        port: u16,
        /// Comma-separated list of domains to route to this service
        #[arg(long, value_delimiter = ',')]
        domains: Vec<String>,
        /// Node ID to register the service on (defaults to this node)
        #[arg(long)]
        node: Option<String>,
        /// Address `host:port` of a remote daemon to mutate via admin RPC.
        /// When set, no local config file is read or written.
        #[arg(long)]
        remote: Option<String>,
    },
    /// List all services
    List {
        /// Address `host:port` of a remote daemon to query for the live
        /// service list. When unset, reads the local config file.
        #[arg(long)]
        remote: Option<String>,
    },
    /// Rename a service within a group
    Rename {
        #[arg(long)]
        group: String,
        #[arg(long = "from")]
        from: String,
        #[arg(long = "to")]
        to: String,
        /// Node ID the service is registered on (defaults to this node)
        #[arg(long)]
        node: Option<String>,
        /// Address `host:port` of a remote daemon to mutate via admin RPC.
        #[arg(long)]
        remote: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize structured logging from RUST_LOG, defaulting to "info".
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        // -----------------------------------------------------------------
        // no sub-command → run daemon based on config mode
        // -----------------------------------------------------------------
        None => {
            let cfg = Config::load(&cli.config)?;
            let shutdown = CancellationToken::new();

            let tls_dir = cfg.tls_dir.as_deref().ok_or_else(|| {
                anyhow::anyhow!("tlsDir must be set in {}; mTLS is required", cli.config)
            })?;
            let material = pki::load_or_create(tls_dir, &cfg.node_id)?;
            let tls_ctx = tls::TlsContext::new(&material)?;

            let state = SharedState::new(cfg.clone(), cli.config.clone(), shutdown.clone(), Some(tls_ctx));

            let metrics_handle = metrics::install();
            tokio::spawn(metrics::run_gauge_loop(state.clone()));

            let daemon = async {
                match cfg.mode {
                    config::Mode::Node => node::run_node(state.clone()).await,
                    config::Mode::Subscribe | config::Mode::NodeSubscribe => {
                        subscribe::run_subscribe(state.clone(), metrics_handle).await
                    }
                }
            };

            tokio::select! {
                result = daemon => result?,
                _ = tokio::signal::ctrl_c() => {
                    graceful_drain(state).await;
                }
            }
        }

        // -----------------------------------------------------------------
        // init
        // -----------------------------------------------------------------
        Some(Commands::Init) => {
            let mut cfg = Config::load(&cli.config).unwrap_or_default();

            cfg.node_id = config::get_hostname();

            cfg.save(&cli.config)?;

            println!(
                "Node initialized. node_id={}, config saved to {}",
                cfg.node_id, cli.config
            );

            // Announce our presence to any already-known peers.
            if !cfg.known_nodes.is_empty() {
                let tls_ctx = if let Some(ref dir) = cfg.tls_dir {
                    let material = pki::load_or_create(dir, &cfg.node_id)?;
                    Some(tls::TlsContext::new(&material)?)
                } else {
                    None
                };
                let state = SharedState::new(cfg.clone(), cli.config.clone(), CancellationToken::new(), tls_ctx);
                gossip::broadcast_message(
                    &state,
                    Message::NodeJoin {
                        id: cfg.node_id.clone(),
                        addr: cfg.peer_addr().to_owned(),
                    },
                )
                .await;
            }
        }

        // -----------------------------------------------------------------
        // check
        // -----------------------------------------------------------------
        Some(Commands::Check) => {
            match Config::check(&cli.config) {
                Ok(_) => {
                    println!("{} is valid", cli.config);
                }
                Err(err) => {
                    eprintln!("{}: {:#}", cli.config, err);
                    std::process::exit(1);
                }
            }
        }

        // -----------------------------------------------------------------
        // status
        // -----------------------------------------------------------------
        Some(Commands::Status { remote }) => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            let target = remote.clone().unwrap_or_else(|| cfg.local_target());

            match tokio::net::TcpStream::connect(&target).await {
                Err(_) => {
                    println!("No daemon running on {target}");
                }
                Ok(tcp) => {
                    let tls_ctx = build_cli_tls(&cfg, cli.cert_dir.as_deref())?;
                    let mut stream = tls_ctx.connect(tcp).await?;

                    gossip::send_message(&mut stream, &Message::StatusRequest).await?;
                    let msg = gossip::recv_message(&mut stream).await?;

                    match msg {
                        Message::StatusResponse {
                            mode, node_id, bind, dynamic_config, known_nodes,
                        } => print_status(&mode, &node_id, &bind, &dynamic_config, &known_nodes),
                        _ => println!("Unexpected response from daemon"),
                    }
                }
            }
        }

        // -----------------------------------------------------------------
        // service add
        // -----------------------------------------------------------------
        Some(Commands::Service {
            action:
                ServiceAction::Add {
                    group: group_name,
                    name,
                    port,
                    domains,
                    node,
                    remote,
                },
        }) => {
            if let Some(target) = remote {
                let cfg = Config::load(&cli.config).unwrap_or_default();
                let owner_node_id = node.unwrap_or_else(|| cfg.node_id.clone());
                let version = run_admin_rpc(
                    &target,
                    AdminOp::ServiceAdd {
                        group: group_name.clone(),
                        name: name.clone(),
                        owner_node_id,
                        port,
                        domains: domains.clone(),
                    },
                    &cfg,
                    cli.cert_dir.as_deref(),
                )
                .await?;
                println!(
                    "Service '{}' added to group '{}' on {} (version {})",
                    name, group_name, target, version
                );
            } else {
                let mut cfg = Config::load(&cli.config).unwrap_or_default();
                let node_id = node.unwrap_or_else(|| cfg.node_id.clone());
                let service = Service {
                    name: name.clone(),
                    node_id,
                    node_port: port,
                    domains: domains.clone(),
                };
                let version = cfg.dynamic_config.upsert_service(&group_name, service);
                cfg.save(&cli.config)?;

                let tls_ctx = if let Some(ref dir) = cfg.tls_dir {
                    let material = pki::load_or_create(dir, &cfg.node_id)?;
                    Some(tls::TlsContext::new(&material)?)
                } else {
                    None
                };
                let state = SharedState::new(cfg.clone(), cli.config.clone(), CancellationToken::new(), tls_ctx);
                gossip::broadcast_message(
                    &state,
                    Message::ConfigUpdate { config: cfg.dynamic_config.clone() },
                )
                .await;

                println!(
                    "Service '{}' added to group '{}' (version {})",
                    name, group_name, version
                );
            }
        }

        // -----------------------------------------------------------------
        // service list
        // -----------------------------------------------------------------
        Some(Commands::Service {
            action: ServiceAction::List { remote },
        }) => {
            let dc = if let Some(target) = remote {
                let cfg = Config::load(&cli.config).unwrap_or_default();
                let tls_ctx = build_cli_tls(&cfg, cli.cert_dir.as_deref())?;
                let tcp = tokio::net::TcpStream::connect(&target)
                    .await
                    .with_context(|| format!("connect to {target}"))?;
                let mut stream = tls_ctx.connect(tcp).await?;
                gossip::send_message(&mut stream, &Message::StatusRequest).await?;
                match gossip::recv_message(&mut stream).await? {
                    Message::StatusResponse { dynamic_config, .. } => dynamic_config,
                    _ => anyhow::bail!("unexpected response from daemon"),
                }
            } else {
                let cfg = Config::load(&cli.config)?;
                cfg.dynamic_config
            };

            let mut found = false;
            for group in &dc.service_groups {
                for svc in &group.services {
                    println!(
                        "{}/{} -> node={} port={}",
                        group.name, svc.name, svc.node_id, svc.node_port
                    );
                    found = true;
                }
            }
            if !found {
                println!("No services configured");
            }
        }

        // -----------------------------------------------------------------
        // service rename
        // -----------------------------------------------------------------
        Some(Commands::Service {
            action:
                ServiceAction::Rename {
                    group: group_name,
                    from,
                    to,
                    node,
                    remote,
                },
        }) => {
            if let Some(target) = remote {
                let cfg = Config::load(&cli.config).unwrap_or_default();
                let owner_node_id = node.unwrap_or_else(|| cfg.node_id.clone());
                let version = run_admin_rpc(
                    &target,
                    AdminOp::ServiceRename {
                        group: group_name.clone(),
                        owner_node_id,
                        from: from.clone(),
                        to: to.clone(),
                    },
                    &cfg,
                    cli.cert_dir.as_deref(),
                )
                .await?;
                println!(
                    "Service '{}' renamed to '{}' in group '{}' on {} (version {})",
                    from, to, group_name, target, version
                );
            } else {
                let mut cfg = Config::load(&cli.config)?;
                let node_id = node.unwrap_or_else(|| cfg.node_id.clone());

                let version = cfg
                    .dynamic_config
                    .rename_service(&group_name, &node_id, &from, &to)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                cfg.save(&cli.config)?;

                let tls_ctx = if let Some(ref dir) = cfg.tls_dir {
                    let material = pki::load_or_create(dir, &cfg.node_id)?;
                    Some(tls::TlsContext::new(&material)?)
                } else {
                    None
                };
                let state = SharedState::new(cfg.clone(), cli.config.clone(), CancellationToken::new(), tls_ctx);
                gossip::broadcast_message(
                    &state,
                    Message::ConfigUpdate { config: cfg.dynamic_config.clone() },
                )
                .await;

                println!(
                    "Service '{}' renamed to '{}' in group '{}' (version {})",
                    from, to, group_name, version
                );
            }
        }

        // -----------------------------------------------------------------
        // cert issue
        // -----------------------------------------------------------------
        Some(Commands::Cert {
            action: CertAction::Issue { node, out },
        }) => {
            let cfg = Config::load(&cli.config)?;
            let tls_dir = cfg.tls_dir.as_deref().ok_or_else(|| {
                anyhow::anyhow!("tlsDir is not set in {}", cli.config)
            })?;
            pki::issue_cert_to_dir(tls_dir, &node, &out)?;
            println!("Certificate for '{}' written to {}/", node, out);
            println!("Copy ca.crt, node.crt, and node.key to the node's tlsDir.");
        }
    }

    Ok(())
}

/// Graceful shutdown: broadcast NodeLeave, stop accepting, drain in-flight.
async fn graceful_drain(state: std::sync::Arc<SharedState>) {
    info!("shutting down gracefully...");

    // Signal all listeners and background loops to stop.
    state.shutdown.cancel();

    // Broadcast NodeLeave so peers stop routing to us.
    if !state.config.node_id.is_empty() {
        info!("broadcasting NodeLeave for {}", state.config.node_id);
        gossip::broadcast_message(
            &state,
            Message::NodeLeave {
                id: state.config.node_id.clone(),
            },
        )
        .await;
    }

    // Wait for in-flight connections to finish.
    if state.in_flight.wait_zero(DRAIN_TIMEOUT).await {
        info!("all connections drained");
    } else {
        info!("drain timeout reached, shutting down with connections still active");
    }
}

/// Build a `TlsContext` for an outbound CLI connection, preferring an
/// explicit `--cert-dir` over the local config's `tlsDir`.
fn build_cli_tls(cfg: &Config, cert_dir: Option<&str>) -> Result<tls::TlsContext> {
    let dir = cert_dir
        .or(cfg.tls_dir.as_deref())
        .ok_or_else(|| anyhow::anyhow!(
            "TLS material not available: pass --cert-dir or set tlsDir in the local config"
        ))?;
    let node_id = if cfg.node_id.is_empty() { "stouter-cli" } else { cfg.node_id.as_str() };
    let material = pki::load_or_create(dir, node_id)?;
    tls::TlsContext::new(&material)
}

/// Send a single `AdminRequest` to `target` and decode the response.
/// On `AdminResult::Err`, exits the process with a code derived from
/// `AdminErrCode` so callers don't need to handle it.
async fn run_admin_rpc(
    target: &str,
    op: AdminOp,
    cfg: &Config,
    cert_dir: Option<&str>,
) -> Result<u64> {
    let tls_ctx = build_cli_tls(cfg, cert_dir)?;
    let tcp = tokio::net::TcpStream::connect(target)
        .await
        .with_context(|| format!("connect to {target}"))?;
    let mut stream = tls_ctx.connect(tcp).await?;

    gossip::send_message(&mut stream, &Message::AdminRequest { op }).await?;
    match gossip::recv_message(&mut stream).await? {
        Message::AdminResponse { result: AdminResult::Ok { new_version } } => Ok(new_version),
        Message::AdminResponse { result: AdminResult::Err { code, message } } => {
            eprintln!("error: {message}");
            let exit_code = match code {
                AdminErrCode::Unauthorized => 3,
                AdminErrCode::Validation | AdminErrCode::NotFound => 2,
                AdminErrCode::Internal => 1,
            };
            std::process::exit(exit_code);
        }
        _ => anyhow::bail!("unexpected response from daemon"),
    }
}

fn print_status(
    mode: &str,
    node_id: &str,
    bind: &str,
    dynamic_config: &config::DynamicConfig,
    known_nodes: &[config::NodeInfo],
) {
    println!("Daemon:  running ({mode} mode)");
    println!("Node ID: {node_id}");
    println!("Bind:    {bind}");
    println!("Config version: {}", dynamic_config.version);

    println!("\nServices:");
    let mut any = false;
    for group in &dynamic_config.service_groups {
        println!("  [{}]", group.name);
        for svc in &group.services {
            println!(
                "    {:<20} node={:<20} port={}",
                svc.name, svc.node_id, svc.node_port
            );
            any = true;
        }
    }
    if !any {
        println!("  (none)");
    }

    println!("\nKnown nodes:");
    if known_nodes.is_empty() {
        println!("  (none)");
    }
    for node in known_nodes {
        println!("  {:<20} {}", node.id, node.addr);
    }
}
