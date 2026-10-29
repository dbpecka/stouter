mod config;
mod gossip;
mod health;
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
    /// Manage L7 routes for HTTP services
    Route {
        #[command(subcommand)]
        action: RouteAction,
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
        /// Relative share of connections this instance receives among all
        /// instances of the service. `0` drains it.
        #[arg(long, default_value_t = config::DEFAULT_WEIGHT)]
        weight: u32,
        /// Maximum concurrent connections each subscriber opens to this
        /// instance. Unlimited when unset.
        #[arg(long)]
        max_connections: Option<u32>,
        /// How subscribers proxy the service: `tcp` (byte stream) or `http`
        /// (per-request balancing, timeouts, retries, logs and metrics).
        #[arg(long, value_enum, default_value_t = config::Protocol::Tcp)]
        protocol: config::Protocol,
        /// HTTP only: time allowed for this instance to send response headers.
        #[arg(long)]
        request_timeout_ms: Option<u64>,
        /// Path to GET when health-checking this instance (any non-2xx is a
        /// failure). Unset: TCP connect check only.
        #[arg(long)]
        health_path: Option<String>,
        /// Label for routes to select this instance by, as `key=value`.
        /// Repeatable.
        #[arg(long = "label", value_parser = parse_label)]
        labels: Vec<(String, String)>,
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
    /// Move a service from one cluster to another. Both `--from` and `--to`
    /// must point at remote daemons (one in each cluster). The service's
    /// port and domains are copied verbatim from the source.
    Move {
        #[arg(long)]
        group: String,
        #[arg(long)]
        name: String,
        /// Owner node ID in the source cluster (defaults to this node)
        #[arg(long)]
        node: Option<String>,
        /// Destination group name (defaults to `--group`)
        #[arg(long = "to-group")]
        to_group: Option<String>,
        /// Owner node ID in the destination cluster (defaults to `--node`)
        #[arg(long = "to-node")]
        to_node: Option<String>,
        /// Address `host:port` of a daemon in the source cluster.
        #[arg(long)]
        from: String,
        /// Address `host:port` of a daemon in the destination cluster.
        #[arg(long)]
        to: String,
        /// Cert directory (ca.crt + node.crt + node.key) trusted by the
        /// source cluster. Falls back to the global `--cert-dir`, then to
        /// the local config's `tlsDir`. The leaf's CN must be in the
        /// source cluster's `adminCns`.
        #[arg(long = "from-cert-dir")]
        from_cert_dir: Option<String>,
        /// Cert directory trusted by the destination cluster. Same
        /// fallback rules as `--from-cert-dir`. The leaf's CN must be in
        /// the destination cluster's `adminCns`.
        #[arg(long = "to-cert-dir")]
        to_cert_dir: Option<String>,
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
    /// Remove a service from a node
    Remove {
        #[arg(long)]
        group: String,
        #[arg(long)]
        name: String,
        /// Node ID the service is registered on (defaults to this node)
        #[arg(long)]
        node: Option<String>,
        /// Address `host:port` of a remote daemon to mutate via admin RPC.
        /// When set, no local config file is read or written.
        #[arg(long)]
        remote: Option<String>,
    },
}

#[derive(Subcommand)]
enum RouteAction {
    /// Replace all routes for a service
    Set {
        /// Service the routes apply to
        #[arg(long)]
        service: String,
        /// JSON array of routes, or `@path` to read it from a file. The
        /// `service` field of each route may be omitted.
        #[arg(long)]
        routes: String,
        /// Address `host:port` of a remote daemon to mutate via admin RPC.
        #[arg(long)]
        remote: Option<String>,
    },
    /// Remove all routes for a service
    Clear {
        #[arg(long)]
        service: String,
        #[arg(long)]
        remote: Option<String>,
    },
    /// List routes
    List {
        /// Address `host:port` of a remote daemon to query.
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
                    weight,
                    max_connections,
                    protocol,
                    request_timeout_ms,
                    health_path,
                    labels,
                    node,
                    remote,
                },
        }) => {
            if let Some(path) = &health_path {
                config::validate_health_path(path).map_err(anyhow::Error::msg)?;
            }
            let labels: std::collections::BTreeMap<String, String> = labels.into_iter().collect();
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
                        weight,
                        max_connections,
                        protocol,
                        request_timeout_ms,
                        health_path,
                        labels,
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
                    weight,
                    max_connections,
                    protocol,
                    request_timeout_ms,
                    health_path,
                    labels,
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
                fetch_remote_dynamic_config(&target, &cfg, cli.cert_dir.as_deref()).await?
            } else {
                let cfg = Config::load(&cli.config)?;
                cfg.dynamic_config
            };

            let mut found = false;
            for group in &dc.service_groups {
                for svc in &group.services {
                    let mut limit = svc
                        .max_connections
                        .map(|m| format!(" maxConnections={m}"))
                        .unwrap_or_default();
                    if !svc.protocol.is_tcp() {
                        limit.push_str(" protocol=http");
                    }
                    if let Some(ms) = svc.request_timeout_ms {
                        limit.push_str(&format!(" requestTimeoutMs={ms}"));
                    }
                    println!(
                        "{}/{} -> node={} port={} weight={}{}",
                        group.name, svc.name, svc.node_id, svc.node_port, svc.weight, limit
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
        // service remove
        // -----------------------------------------------------------------
        Some(Commands::Service {
            action:
                ServiceAction::Remove {
                    group: group_name,
                    name,
                    node,
                    remote,
                },
        }) => {
            if let Some(target) = remote {
                let cfg = Config::load(&cli.config).unwrap_or_default();
                let owner_node_id = node.unwrap_or_else(|| cfg.node_id.clone());
                let version = run_admin_rpc(
                    &target,
                    AdminOp::ServiceRemove {
                        group: group_name.clone(),
                        owner_node_id,
                        name: name.clone(),
                    },
                    &cfg,
                    cli.cert_dir.as_deref(),
                )
                .await?;
                println!(
                    "Service '{}' removed from group '{}' on {} (version {})",
                    name, group_name, target, version
                );
            } else {
                let mut cfg = Config::load(&cli.config)?;
                let node_id = node.unwrap_or_else(|| cfg.node_id.clone());

                let version = cfg
                    .dynamic_config
                    .remove_service(&group_name, &node_id, &name)
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
                    "Service '{}' removed from group '{}' (version {})",
                    name, group_name, version
                );
            }
        }

        // -----------------------------------------------------------------
        // route set / clear
        // -----------------------------------------------------------------
        Some(Commands::Route {
            action: action @ (RouteAction::Set { .. } | RouteAction::Clear { .. }),
        }) => {
            let (service, routes, remote) = match action {
                RouteAction::Set { service, routes, remote } => {
                    let routes = parse_routes(&service, &routes)?;
                    (service, routes, remote)
                }
                RouteAction::Clear { service, remote } => (service, Vec::new(), remote),
                RouteAction::List { .. } => unreachable!("matched above"),
            };
            let count = routes.len();
            let version = if let Some(target) = &remote {
                let cfg = Config::load(&cli.config).unwrap_or_default();
                run_admin_rpc(
                    target,
                    AdminOp::RoutesSet { service: service.clone(), routes },
                    &cfg,
                    cli.cert_dir.as_deref(),
                )
                .await?
            } else {
                let mut cfg = Config::load(&cli.config)?;
                let version = cfg
                    .dynamic_config
                    .set_routes(&service, routes)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                cfg.save(&cli.config)?;
                broadcast_config(&cfg, &cli.config).await?;
                version
            };
            println!("Set {count} route(s) for '{service}' (version {version})");
        }

        // -----------------------------------------------------------------
        // route list
        // -----------------------------------------------------------------
        Some(Commands::Route { action: RouteAction::List { remote } }) => {
            let dc = if let Some(target) = remote {
                let cfg = Config::load(&cli.config).unwrap_or_default();
                fetch_remote_dynamic_config(&target, &cfg, cli.cert_dir.as_deref()).await?
            } else {
                Config::load(&cli.config)?.dynamic_config
            };
            if dc.routes.is_empty() {
                println!("No routes configured");
            }
            for (i, route) in dc.routes.iter().enumerate() {
                let backends: Vec<String> = route
                    .backends
                    .iter()
                    .map(|b| {
                        let labels: Vec<String> = b.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
                        let labels = if labels.is_empty() { "*".to_string() } else { labels.join(",") };
                        format!("{labels} weight={}", b.weight)
                    })
                    .collect();
                println!(
                    "{} {:<10} match={} -> {}",
                    route.service,
                    route.label(i),
                    serde_json::to_string(&route.matches)?,
                    backends.join(" | ")
                );
            }
        }

        // -----------------------------------------------------------------
        // service move (cross-cluster)
        // -----------------------------------------------------------------
        Some(Commands::Service {
            action:
                ServiceAction::Move {
                    group: group_name,
                    name,
                    node,
                    to_group,
                    to_node,
                    from,
                    to,
                    from_cert_dir,
                    to_cert_dir,
                },
        }) => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            let owner_node_id = node.unwrap_or_else(|| cfg.node_id.clone());
            let dest_group = to_group.unwrap_or_else(|| group_name.clone());
            let dest_node = to_node.unwrap_or_else(|| owner_node_id.clone());

            // Each cluster has its own CA, so each side needs its own cert
            // dir. Fall back through: per-side flag → global --cert-dir →
            // local tlsDir.
            let from_certs = from_cert_dir.as_deref().or(cli.cert_dir.as_deref());
            let to_certs = to_cert_dir.as_deref().or(cli.cert_dir.as_deref());

            // 1. Read the service from the source cluster.
            let src_dc =
                fetch_remote_dynamic_config(&from, &cfg, from_certs).await?;
            let (port, domains, weight, max_connections, protocol, request_timeout_ms, health_path, labels) = src_dc
                .service_groups
                .iter()
                .find(|g| g.name == group_name)
                .and_then(|g| {
                    g.services
                        .iter()
                        .find(|s| s.name == name && s.node_id == owner_node_id)
                })
                .map(|s| {
                    (
                        s.node_port,
                        s.domains.clone(),
                        s.weight,
                        s.max_connections,
                        s.protocol,
                        s.request_timeout_ms,
                        s.health_path.clone(),
                        s.labels.clone(),
                    )
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "service '{}' on node '{}' not found in group '{}' on {}",
                        name,
                        owner_node_id,
                        group_name,
                        from
                    )
                })?;

            // 2. Add to the destination cluster.
            let add_version = match try_admin_rpc(
                &to,
                AdminOp::ServiceAdd {
                    group: dest_group.clone(),
                    name: name.clone(),
                    owner_node_id: dest_node.clone(),
                    port,
                    domains: domains.clone(),
                    weight,
                    max_connections,
                    protocol,
                    request_timeout_ms,
                    health_path,
                    labels,
                },
                &cfg,
                to_certs,
            )
            .await?
            {
                Ok(v) => v,
                Err((_, msg)) => anyhow::bail!("add to {to} failed: {msg}"),
            };

            // 3. Remove from the source cluster. If this fails, roll back
            //    the add so we don't end up with the service in both
            //    clusters.
            let remove_result = try_admin_rpc(
                &from,
                AdminOp::ServiceRemove {
                    group: group_name.clone(),
                    owner_node_id: owner_node_id.clone(),
                    name: name.clone(),
                },
                &cfg,
                from_certs,
            )
            .await;

            match remove_result {
                Ok(Ok(remove_version)) => {
                    println!(
                        "Moved '{}/{}' from {} (v{}) to {} (v{})",
                        group_name, name, from, remove_version, to, add_version
                    );
                }
                outcome => {
                    eprintln!(
                        "remove from {from} failed; rolling back add on {to}"
                    );
                    let rollback = try_admin_rpc(
                        &to,
                        AdminOp::ServiceRemove {
                            group: dest_group,
                            owner_node_id: dest_node,
                            name: name.clone(),
                        },
                        &cfg,
                        to_certs,
                    )
                    .await;
                    if let Err(e) = rollback {
                        eprintln!(
                            "rollback failed: service '{name}' may now exist in both clusters: {e:#}"
                        );
                    } else if let Ok(Err((_, msg))) = rollback {
                        eprintln!(
                            "rollback failed: service '{name}' may now exist in both clusters: {msg}"
                        );
                    }
                    match outcome {
                        Ok(Err((_, msg))) => anyhow::bail!("move failed: {msg}"),
                        Err(e) => return Err(e.context("move failed")),
                        Ok(Ok(_)) => unreachable!(),
                    }
                }
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
/// Returns the typed admin result; transport-level failures bubble up as
/// `anyhow::Error`.
async fn try_admin_rpc(
    target: &str,
    op: AdminOp,
    cfg: &Config,
    cert_dir: Option<&str>,
) -> Result<std::result::Result<u64, (AdminErrCode, String)>> {
    let tls_ctx = build_cli_tls(cfg, cert_dir)?;
    let tcp = tokio::net::TcpStream::connect(target)
        .await
        .with_context(|| format!("connect to {target}"))?;
    let mut stream = tls_ctx.connect(tcp).await?;

    gossip::send_message(&mut stream, &Message::AdminRequest { op }).await?;
    match gossip::recv_message(&mut stream).await? {
        Message::AdminResponse { result: AdminResult::Ok { new_version } } => Ok(Ok(new_version)),
        Message::AdminResponse { result: AdminResult::Err { code, message } } => {
            Ok(Err((code, message)))
        }
        _ => anyhow::bail!("unexpected response from daemon"),
    }
}

/// Like `try_admin_rpc`, but on `AdminResult::Err` exits the process with a
/// code derived from `AdminErrCode` so single-step callers don't need to
/// handle it.
async fn run_admin_rpc(
    target: &str,
    op: AdminOp,
    cfg: &Config,
    cert_dir: Option<&str>,
) -> Result<u64> {
    match try_admin_rpc(target, op, cfg, cert_dir).await? {
        Ok(new_version) => Ok(new_version),
        Err((code, message)) => {
            eprintln!("error: {message}");
            let exit_code = match code {
                AdminErrCode::Unauthorized => 3,
                AdminErrCode::Validation | AdminErrCode::NotFound => 2,
                AdminErrCode::Internal => 1,
            };
            std::process::exit(exit_code);
        }
    }
}

/// Fetch the `DynamicConfig` from a remote daemon via `StatusRequest`.
async fn fetch_remote_dynamic_config(
    target: &str,
    cfg: &Config,
    cert_dir: Option<&str>,
) -> Result<config::DynamicConfig> {
    let tls_ctx = build_cli_tls(cfg, cert_dir)?;
    let tcp = tokio::net::TcpStream::connect(target)
        .await
        .with_context(|| format!("connect to {target}"))?;
    let mut stream = tls_ctx.connect(tcp).await?;
    gossip::send_message(&mut stream, &Message::StatusRequest).await?;
    match gossip::recv_message(&mut stream).await? {
        Message::StatusResponse { dynamic_config, .. } => Ok(dynamic_config),
        _ => anyhow::bail!("unexpected response from {target}"),
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
            let unhealthy = known_nodes
                .iter()
                .any(|n| n.id == svc.node_id && n.health.is_unhealthy(&svc.name));
            let line = format!(
                "    {:<20} node={:<20} port={:<6} {}",
                svc.name,
                svc.node_id,
                svc.node_port,
                if unhealthy { "UNHEALTHY" } else { "" }
            );
            println!("{}", line.trim_end());
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
        if node.health.unhealthy.is_empty() {
            println!("  {:<20} {}", node.id, node.addr);
        } else {
            println!(
                "  {:<20} {:<22} unhealthy: {}",
                node.id,
                node.addr,
                node.health.unhealthy.join(", ")
            );
        }
    }
}

/// Parse a `key=value` label argument.
fn parse_label(arg: &str) -> std::result::Result<(String, String), String> {
    match arg.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("label '{arg}' must look like key=value")),
    }
}

/// Parse `--routes` (inline JSON or `@file`) for `service`. Each route's
/// `service` may be omitted and defaults to `service`.
fn parse_routes(service: &str, arg: &str) -> Result<Vec<config::Route>> {
    let text = match arg.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("read {path}"))?,
        None => arg.to_string(),
    };
    let mut raw: Vec<serde_json::Value> = serde_json::from_str(&text).context("routes must be a JSON array")?;
    for route in &mut raw {
        if let Some(obj) = route.as_object_mut() {
            obj.entry("service").or_insert_with(|| service.into());
        }
    }
    let routes: Vec<config::Route> = serde_json::from_value(raw.into()).context("invalid route")?;
    config::validate_routes(service, &routes).map_err(anyhow::Error::msg)?;
    Ok(routes)
}

/// Push a locally changed config to the cluster.
async fn broadcast_config(cfg: &Config, path: &str) -> Result<()> {
    let tls_ctx = match &cfg.tls_dir {
        Some(dir) => Some(tls::TlsContext::new(&pki::load_or_create(dir, &cfg.node_id)?)?),
        None => None,
    };
    let state = SharedState::new(cfg.clone(), path.to_string(), CancellationToken::new(), tls_ctx);
    gossip::broadcast_message(&state, Message::ConfigUpdate { config: cfg.dynamic_config.clone() }).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_routes_fills_in_service_and_validates() {
        let routes = parse_routes("api", r#"[{"match": {"pathPrefix": "/v2"}, "backends": [{"labels": {"version": "v2"}}]}]"#).unwrap();
        assert_eq!(routes[0].service, "api");
        assert!(parse_routes("api", r#"[{"service": "web", "backends": [{}]}]"#).is_err());
        assert!(parse_routes("api", r#"{"not": "an array"}"#).is_err());
        assert!(parse_routes("api", r#"[{"backends": []}]"#).is_err());
    }

    #[test]
    fn parse_label_requires_key() {
        assert_eq!(parse_label("version=v2").unwrap(), ("version".into(), "v2".into()));
        assert_eq!(parse_label("empty=").unwrap(), ("empty".into(), String::new()));
        assert!(parse_label("=v2").is_err());
        assert!(parse_label("novalue").is_err());
    }
}
