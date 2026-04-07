mod config;
mod crypto;
mod gossip;
mod io;
mod metrics;
mod mux;
mod node;
mod state;
mod subscribe;

use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rand::Rng;
use tokio_util::sync::CancellationToken;
use tracing::info;

use config::{Config, Service, ServiceGroup};
use gossip::messages::Message;
use state::SharedState;

/// Maximum time to wait for in-flight connections to drain on shutdown.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(name = "stouter", about = "Light-weight service discovery and tunneling")]
struct Cli {
    #[arg(short, long, global = true, default_value = "stouter.json")]
    config: String,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Add this host to the cluster
    Init,
    /// Show the status of the running daemon on this host
    Status,
    /// Manage services
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Add a service on this node
    Add {
        #[arg(long)]
        group: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        port: u16,
    },
    /// List all services
    List,
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
            let state = SharedState::new(cfg.clone(), cli.config.clone(), shutdown.clone());

            let metrics_handle = metrics::install();
            tokio::spawn(metrics::run_gauge_loop(state.clone()));

            let daemon = async {
                match cfg.mode {
                    config::Mode::Node => node::run_node(state.clone()).await,
                    config::Mode::Subscribe => subscribe::run_subscribe(state.clone(), metrics_handle).await,
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

            // Generate a random 32-byte local secret and hex-encode it.
            let mut secret_bytes = [0u8; 32];
            rand::thread_rng().fill(&mut secret_bytes);
            cfg.local_secret = hex::encode(secret_bytes);

            cfg.node_id = config::get_hostname();

            cfg.save(&cli.config)?;

            println!(
                "Node initialized. node_id={}, localSecret saved to {}",
                cfg.node_id, cli.config
            );

            // Announce our presence to any already-known peers.
            if !cfg.known_nodes.is_empty() {
                let state = SharedState::new(cfg.clone(), cli.config.clone(), CancellationToken::new());
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
        // status
        // -----------------------------------------------------------------
        Some(Commands::Status) => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            let bind = &cfg.bind;

            match tokio::net::TcpStream::connect(bind).await {
                Err(_) => {
                    println!("No daemon running on {bind}");
                }
                Ok(mut stream) => {
                    gossip::send_message(
                        &mut stream,
                        &Message::StatusRequest,
                        &cfg.cluster_secret,
                    )
                    .await?;

                    let msg = gossip::recv_message(&mut stream, &cfg.cluster_secret).await?;

                    match msg {
                        Message::StatusResponse {
                            mode,
                            node_id,
                            bind,
                            dynamic_config,
                            known_nodes,
                        } => {
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
                            for node in &known_nodes {
                                println!("  {:<20} {}", node.id, node.addr);
                            }
                        }
                        _ => {
                            println!("Unexpected response from daemon");
                        }
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
                },
        }) => {
            let mut cfg = Config::load(&cli.config).unwrap_or_default();

            let service = Service {
                name: name.clone(),
                node_id: cfg.node_id.clone(),
                node_port: port,
                domains: Vec::new(),
            };

            // Find or create the target service group.
            if let Some(g) = cfg
                .dynamic_config
                .service_groups
                .iter_mut()
                .find(|g| g.name == group_name)
            {
                if let Some(existing) = g.services.iter_mut().find(|s| s.name == name && s.node_id == cfg.node_id) {
                    existing.node_port = port;
                } else {
                    g.services.push(service);
                }
            } else {
                cfg.dynamic_config.service_groups.push(ServiceGroup {
                    name: group_name.clone(),
                    services: vec![service],
                });
            }

            cfg.dynamic_config.version += 1;
            cfg.save(&cli.config)?;

            let version = cfg.dynamic_config.version;
            let state = SharedState::new(cfg.clone(), cli.config.clone(), CancellationToken::new());
            gossip::broadcast_message(
                &state,
                Message::ConfigUpdate {
                    config: cfg.dynamic_config.clone(),
                },
            )
            .await;

            println!(
                "Service '{}' added to group '{}' (version {})",
                name, group_name, version
            );
        }

        // -----------------------------------------------------------------
        // service list
        // -----------------------------------------------------------------
        Some(Commands::Service {
            action: ServiceAction::List,
        }) => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            let mut found = false;
            for group in &cfg.dynamic_config.service_groups {
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
