mod authz;
mod config;
mod gossip;
mod health;
mod history;
mod io;
mod metrics;
mod mux;
mod node;
mod pki;
mod renew;
mod signing;
mod state;
mod subscribe;
mod telemetry;
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
    /// Manage which nodes may reach which services
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
    },
    /// Manage TLS certificates
    Cert {
        #[command(subcommand)]
        action: CertAction,
    },
    /// Manage the cluster's dynamic config as a whole
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Sign the dynamic config in the local config file with your (admin)
    /// certificate and push it to a daemon, replacing the cluster's. How to
    /// apply a hand edit where `requireSignedConfig` is on.
    ///
    /// Refused if the cluster's config changed since the version the file
    /// holds, as pushing would undo those changes: `config pull` first.
    /// The file is then updated to the version pushed.
    Push {
        /// Address `host:port` of the daemon to push to (defaults to this
        /// host's daemon).
        #[arg(long)]
        remote: Option<String>,
        /// Push even though the cluster's config is newer than the file's,
        /// replacing whatever changed since.
        #[arg(long)]
        force: bool,
    },
    /// List the versions of the dynamic config a daemon took (or, with no
    /// daemon, the local file's), with when, who and what changed
    History {
        /// Address `host:port` of the daemon to ask (defaults to this
        /// host's history file).
        #[arg(long)]
        remote: Option<String>,
        /// Print this version's config, as JSON.
        #[arg(long)]
        version: Option<u64>,
    },
    /// Put back the services, routes and policies of an earlier version
    /// (see `config history`), as a new version. Revocations stay as they are.
    Rollback {
        /// The version to go back to.
        #[arg(long)]
        to: u64,
        /// Address `host:port` of the daemon to make the change through
        /// (defaults to this host's daemon, else the config file).
        #[arg(long)]
        remote: Option<String>,
    },
    /// Write the cluster's current dynamic config into the local config
    /// file, keeping its other settings: where to start an edit to push.
    /// A file whose services or routes differ is first copied to `.bak`.
    Pull {
        /// Address `host:port` of the daemon to read from (defaults to this
        /// host's daemon).
        #[arg(long)]
        remote: Option<String>,
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
    /// Revoke certificates: no node accepts them any more, for connections
    /// or signatures, and connections already open with them fail
    Revoke {
        #[command(flatten)]
        which: RevokeTargets,
        /// Address `host:port` of the daemon to make the change through
        /// (defaults to this host's daemon, else the config file).
        #[arg(long)]
        remote: Option<String>,
    },
    /// Undo `cert revoke`
    Unrevoke {
        #[command(flatten)]
        which: RevokeTargets,
        /// Address `host:port` of the daemon to make the change through
        /// (defaults to this host's daemon, else the config file).
        #[arg(long)]
        remote: Option<String>,
    },
    /// List revoked certificates
    Revoked {
        /// Address `host:port` of a daemon to ask (defaults to the config file).
        #[arg(long)]
        remote: Option<String>,
    },
    /// Print a certificate file's SHA-256 fingerprint, as `cert revoke` takes it
    Fingerprint {
        /// Certificate file (PEM), such as a node's node.crt
        path: String,
    },
}

#[derive(Subcommand)]
enum PolicyAction {
    /// Let nodes open tunnels to a service. A service with no policy yet
    /// gets one, closing it to every other node (except those hosting it)
    Allow {
        #[arg(long)]
        service: String,
        /// A node that may reach it. Repeatable.
        #[arg(long = "from", required = true)]
        nodes: Vec<String>,
        /// Address `host:port` of the daemon to make the change through
        /// (defaults to this host's daemon, else the config file).
        #[arg(long)]
        remote: Option<String>,
    },
    /// Stop nodes opening tunnels to a service (the policy stays, even
    /// empty: `policy clear` opens the service again)
    Deny {
        #[arg(long)]
        service: String,
        /// A node that may no longer reach it. Repeatable.
        #[arg(long = "from", required = true)]
        nodes: Vec<String>,
        /// Address `host:port` of the daemon to make the change through.
        #[arg(long)]
        remote: Option<String>,
    },
    /// Remove a service's policy, opening it to every node again
    Clear {
        #[arg(long)]
        service: String,
        /// Address `host:port` of the daemon to make the change through.
        #[arg(long)]
        remote: Option<String>,
    },
    /// List policies
    List {
        /// Address `host:port` of a daemon to ask (defaults to the config file).
        #[arg(long)]
        remote: Option<String>,
    },
}

/// What `cert revoke` and `cert unrevoke` are about.
#[derive(clap::Args)]
struct RevokeTargets {
    /// Every certificate for this node name, issued before or after: for a
    /// node removed for good. Repeatable.
    #[arg(long = "node")]
    nodes: Vec<String>,
    /// This certificate file (PEM) only: for a key that leaked, where the
    /// node can be issued a new certificate. Repeatable.
    #[arg(long = "cert")]
    cert_files: Vec<String>,
    /// A certificate by its SHA-256 fingerprint (see `cert fingerprint`),
    /// instead of its file. Repeatable.
    #[arg(long = "fingerprint")]
    fingerprints: Vec<String>,
}

impl RevokeTargets {
    /// The node names, and the fingerprints of the certificates (from files
    /// or given), normalized.
    fn resolve(&self) -> Result<(Vec<String>, Vec<String>)> {
        if self.nodes.is_empty() && self.cert_files.is_empty() && self.fingerprints.is_empty() {
            anyhow::bail!("name what to revoke: --node NAME, --cert FILE or --fingerprint HEX");
        }
        let mut certs = Vec::new();
        for path in &self.cert_files {
            certs.push(tls::fingerprint_hex(pki::read_cert(path)?.as_ref()));
        }
        for fp in &self.fingerprints {
            certs.push(config::normalize_fingerprint(fp).map_err(anyhow::Error::msg)?);
        }
        Ok((self.nodes.clone(), certs))
    }
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
        #[arg(long = "label", value_parser = parse_key_value)]
        labels: Vec<(String, String)>,
        /// Metadata for integrations as `key=value`, e.g.
        /// `--meta traefik.scheme=h2c` for the Traefik plugin. Repeatable.
        #[arg(long = "meta", value_parser = parse_key_value)]
        meta: Vec<(String, String)>,
        /// Fixed `127.0.0.1` port subscribers open this service's proxy on
        /// (random when unset).
        #[arg(long)]
        local_port: Option<u16>,
        /// Node ID to register the service on (defaults to this node; with
        /// `--remote`, the node at that address)
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
        /// Owner node ID in the source cluster (defaults to the node at
        /// `--from`)
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
        /// Node ID the service is registered on (defaults to this node; with
        /// `--remote`, the node at that address)
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
        /// Node ID the service is registered on (defaults to this node; with
        /// `--remote`, the node at that address)
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
            let mut cfg = Config::load(&cli.config)?;
            // This node's own settings must be right to start. Problems in
            // the cluster-wide dynamic config are only reported: a newer
            // version may fix them, and a node that won't start over one
            // can't receive it.
            cfg.validate_local().map_err(|e| anyhow::anyhow!("{}: {e:#}", cli.config))?;
            for unknown in cfg.unknown_settings() {
                tracing::warn!("{}: {unknown}", cli.config);
            }
            for problem in cfg.config_problems() {
                tracing::warn!(
                    "{}: dynamic config version {}: {problem} (starting anyway: `stouter check` lists problems)",
                    cli.config,
                    cfg.dynamic_config.version
                );
            }
            let shutdown = CancellationToken::new();

            let tls_dir = cfg.tls_dir.as_deref().ok_or_else(|| {
                anyhow::anyhow!("tlsDir must be set in {}; mTLS is required", cli.config)
            })?;
            let material = pki::load_or_create_with(tls_dir, &cfg.node_id, cfg.cert_lifetime())?;
            let tls_ctx = tls::TlsContext::new(&material)?;
            tls_ctx.revocations().set(&cfg.dynamic_config.revoked);

            cfg.dynamic_config = startup_config(&cfg, &tls_ctx, &cli.config);
            if let Some(stamped) = stamp_offline_edit(&cfg, &history::load(&cli.config)) {
                tracing::info!(
                    "{}: edited while the daemon was stopped (version {} as it last ran, with other settings); \
                     taking it as version {}",
                    cli.config,
                    cfg.dynamic_config.version,
                    stamped.version
                );
                cfg.dynamic_config = stamped;
                if let Err(e) = cfg.save(&cli.config) {
                    tracing::warn!("can't write version {} to {}: {e:#}", cfg.dynamic_config.version, cli.config);
                }
            }

            let state = SharedState::new(cfg.clone(), cli.config.clone(), shutdown.clone(), Some(tls_ctx));
            // The version this node starts on begins (or continues) its history.
            history::record(&cli.config, &cfg.dynamic_config);
            // Signs this node's own record, which peers believe only signed.
            let _ = state.signer.set(signing::Signer::from_material(&material)?);
            // Orders this process's heartbeats above any earlier process's.
            let _ = state.incarnation.set(state::incarnation_for(&cli.config));
            // Holding the CA's key, this node renews certificates (its own
            // and other nodes'); every node renews its own in time.
            if let Some(issuer) = pki::Issuer::load(tls_dir, cfg.cert_lifetime())? {
                let _ = state.issuer.set(issuer);
            }
            tokio::spawn(renew::run_renewal_loop(state.clone()));

            if let Some(tracing_cfg) = &cfg.tracing {
                let _ = state.tracer.set(telemetry::Tracer::start(tracing_cfg, &cfg.node_id));
            }

            let metrics_handle = metrics::install();
            tokio::spawn(metrics::run_gauge_loop(state.clone()));
            tokio::spawn(state::run_pool_sweeper(state.clone()));

            let daemon = async {
                match cfg.mode {
                    config::Mode::Node => node::run_node(state.clone(), metrics_handle).await,
                    config::Mode::Subscribe | config::Mode::NodeSubscribe => {
                        subscribe::run_subscribe(state.clone(), metrics_handle).await
                    }
                }
            };

            tokio::select! {
                result = daemon => result?,
                signal = shutdown_signal() => {
                    info!("received {signal}");
                    graceful_drain(state).await;
                }
            }
        }

        // -----------------------------------------------------------------
        // init
        // -----------------------------------------------------------------
        Some(Commands::Init) => {
            let mut cfg = Config::load_or_default(&cli.config)?;

            // A node keeps the id it has: its certificate is issued for it,
            // and peers know it by it.
            if cfg.node_id.is_empty() {
                cfg.node_id = config::get_hostname();
            }

            cfg.save(&cli.config)?;

            println!(
                "Node initialized. node_id={}, config saved to {}",
                cfg.node_id, cli.config
            );

            // Announce our presence to any already-known peers.
            if !cfg.known_nodes.is_empty() {
                let tls_ctx = local_tls(&cfg)?;
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
            let cfg = Config::load_or_default(&cli.config)?;
            let target = remote.clone().unwrap_or_else(|| cfg.local_target());

            match tokio::time::timeout(state::CONNECT_TIMEOUT, tokio::net::TcpStream::connect(&target)).await {
                Err(_) => {
                    println!("No daemon answering on {target} (connect timed out)");
                }
                Ok(Err(_)) => {
                    println!("No daemon running on {target}");
                }
                Ok(Ok(tcp)) => {
                    let tls_ctx = build_cli_tls(&cfg, cli.cert_dir.as_deref())?;
                    let msg = tokio::time::timeout(RPC_TIMEOUT, async {
                        let mut stream = tls_ctx.connect(tcp).await?;
                        gossip::send_message(&mut stream, &Message::StatusRequest).await?;
                        gossip::recv_message(&mut stream).await
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("no answer from {target} within {}s", RPC_TIMEOUT.as_secs()))??;

                    match msg {
                        Message::StatusResponse {
                            mode, node_id, bind, dynamic_config, known_nodes, require_signed_config, cert_expires,
                            issues_certs,
                        } => {
                            print_status(&mode, &node_id, &bind, &dynamic_config, &known_nodes);
                            if let Some(until) = cert_expires {
                                println!(
                                    "\nCertificate valid until {} (renewed when a third of its life is left){}",
                                    history::format_time(until.max(0) as u64 * 1000),
                                    if issues_certs { "; this node holds the CA's key and renews others'" } else { "" }
                                );
                            }
                            if require_signed_config {
                                println!("\nConfig changes must be signed by an admin (requireSignedConfig).");
                            }
                        }
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
                    meta,
                    local_port,
                    node,
                    remote,
                },
        }) => {
            if let Some(path) = &health_path {
                config::validate_health_path(path).map_err(anyhow::Error::msg)?;
            }
            let labels: std::collections::BTreeMap<String, String> = labels.into_iter().collect();
            let meta: std::collections::BTreeMap<String, String> = meta.into_iter().collect();
            if let Some(target) = change_target(remote, &cli.config, cli.cert_dir.as_deref()).await? {
                let cfg = Config::load_or_default(&cli.config)?;
                let owner_node_id = owner_node(node, Some(&target), &cfg, cli.cert_dir.as_deref()).await?;
                let version = run_admin_rpc(
                    &target,
                    AdminOp::service_add(
                        group_name.clone(),
                        Service {
                            name: name.clone(),
                            node_id: owner_node_id.clone(),
                            node_port: port,
                            domains: domains.clone(),
                            weight,
                            max_connections,
                            protocol,
                            request_timeout_ms,
                            health_path,
                            labels,
                            meta,
                            local_port,
                        },
                    ),
                    &cfg,
                    cli.cert_dir.as_deref(),
                )
                .await?;
                println!(
                    "Service '{}' added to group '{}' for node '{}', through {} (version {})",
                    name, group_name, owner_node_id, target, version
                );
            } else {
                let mut cfg = Config::load_or_default(&cli.config)?;
                let node_id = owner_node(node, None, &cfg, cli.cert_dir.as_deref()).await?;
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
                    meta,
                    local_port,
                };
                let before = cfg.dynamic_config.clone();
                let version = cfg.dynamic_config.upsert_service(&group_name, service).map_err(|e| anyhow::anyhow!("{e}"))?;
                save_local_change(&mut cfg, &before, &cli.config, cli.cert_dir.as_deref())?;

                let tls_ctx = local_tls(&cfg)?;
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
                let cfg = Config::load_or_default(&cli.config)?;
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
                    if let Some(port) = svc.local_port {
                        limit.push_str(&format!(" localPort={port}"));
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
            if let Some(target) = change_target(remote, &cli.config, cli.cert_dir.as_deref()).await? {
                let cfg = Config::load_or_default(&cli.config)?;
                let owner_node_id = owner_node(node, Some(&target), &cfg, cli.cert_dir.as_deref()).await?;
                let version = run_admin_rpc(
                    &target,
                    AdminOp::ServiceRename {
                        group: group_name.clone(),
                        owner_node_id: owner_node_id.clone(),
                        from: from.clone(),
                        to: to.clone(),
                    },
                    &cfg,
                    cli.cert_dir.as_deref(),
                )
                .await?;
                println!(
                    "Service '{}' renamed to '{}' in group '{}' for node '{}', through {} (version {})",
                    from, to, group_name, owner_node_id, target, version
                );
            } else {
                let mut cfg = Config::load(&cli.config)?;
                let node_id = owner_node(node, None, &cfg, cli.cert_dir.as_deref()).await?;

                let before = cfg.dynamic_config.clone();
                let version = cfg
                    .dynamic_config
                    .rename_service(&group_name, &node_id, &from, &to)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                save_local_change(&mut cfg, &before, &cli.config, cli.cert_dir.as_deref())?;

                let tls_ctx = local_tls(&cfg)?;
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
            if let Some(target) = change_target(remote, &cli.config, cli.cert_dir.as_deref()).await? {
                let cfg = Config::load_or_default(&cli.config)?;
                let owner_node_id = owner_node(node, Some(&target), &cfg, cli.cert_dir.as_deref()).await?;
                let version = run_admin_rpc(
                    &target,
                    AdminOp::ServiceRemove {
                        group: group_name.clone(),
                        owner_node_id: owner_node_id.clone(),
                        name: name.clone(),
                    },
                    &cfg,
                    cli.cert_dir.as_deref(),
                )
                .await?;
                println!(
                    "Service '{}' removed from group '{}' for node '{}', through {} (version {})",
                    name, group_name, owner_node_id, target, version
                );
            } else {
                let mut cfg = Config::load(&cli.config)?;
                let node_id = owner_node(node, None, &cfg, cli.cert_dir.as_deref()).await?;

                let before = cfg.dynamic_config.clone();
                let version = cfg
                    .dynamic_config
                    .remove_service(&group_name, &node_id, &name)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                save_local_change(&mut cfg, &before, &cli.config, cli.cert_dir.as_deref())?;

                let tls_ctx = local_tls(&cfg)?;
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
            let remote = change_target(remote, &cli.config, cli.cert_dir.as_deref()).await?;
            let version = if let Some(target) = &remote {
                let cfg = Config::load_or_default(&cli.config)?;
                run_admin_rpc(
                    target,
                    AdminOp::RoutesSet { service: service.clone(), routes },
                    &cfg,
                    cli.cert_dir.as_deref(),
                )
                .await?
            } else {
                let mut cfg = Config::load(&cli.config)?;
                let before = cfg.dynamic_config.clone();
                let version = cfg
                    .dynamic_config
                    .set_routes(&service, routes)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                save_local_change(&mut cfg, &before, &cli.config, cli.cert_dir.as_deref())?;
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
                let cfg = Config::load_or_default(&cli.config)?;
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
            let cfg = Config::load_or_default(&cli.config)?;
            let from_certs = from_cert_dir.as_deref().or(cli.cert_dir.as_deref());
            let owner_node_id = owner_node(node, Some(&from), &cfg, from_certs).await?;
            let dest_group = to_group.unwrap_or_else(|| group_name.clone());
            let dest_node = to_node.unwrap_or_else(|| owner_node_id.clone());

            // Each cluster has its own CA, so each side needs its own cert
            // dir. Fall back through: per-side flag → global --cert-dir →
            // local tlsDir.
            let to_certs = to_cert_dir.as_deref().or(cli.cert_dir.as_deref());

            // 1. Read the service from the source cluster.
            let src_dc = fetch_trusted_config(&from, &cfg, from_certs).await?;
            let src_svc = src_dc
                .service_groups
                .iter()
                .find(|g| g.name == group_name)
                .and_then(|g| {
                    g.services
                        .iter()
                        .find(|s| s.name == name && s.node_id == owner_node_id)
                })
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "service '{}' on node '{}' not found in group '{}' on {}",
                        name,
                        owner_node_id,
                        group_name,
                        from
                    )
                })?;

            // 2. Add to the destination cluster, which mustn't have it yet:
            //    the add would replace that instance, and a rollback (below)
            //    would then delete it.
            let dest_dc = fetch_trusted_config(&to, &cfg, to_certs).await?;
            if let Some(existing) = dest_dc
                .service_groups
                .iter()
                .filter(|g| g.name == dest_group)
                .flat_map(|g| &g.services)
                .find(|s| s.name == name && s.node_id == dest_node)
            {
                anyhow::bail!(
                    "{to} already has '{dest_group}/{name}' on node '{dest_node}' (port {}); remove it there first, or \
                     move to another --to-node or --to-group",
                    existing.node_port
                );
            }
            let add_version = match try_admin_rpc(
                &to,
                AdminOp::service_add(dest_group.clone(), Service { node_id: dest_node.clone(), ..src_svc }),
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

            // No answer (a timeout, a dropped connection) leaves it open
            // whether the remove happened: look before rolling back, or the
            // service could end up in neither cluster.
            let remove_result = match remove_result {
                Err(e) => {
                    let still_there = fetch_trusted_config(&from, &cfg, from_certs).await.map(|dc| {
                        dc.service_groups
                            .iter()
                            .filter(|g| g.name == group_name)
                            .flat_map(|g| &g.services)
                            .any(|s| s.name == name && s.node_id == owner_node_id)
                    });
                    match still_there {
                        Ok(false) => Ok(Ok(None)),
                        Ok(true) => Err(e),
                        Err(check) => anyhow::bail!(
                            "move failed: no answer to the remove on {from} ({e:#}), and can't check whether it \
                             happened ({check:#}). The service is now on {to}; check {from} and remove it from one \
                             of the two"
                        ),
                    }
                }
                answered => answered.map(|result| result.map(Some)),
            };

            match remove_result {
                Ok(Ok(remove_version)) => {
                    // No version when the answer was lost but the remove happened.
                    let source = match remove_version {
                        Some(version) => format!("{from} (v{version})"),
                        None => from.clone(),
                    };
                    println!("Moved '{group_name}/{name}' from {source} to {to} (v{add_version})");
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
        // config push
        // -----------------------------------------------------------------
        Some(Commands::Config {
            action: ConfigAction::Push { remote, force },
        }) => {
            let cfg = Config::load(&cli.config)?;
            let target = remote.unwrap_or_else(|| cfg.local_target());
            let material = load_cli_material(&cfg, cli.cert_dir.as_deref())?;
            let tls_ctx = cli_tls_context(&cfg, &material)?;
            let mut desired = cfg.dynamic_config.clone();
            desired.signature = None;
            let mut pushed = None;
            for attempt in 1..=PUSH_ATTEMPTS {
                let daemon = fetch_daemon_config(&target, &tls_ctx).await?;
                let signed = signed_cluster(&cfg, daemon.require_signed_config);
                let reported = daemon.dynamic_config;
                // What the cluster has, as far as we can tell. A signed
                // cluster's config counts only as its signer made it; one
                // that can't be verified is about to be replaced anyway.
                let known = if signed && reported.signature.is_some() {
                    match trusted_config(&reported, &cfg, &material, &tls_ctx) {
                        Ok(verified) => Some(verified),
                        Err(e) => {
                            eprintln!("warning: can't verify the config on {target} ({e:#}); replacing it");
                            None
                        }
                    }
                } else if signed {
                    None
                } else {
                    Some(reported.clone())
                };
                if let Some(current) = known.as_ref().filter(|current| current.same_settings(&desired)) {
                    println!("{target} already has the config in {} (version {})", cli.config, current.version);
                    if let Some(aside) = config::kept_copies(&cli.config, "unapplied").first() {
                        println!(
                            "note: {aside} holds an edit a daemon set aside instead of writing over it; if that's \
                             the edit you meant, make it again in {} and push",
                            cli.config
                        );
                    }
                    return Ok(());
                }
                if let Some(refusal) = stale_push(&reported, &desired, &cli.config, force) {
                    anyhow::bail!("{refusal}");
                }
                // Above both the target's version and the file's: a target
                // behind the rest of the cluster (the file came from a node
                // that is ahead) would otherwise get a version the cluster
                // already has, and lose this change to it.
                let version = reported.version.max(desired.version).checked_add(1).with_context(|| {
                    format!("{} is at the highest version there is: set a lower version in it", cli.config)
                })?;
                let next = config::DynamicConfig { version, ..desired.clone() };
                // Refused only for problems the push adds: measured against
                // the verified config, or else what the target reports (as
                // the target itself will).
                let baseline = known.unwrap_or_else(|| reported.clone());
                if let Some(problem) = config::new_problem(&baseline, &next) {
                    anyhow::bail!("{problem}");
                }
                let push = signed_push(next, reported.version, &material)?;
                let AdminOp::ConfigPush { config: signed, .. } = &push else { unreachable!("signed_push makes a ConfigPush") };
                let signed = signed.clone();
                match send_admin(&target, &tls_ctx, push).await? {
                    Ok(version) => {
                        pushed = Some((version, signed));
                        break;
                    }
                    Err((AdminErrCode::Conflict, _)) if attempt < PUSH_ATTEMPTS => continue,
                    Err((_, message)) => anyhow::bail!("push to {target} failed: {message}"),
                }
            }
            let (version, signed) = pushed.expect("the last attempt breaks or bails");
            println!("Pushed the config in {} to {target} (version {version})", cli.config);
            // The file now holds what the cluster has, so the next edit
            // starts from (and is checked against) this version. Re-read,
            // in case a local daemon rewrote the file meanwhile.
            let saved = Config::load(&cli.config).and_then(|mut file| {
                file.dynamic_config = signed;
                file.save(&cli.config)
            });
            if let Err(e) = saved {
                eprintln!("warning: could not update {} to version {version}: {e:#}", cli.config);
            }
        }

        // -----------------------------------------------------------------
        // config pull
        // -----------------------------------------------------------------
        Some(Commands::Config {
            action: ConfigAction::Pull { remote },
        }) => {
            let mut cfg = Config::load(&cli.config)?;
            let target = remote.unwrap_or_else(|| cfg.local_target());
            let material = load_cli_material(&cfg, cli.cert_dir.as_deref())?;
            let tls_ctx = cli_tls_context(&cfg, &material)?;
            let daemon = fetch_daemon_config(&target, &tls_ctx).await?;
            let current = if !signed_cluster(&cfg, daemon.require_signed_config) {
                daemon.dynamic_config
            } else if daemon.dynamic_config.signature.is_none() {
                eprintln!(
                    "warning: the config on {target} isn't admin-signed yet; review it before you sign it with \
                     `stouter config push`"
                );
                daemon.dynamic_config
            } else {
                // Only what an admin signed: the file is what `config push`
                // signs next.
                trusted_config(&daemon.dynamic_config, &cfg, &material, &tls_ctx)
                    .with_context(|| format!("won't write the config from {target} to {}", cli.config))?
            };
            let version = current.version;
            if !cfg.dynamic_config.same_settings(&current) {
                // It may hold edits not pushed yet: keep them (never over
                // an earlier backup).
                let backup = config::unused_path(&format!("{}.bak", cli.config));
                std::fs::copy(&cli.config, &backup).with_context(|| format!("copy {} to {backup}", cli.config))?;
                println!("Saved the previous config (version {}) to {backup}", cfg.dynamic_config.version);
            }
            cfg.dynamic_config = current;
            cfg.save(&cli.config)?;
            println!("Wrote the config from {target} (version {version}) to {}", cli.config);
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
            pki::issue_cert_to_dir_with(tls_dir, &node, &out, cfg.cert_lifetime())?;
            let cert = pki::read_cert(&format!("{out}/node.crt"))?;
            println!("Certificate for '{}' written to {}/", node, out);
            println!("SHA-256 fingerprint: {}", tls::fingerprint_hex(cert.as_ref()));
            println!("Copy ca.crt, node.crt, and node.key to the node's tlsDir.");
            if cfg.dynamic_config.revoked.nodes.contains(&node) {
                eprintln!(
                    "warning: node name '{node}' is revoked, so no node accepts this certificate: \
                     `stouter cert unrevoke --node {node}` first"
                );
            }
        }

        // -----------------------------------------------------------------
        // cert revoke / unrevoke
        // -----------------------------------------------------------------
        Some(Commands::Cert { action: action @ (CertAction::Revoke { .. } | CertAction::Unrevoke { .. }) }) => {
            let (revoking, which, remote) = match action {
                CertAction::Revoke { which, remote } => (true, which, remote),
                CertAction::Unrevoke { which, remote } => (false, which, remote),
                _ => unreachable!("matched above"),
            };
            let (nodes, certs) = which.resolve()?;
            let local = Config::load_or_default(&cli.config)?;
            if revoking {
                refuse_revoking_own_cert(&local, cli.cert_dir.as_deref(), &nodes, &certs)?;
            }
            let op = if revoking {
                AdminOp::Revoke { nodes: nodes.clone(), certs: certs.clone() }
            } else {
                AdminOp::Unrevoke { nodes: nodes.clone(), certs: certs.clone() }
            };
            let remote = change_target(remote, &cli.config, cli.cert_dir.as_deref()).await?;
            let version = if let Some(target) = &remote {
                run_admin_rpc(target, op, &local, cli.cert_dir.as_deref()).await?
            } else {
                let mut cfg = Config::load(&cli.config)?;
                let before = cfg.dynamic_config.clone();
                let version = op.apply(&mut cfg.dynamic_config).map_err(|e| anyhow::anyhow!("{e}"))?;
                save_local_change(&mut cfg, &before, &cli.config, cli.cert_dir.as_deref())?;
                broadcast_config(&cfg, &cli.config).await?;
                version
            };
            let what: Vec<String> =
                nodes.iter().map(|n| format!("node '{n}'")).chain(certs.iter().map(|c| format!("certificate {c}"))).collect();
            let verb = if revoking { "Revoked" } else { "Unrevoked" };
            println!("{verb} {} (version {version})", what.join(", "));
        }

        // -----------------------------------------------------------------
        // config history
        // -----------------------------------------------------------------
        Some(Commands::Config { action: ConfigAction::History { remote, version } }) => {
            let local = Config::load_or_default(&cli.config)?;
            let entries = match &remote {
                Some(target) => fetch_history(target, version, &local, cli.cert_dir.as_deref()).await?,
                None => {
                    let mut entries = history::load(&cli.config);
                    if let Some(v) = version {
                        entries.retain(|e| e.version == v);
                    }
                    entries
                }
            };
            let from = remote.as_deref().unwrap_or(&cli.config);
            match version {
                Some(v) => {
                    let config = entries.into_iter().find_map(|e| e.config).ok_or_else(|| {
                        anyhow::anyhow!("version {v} isn't in the history of {from}: `stouter config history` lists them")
                    })?;
                    println!("{}", serde_json::to_string_pretty(&config)?);
                }
                None if entries.is_empty() => println!("No config history recorded for {from} yet"),
                None => {
                    for entry in entries.iter().rev() {
                        let who = match (&entry.signed_by, entry.changed_by.as_str()) {
                            (Some(admin), "") => format!(", signed by {admin}"),
                            (Some(admin), by) => format!(" by {by}, signed by {admin}"),
                            (None, "") => String::new(),
                            (None, by) => format!(" by {by}"),
                        };
                        println!("version {}  {}{who}", entry.version, history::format_time(entry.at_ms));
                        for change in &entry.changes {
                            println!("    {change}");
                        }
                    }
                }
            }
        }

        // -----------------------------------------------------------------
        // config rollback
        // -----------------------------------------------------------------
        Some(Commands::Config { action: ConfigAction::Rollback { to, remote } }) => {
            let local = Config::load_or_default(&cli.config)?;
            let target = change_target(remote, &cli.config, cli.cert_dir.as_deref()).await?;
            // The version to restore: from the daemon's history, or the file's.
            let entries = match &target {
                Some(addr) => fetch_history(addr, Some(to), &local, cli.cert_dir.as_deref()).await?,
                None => history::load(&cli.config),
            };
            let earlier = entries.into_iter().filter(|e| e.version == to).find_map(|e| e.config).ok_or_else(|| {
                anyhow::anyhow!("version {to} isn't in the config history: `stouter config history` lists them")
            })?;
            // Where changes must be signed, only what an admin signed is put
            // back: a daemon's history is its word, and signing it as given
            // would sign whatever it made up.
            let signed = match &target {
                Some(addr) => {
                    let tls_ctx = build_cli_tls(&local, cli.cert_dir.as_deref())?;
                    signed_cluster(&local, fetch_daemon_config(addr, &tls_ctx).await?.require_signed_config)
                }
                None => local.require_signed_config,
            };
            let earlier = if signed {
                if earlier.signature.is_none() {
                    anyhow::bail!(
                        "version {to} isn't admin-signed, so it can't be put back where changes must be signed: \
                         make the change by hand (`config pull`, edit, `config push`)"
                    );
                }
                let material = load_cli_material(&local, cli.cert_dir.as_deref())?;
                let tls_ctx = cli_tls_context(&local, &material)?;
                trusted_config(&earlier, &local, &material, &tls_ctx).with_context(|| format!("version {to}"))?
            } else {
                earlier
            };
            let op = AdminOp::Restore { config: earlier };
            let version = if let Some(addr) = &target {
                run_admin_rpc(addr, op, &local, cli.cert_dir.as_deref()).await?
            } else {
                let mut cfg = Config::load(&cli.config)?;
                let before = cfg.dynamic_config.clone();
                let version = op.apply(&mut cfg.dynamic_config).map_err(|e| anyhow::anyhow!("{e}"))?;
                save_local_change(&mut cfg, &before, &cli.config, cli.cert_dir.as_deref())?;
                broadcast_config(&cfg, &cli.config).await?;
                version
            };
            println!("Put back the services, routes and policies of version {to}, as version {version}");
        }

        // -----------------------------------------------------------------
        // policy allow / deny / clear
        // -----------------------------------------------------------------
        Some(Commands::Policy { action: action @ (PolicyAction::Allow { .. } | PolicyAction::Deny { .. } | PolicyAction::Clear { .. }) }) => {
            let (op, remote) = match action {
                PolicyAction::Allow { service, nodes, remote } => (AdminOp::PolicyAllow { service, nodes }, remote),
                PolicyAction::Deny { service, nodes, remote } => (AdminOp::PolicyDeny { service, nodes }, remote),
                PolicyAction::Clear { service, remote } => (AdminOp::PolicyClear { service }, remote),
                PolicyAction::List { .. } => unreachable!("matched above"),
            };
            let service = match &op {
                AdminOp::PolicyAllow { service, .. } | AdminOp::PolicyDeny { service, .. } | AdminOp::PolicyClear { service } => service.clone(),
                _ => unreachable!("policy ops only"),
            };
            let local = Config::load_or_default(&cli.config)?;
            let remote = change_target(remote, &cli.config, cli.cert_dir.as_deref()).await?;
            let (version, dc) = if let Some(target) = &remote {
                let version = run_admin_rpc(target, op, &local, cli.cert_dir.as_deref()).await?;
                (version, fetch_remote_dynamic_config(target, &local, cli.cert_dir.as_deref()).await.ok())
            } else {
                let mut cfg = Config::load(&cli.config)?;
                let before = cfg.dynamic_config.clone();
                let version = op.apply(&mut cfg.dynamic_config).map_err(|e| anyhow::anyhow!("{e}"))?;
                save_local_change(&mut cfg, &before, &cli.config, cli.cert_dir.as_deref())?;
                broadcast_config(&cfg, &cli.config).await?;
                (version, Some(cfg.dynamic_config))
            };
            match dc.as_ref().map(|dc| dc.policies.iter().find(|p| p.service == service)) {
                Some(Some(policy)) => println!("{} (version {version})", describe_policy(policy)),
                Some(None) => println!("Service '{service}' has no policy: every node may reach it (version {version})"),
                None => println!("Policy for '{service}' changed (version {version})"),
            }
            if dc.as_ref().is_some_and(|dc| !dc.service_groups.iter().flat_map(|g| &g.services).any(|s| s.name == service)) {
                eprintln!("note: no service named '{service}' is configured (yet)");
            }
        }

        // -----------------------------------------------------------------
        // policy list
        // -----------------------------------------------------------------
        Some(Commands::Policy { action: PolicyAction::List { remote } }) => {
            let dc = if let Some(target) = remote {
                let cfg = Config::load_or_default(&cli.config)?;
                fetch_remote_dynamic_config(&target, &cfg, cli.cert_dir.as_deref()).await?
            } else {
                Config::load(&cli.config)?.dynamic_config
            };
            if dc.policies.is_empty() {
                println!("No policies: every node may reach every service (version {})", dc.version);
            }
            for policy in &dc.policies {
                println!("{}", describe_policy(policy));
            }
        }

        // -----------------------------------------------------------------
        // cert revoked
        // -----------------------------------------------------------------
        Some(Commands::Cert { action: CertAction::Revoked { remote } }) => {
            let dc = if let Some(target) = remote {
                let cfg = Config::load_or_default(&cli.config)?;
                fetch_remote_dynamic_config(&target, &cfg, cli.cert_dir.as_deref()).await?
            } else {
                Config::load(&cli.config)?.dynamic_config
            };
            if dc.revoked.is_empty() {
                println!("No revoked certificates (version {})", dc.version);
            }
            for node in &dc.revoked.nodes {
                println!("node {node} (every certificate)");
            }
            for cert in &dc.revoked.certs {
                println!("certificate {cert}");
            }
        }

        // -----------------------------------------------------------------
        // cert fingerprint
        // -----------------------------------------------------------------
        Some(Commands::Cert { action: CertAction::Fingerprint { path } }) => {
            println!("{}", tls::fingerprint_hex(pki::read_cert(&path)?.as_ref()));
        }
    }

    Ok(())
}

/// The dynamic config a daemon starts with. With `requireSignedConfig`,
/// what an admin signed: the signed text, not whatever the file says around
/// the signature. Otherwise an edit to a signed config would be in force
/// here, and served to the CLI as the config to make (and sign) the next
/// change on. A config with no valid signature is kept, with a warning:
/// a cluster has one until its first signed change.
fn startup_config(cfg: &Config, tls_ctx: &tls::TlsContext, path: &str) -> config::DynamicConfig {
    if !cfg.require_signed_config {
        return cfg.dynamic_config.clone();
    }
    match signing::verify(&cfg.dynamic_config, tls_ctx, &cfg.admin_cns) {
        Ok((signed, _)) => {
            if signed != cfg.dynamic_config {
                tracing::warn!(
                    "{path}: ignoring edits to the admin-signed config (version {}) and running what was signed; \
                     apply an edit with `stouter config push` (requireSignedConfig)",
                    signed.version
                );
            }
            signed
        }
        Err(e) => {
            tracing::warn!(
                "starting with a config that isn't admin-signed ({e:#}); peers requiring signatures won't accept it \
                 until an admin pushes a signed change"
            );
            cfg.dynamic_config.clone()
        }
    }
}

/// The config file's config as a new version, if it was edited by hand
/// while the daemon was stopped: at the version the daemon last ran
/// (`history`'s newest entry) but saying something else. It gets the next
/// version and this node as its author, as the config poll gives an edit
/// made while the daemon runs. Taken as it was, it would meet the cluster's
/// config at the same version and be settled by the tie-break: reverted
/// with no trace, or spread with no new version.
///
/// Not with `requireSignedConfig`, where an edit isn't taken at all (see
/// [`startup_config`]); nor without a history to compare with.
fn stamp_offline_edit(cfg: &Config, history: &[history::HistoryEntry]) -> Option<config::DynamicConfig> {
    if cfg.require_signed_config {
        return None;
    }
    let ran = history.last()?.config.as_ref()?;
    let dc = &cfg.dynamic_config;
    if dc.version != ran.version || dc.same_settings(ran) {
        return None;
    }
    let version = match dc.next_version() {
        Ok(version) => version,
        Err(e) => {
            tracing::warn!("the config file was edited while the daemon was stopped, but {e}");
            return None;
        }
    };
    Some(config::DynamicConfig {
        version,
        changed_by: cfg.node_id.clone(),
        signed_at: None,
        signature: None,
        ..dc.clone()
    })
}

/// Resolve when the process is asked to stop: SIGINT (Ctrl-C) or, on Unix,
/// SIGTERM, which is what `docker stop`, systemd and Kubernetes send. Returns
/// the signal's name.
///
/// Without a SIGTERM handler the process dies at once on SIGTERM, or, as PID
/// 1 in a container, ignores it until it is SIGKILLed; either way peers get
/// no NodeLeave and in-flight connections are cut.
async fn shutdown_signal() -> &'static str {
    let interrupt = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!("cannot listen for SIGINT: {e}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(e) => {
                tracing::warn!("cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => "SIGINT",
        () = terminate => "SIGTERM",
    }
}

/// Graceful shutdown: broadcast NodeLeave, stop accepting, drain in-flight.
async fn graceful_drain(state: std::sync::Arc<SharedState>) {
    info!("shutting down gracefully...");

    // Signal all listeners and background loops to stop.
    state.shutdown.cancel();

    // Broadcast NodeLeave so peers stop routing to us.
    if !state.config.node_id.is_empty() {
        info!("broadcasting NodeLeave for {}", state.config.node_id);
        // Our record now says we left; gossip relays it to nodes the leave
        // message doesn't reach.
        let (heartbeat, record) = gossip::final_record(&state);
        gossip::broadcast_message(
            &state,
            Message::NodeLeave { id: state.config.node_id.clone(), heartbeat, record },
        )
        .await;
    }

    // Wait for in-flight connections to finish.
    if state.in_flight.wait_zero(DRAIN_TIMEOUT).await {
        info!("all connections drained");
    } else {
        info!("drain timeout reached, shutting down with connections still active");
    }

    // Export spans still queued, so the last requests' traces aren't lost.
    if let Some(tracer) = state.tracer.get() {
        tracer.flush(Duration::from_secs(5)).await;
    }
}

/// Build a `TlsContext` for an outbound CLI connection, preferring an
/// explicit `--cert-dir` over the local config's `tlsDir`.
/// The CLI's own TLS identity: from `--cert-dir`, else the local config's
/// `tlsDir`. Also what it signs config changes with.
fn load_cli_material(cfg: &Config, cert_dir: Option<&str>) -> Result<pki::PkiMaterial> {
    let (dir, from) = match (cert_dir, cfg.tls_dir.as_deref()) {
        (Some(dir), _) => (dir, "--cert-dir"),
        (None, Some(dir)) => (dir, "tlsDir in the local config"),
        (None, None) => anyhow::bail!("TLS material not available: pass --cert-dir or set tlsDir in the local config"),
    };
    let node_id = if cfg.node_id.is_empty() { "stouter-cli" } else { cfg.node_id.as_str() };
    pki::load_existing(dir, node_id).with_context(|| format!("the CLI's certificate (from {from})"))
}

/// Build a `TlsContext` for an outbound CLI connection, preferring an
/// explicit `--cert-dir` over the local config's `tlsDir`.
fn build_cli_tls(cfg: &Config, cert_dir: Option<&str>) -> Result<tls::TlsContext> {
    cli_tls_context(cfg, &load_cli_material(cfg, cert_dir)?)
}

/// The CLI's TLS context for `material`, refusing the certificates the
/// local config revokes (as a daemon refuses those its config does): for
/// connecting, and for checking who signed a config a daemon serves. With
/// none loaded, a config forged with a revoked admin's key, served by a
/// lying daemon, would pass, and the admin's change on top of it be signed.
fn cli_tls_context(cfg: &Config, material: &pki::PkiMaterial) -> Result<tls::TlsContext> {
    let tls_ctx = tls::TlsContext::new(material)?;
    tls_ctx.revocations().set(&cfg.dynamic_config.revoked);
    Ok(tls_ctx)
}

/// Open an mTLS connection to a daemon, giving up after `CONNECT_TIMEOUT`.
async fn connect_daemon(target: &str, tls_ctx: &tls::TlsContext) -> Result<tls::MeshStream> {
    tokio::time::timeout(state::CONNECT_TIMEOUT, async {
        let tcp = tokio::net::TcpStream::connect(target)
            .await
            .with_context(|| format!("connect to {target}"))?;
        tls_ctx.connect(tcp).await
    })
    .await
    .map_err(|_| anyhow::anyhow!("connect to {target} timed out"))?
}

/// What a daemon's `StatusRequest` says about its config.
struct DaemonConfig {
    dynamic_config: config::DynamicConfig,
    require_signed_config: bool,
}

/// Time allowed for a daemon to answer a CLI request (an admin change
/// waits for the daemon to tell its peers).
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Send `request` to a daemon and read its answer, within [`RPC_TIMEOUT`].
async fn daemon_request(target: &str, tls_ctx: &tls::TlsContext, request: &Message) -> Result<Message> {
    let mut stream = connect_daemon(target, tls_ctx).await?;
    tokio::time::timeout(RPC_TIMEOUT, async {
        gossip::send_message(&mut stream, request).await?;
        gossip::recv_message(&mut stream).await
    })
    .await
    .map_err(|_| anyhow::anyhow!("no answer from {target} within {}s", RPC_TIMEOUT.as_secs()))?
}

async fn fetch_daemon_config(target: &str, tls_ctx: &tls::TlsContext) -> Result<DaemonConfig> {
    match daemon_request(target, tls_ctx, &Message::StatusRequest).await? {
        Message::StatusResponse { dynamic_config, require_signed_config, .. } => {
            Ok(DaemonConfig { dynamic_config, require_signed_config })
        }
        _ => anyhow::bail!("unexpected response from {target}"),
    }
}

/// Send one `AdminRequest` and decode the typed result.
async fn send_admin(
    target: &str,
    tls_ctx: &tls::TlsContext,
    op: AdminOp,
) -> Result<std::result::Result<u64, (AdminErrCode, String)>> {
    match daemon_request(target, tls_ctx, &Message::AdminRequest { op }).await? {
        Message::AdminResponse { result: AdminResult::Ok { new_version } } => Ok(Ok(new_version)),
        Message::AdminResponse { result: AdminResult::Err { code, message } } => Ok(Err((code, message))),
        _ => anyhow::bail!("unexpected response from daemon"),
    }
}

/// Why pushing `desired` (the file's config, `path`) over `current` (the
/// cluster's) must not go ahead: the cluster's changed since the version
/// the file holds, and pushing would silently undo those changes.
fn stale_push(current: &config::DynamicConfig, desired: &config::DynamicConfig, path: &str, force: bool) -> Option<String> {
    (current.version > desired.version && !force).then(|| {
        format!(
            "the cluster's config is at version {}, but {path} holds version {}: pushing it would undo the changes made \
             since. Run `stouter config pull` (it keeps a copy of the file) and make your edit on top, or pass --force \
             to replace the cluster's config anyway",
            current.version, desired.version
        )
    })
}

/// Times a signed change is re-made after the config changed under it.
const PUSH_ATTEMPTS: usize = 3;

/// `next`, signed with the CLI's certificate, as a push onto `base_version`.
fn signed_push(mut next: config::DynamicConfig, base_version: u64, material: &pki::PkiMaterial) -> Result<AdminOp> {
    next.changed_by = tls::cert_cn(&material.node_cert_der).unwrap_or_else(|| "stouter-cli".into());
    let config = signing::sign(&next, &signing::Signer::from_material(material)?)?;
    Ok(AdminOp::ConfigPush { config, base_version })
}

/// Make the change `op` on the daemon at `target`. Returns the typed admin
/// result; transport-level failures bubble up as `anyhow::Error`.
///
/// A daemon with `requireSignedConfig` only takes signed changes: the CLI
/// then reads its config, applies `op` itself, signs the result and pushes
/// it, re-doing that if the config changed meanwhile.
async fn try_admin_rpc(
    target: &str,
    op: AdminOp,
    cfg: &Config,
    cert_dir: Option<&str>,
) -> Result<std::result::Result<u64, (AdminErrCode, String)>> {
    let material = load_cli_material(cfg, cert_dir)?;
    let tls_ctx = cli_tls_context(cfg, &material)?;
    for attempt in 1..=PUSH_ATTEMPTS {
        let daemon = fetch_daemon_config(target, &tls_ctx).await?;
        if !signed_cluster(cfg, daemon.require_signed_config) {
            return send_admin(target, &tls_ctx, op).await;
        }
        // Build only on what an admin signed: signing whatever the daemon
        // reports would let a node get its own edits admin-signed.
        if daemon.dynamic_config.signature.is_none() {
            anyhow::bail!(
                "the config on {target} (version {}) isn't admin-signed yet, so there is no verified config to make \
                 this change on: sign one first with `stouter config pull` and `stouter config push`",
                daemon.dynamic_config.version
            );
        }
        let current = trusted_config(&daemon.dynamic_config, cfg, &material, &tls_ctx)
            .with_context(|| format!("the config on {target}"))?;
        let mut next = current.clone();
        if let Err(e) = op.clone().apply(&mut next) {
            return Ok(Err(((&e).into(), e.to_string())));
        }
        if let Some(problem) = config::new_problem(&current, &next) {
            return Ok(Err((AdminErrCode::Validation, problem)));
        }
        let push = signed_push(next, current.version, &material)?;
        match send_admin(target, &tls_ctx, push).await? {
            Err((AdminErrCode::Conflict, _)) if attempt < PUSH_ATTEMPTS => continue,
            result => return Ok(result),
        }
    }
    unreachable!("the last attempt always returns")
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
                AdminErrCode::Validation | AdminErrCode::NotFound | AdminErrCode::Conflict => 2,
                AdminErrCode::Internal => 1,
            };
            std::process::exit(exit_code);
        }
    }
}

/// `dc` (a config a daemon reported) as its admin signed it: decoded from the
/// signed text, so edits made around the signature don't count, and signed
/// by someone the CLI trusts itself (a CN in the local config's `adminCns`,
/// or the CLI's own certificate). The daemon's word for who its admins are
/// can't be taken: a node lying about the config would lie about that too.
fn trusted_config(
    dc: &config::DynamicConfig,
    cfg: &Config,
    material: &pki::PkiMaterial,
    tls_ctx: &tls::TlsContext,
) -> Result<config::DynamicConfig> {
    let mut trusted = cfg.admin_cns.clone();
    trusted.extend(tls::cert_cn(&material.node_cert_der));
    signing::verify(dc, tls_ctx, &trusted).map(|(verified, _)| verified).map_err(|e| {
        e.context(format!(
            "not verified as admin-signed; to trust another admin's changes, list their certificate's CN in adminCns in \
             the CLI's config ({})",
            if cfg.admin_cns.is_empty() { "none listed".to_string() } else { cfg.admin_cns.join(", ") }
        ))
    })
}

/// A daemon's config history: every version (without configs), or one.
async fn fetch_history(
    target: &str,
    version: Option<u64>,
    cfg: &Config,
    cert_dir: Option<&str>,
) -> Result<Vec<history::HistoryEntry>> {
    let tls_ctx = build_cli_tls(cfg, cert_dir)?;
    match daemon_request(target, &tls_ctx, &Message::HistoryRequest { version }).await? {
        Message::HistoryResponse { entries } => Ok(entries),
        _ => anyhow::bail!("unexpected response from {target} (a daemon too old to keep a config history?)"),
    }
}

/// Who signed `dc` and when, and when their certificate expires (as the
/// signature says; checked by daemons when they took it).
fn describe_signature(dc: &config::DynamicConfig) -> Option<String> {
    let sig = dc.signature.as_ref()?;
    let der: Vec<u8> = (0..sig.signer_cert.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(sig.signer_cert.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    let signer = tls::cert_cn(&der.clone().into()).unwrap_or_else(|| "?".into());
    let when = dc.signed_at.map(|at| format!(" at {}", history::format_time(at * 1000))).unwrap_or_default();
    let cert = match pki::validity(&der) {
        Some((_, until)) if until * 1000 < now_ms() as i64 => format!(
            "; that certificate expired {} (what it signed while valid stays valid)",
            history::format_time(until.max(0) as u64 * 1000)
        ),
        Some((_, until)) => format!("; that certificate is valid until {}", history::format_time(until.max(0) as u64 * 1000)),
        None => String::new(),
    };
    Some(format!("{signer}{when}{cert}"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// One line saying who may reach a service under `policy`.
fn describe_policy(policy: &config::Policy) -> String {
    if policy.allow.is_empty() {
        format!("Service '{}' may be reached only by the nodes hosting it", policy.service)
    } else {
        format!("Service '{}' may be reached by {} (and the nodes hosting it)", policy.service, policy.allow.join(", "))
    }
}

/// Refuse revoking the certificate this CLI uses: it would lock the CLI out,
/// and in a signed cluster no node would take a config revoking its own
/// signer (another admin must revoke it).
fn refuse_revoking_own_cert(cfg: &Config, cert_dir: Option<&str>, nodes: &[String], certs: &[String]) -> Result<()> {
    let Ok(material) = load_cli_material(cfg, cert_dir) else { return Ok(()) };
    let cn = tls::cert_cn(&material.node_cert_der).unwrap_or_default();
    let fp = tls::fingerprint_hex(material.node_cert_der.as_ref());
    if nodes.contains(&cn) || certs.contains(&fp) {
        anyhow::bail!(
            "that revokes the certificate this CLI uses ('{cn}'): revoke it with another admin's certificate (--cert-dir)"
        );
    }
    Ok(())
}

/// Whether the CLI treats the cluster as requiring admin-signed configs: if
/// its own config says so, or the daemon does. Not the daemon's word alone:
/// a node lying about the config would say it needn't be signed, and have
/// the CLI take (and an admin then sign) whatever it serves.
fn signed_cluster(cfg: &Config, daemon_says: bool) -> bool {
    cfg.require_signed_config || daemon_says
}

/// A remote daemon's config, as far as the CLI can trust it (see
/// [`trusted_config`]) when it requires signed configs.
async fn fetch_trusted_config(target: &str, cfg: &Config, cert_dir: Option<&str>) -> Result<config::DynamicConfig> {
    let material = load_cli_material(cfg, cert_dir)?;
    let tls_ctx = cli_tls_context(cfg, &material)?;
    let daemon = fetch_daemon_config(target, &tls_ctx).await?;
    if !signed_cluster(cfg, daemon.require_signed_config) {
        return Ok(daemon.dynamic_config);
    }
    if daemon.dynamic_config.signature.is_none() {
        anyhow::bail!("the config on {target} isn't admin-signed yet (requireSignedConfig)");
    }
    trusted_config(&daemon.dynamic_config, cfg, &material, &tls_ctx).with_context(|| format!("the config on {target}"))
}

/// Where a config change goes: `--remote` if given; else the daemon running
/// on this host for this config, if one answers (so the change is made
/// where the config is in force, not in its file behind its back, where a
/// change the daemon makes at the same time could undo it); else `None`:
/// edit the file.
async fn change_target(remote: Option<String>, config_path: &str, cert_dir: Option<&str>) -> Result<Option<String>> {
    if remote.is_some() {
        return Ok(remote);
    }
    let cfg = Config::load_or_default(config_path)?;
    // A daemon with no listener (outboundOnly) can't be asked.
    if cfg.outbound_only || cfg.node_id.is_empty() {
        return Ok(None);
    }
    let target = cfg.local_target();
    let Ok(tls_ctx) = build_cli_tls(&cfg, cert_dir) else { return Ok(None) };
    let asked = tokio::time::timeout(Duration::from_secs(2), daemon_request(&target, &tls_ctx, &Message::StatusRequest));
    match asked.await {
        // A subscribe-mode daemon takes no admin requests (it hosts no
        // services, and its config is only ever what the cluster sends): a
        // change there is to the file, as with no daemon running.
        Ok(Ok(Message::StatusResponse { node_id, mode, .. })) if node_id == cfg.node_id && mode != "subscribe" => {
            Ok(Some(target))
        }
        _ => Ok(None),
    }
}

/// The node a service command is about: `--node` if given, else the node at
/// `remote` (it says, in its status) with `--remote`, else this host's
/// node. Never empty: a service on node "" is one no node hosts.
async fn owner_node(node: Option<String>, remote: Option<&str>, cfg: &Config, cert_dir: Option<&str>) -> Result<String> {
    if let Some(node) = node {
        return Ok(node);
    }
    let owner = match remote {
        // Who it is by its certificate, not by its word: a daemon claiming
        // another node's id would otherwise have the CLI change (and sign
        // a change to) that node's services.
        Some(target) => {
            let tls_ctx = build_cli_tls(cfg, cert_dir)?;
            let mut stream = connect_daemon(target, &tls_ctx).await?;
            let cn = stream.peer_cn().map(str::to_string);
            let reply = tokio::time::timeout(RPC_TIMEOUT, async {
                gossip::send_message(&mut stream, &Message::StatusRequest).await?;
                gossip::recv_message(&mut stream).await
            })
            .await
            .map_err(|_| anyhow::anyhow!("no answer from {target} within {}s", RPC_TIMEOUT.as_secs()))??;
            let Message::StatusResponse { node_id, .. } = reply else { anyhow::bail!("unexpected response from {target}") };
            match cn {
                Some(cn) if cn == node_id => node_id,
                cn => anyhow::bail!(
                    "{target} says it is node '{node_id}', but its certificate is for '{}': pass --node to say which node",
                    cn.as_deref().unwrap_or("no one")
                ),
            }
        }
        None => cfg.node_id.clone(),
    };
    if owner.is_empty() {
        anyhow::bail!("no node to put the service on: pass --node, or set nodeId in the local config");
    }
    Ok(owner)
}

/// Fetch the `DynamicConfig` from a remote daemon via `StatusRequest`.
async fn fetch_remote_dynamic_config(
    target: &str,
    cfg: &Config,
    cert_dir: Option<&str>,
) -> Result<config::DynamicConfig> {
    let tls_ctx = build_cli_tls(cfg, cert_dir)?;
    Ok(fetch_daemon_config(target, &tls_ctx).await?.dynamic_config)
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
    if let Some(signed) = describe_signature(dynamic_config) {
        println!("Signed by: {signed}");
    }

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

/// Parse a `key=value` argument (`--label`, `--meta`).
fn parse_key_value(arg: &str) -> std::result::Result<(String, String), String> {
    match arg.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("'{arg}' must look like key=value")),
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

/// Save a change the CLI made to the local config file: refused if it
/// introduces a config problem (a domain claimed twice, routes left pointing
/// at a removed service, ...), and stamped with who made it.
///
/// With `requireSignedConfig` the change is signed with the CLI's
/// certificate, which must be an admin's, or peers (and the local daemon)
/// would refuse it.
fn save_local_change(
    cfg: &mut Config,
    before: &config::DynamicConfig,
    path: &str,
    cert_dir: Option<&str>,
) -> Result<()> {
    if let Some(problem) = config::new_problem(before, &cfg.dynamic_config) {
        anyhow::bail!("{problem}");
    }
    // The file must still hold the config the change was made on: saving
    // over a newer one (a daemon's, a hand edit's) would undo it.
    let now = Config::load_or_default(path)?.dynamic_config;
    if now != *before {
        anyhow::bail!("{path} changed while this command ran (it now holds version {}); run it again", now.version);
    }
    if cfg.require_signed_config {
        let material = load_cli_material(cfg, cert_dir)?;
        let signer = tls::cert_cn(&material.node_cert_der).unwrap_or_default();
        if !cfg.admin_cns.contains(&signer) {
            anyhow::bail!(
                "this cluster accepts only signed config changes (requireSignedConfig), and the certificate \
                 in use ('{signer}') is not in adminCns: pass --cert-dir with an admin's certificate"
            );
        }
        // Only on what an admin signed: signing the file's config as it
        // stands would sign, along with this change, any edit made in the
        // file around its signature (one the daemon itself ignores).
        if before.signature.is_none() {
            anyhow::bail!(
                "the config in {path} isn't admin-signed yet, so there is no verified config to make this change on: \
                 sign one first with `stouter config push`"
            );
        }
        let tls_ctx = cli_tls_context(cfg, &material)?;
        let verified = trusted_config(before, cfg, &material, &tls_ctx).with_context(|| format!("the config in {path}"))?;
        if !verified.same_settings(before) {
            anyhow::bail!(
                "{path} has edits around its signature, which an admin never signed: start from what was signed \
                 (`stouter config pull`) and make the change again"
            );
        }
        cfg.dynamic_config.changed_by = signer;
        cfg.dynamic_config = signing::sign(&cfg.dynamic_config, &signing::Signer::from_material(&material)?)?;
    } else {
        cfg.dynamic_config.changed_by =
            if cfg.node_id.is_empty() { config::get_hostname() } else { cfg.node_id.clone() };
        // An edit invalidates any signature the config carried.
        cfg.dynamic_config.signature = None;
        cfg.dynamic_config.signed_at = None;
    }
    cfg.save(path)?;
    history::record(path, &cfg.dynamic_config);
    Ok(())
}

/// This host's TLS identity, from its `tlsDir`, for telling peers about a
/// local change. `None` if it has none yet (its daemon never ran): the
/// daemon tells them itself when it starts. Never creates a CA, as one made
/// here would become the daemon's, cut off from the cluster's.
fn local_tls(cfg: &Config) -> Result<Option<tls::TlsContext>> {
    let Some(dir) = cfg.tls_dir.as_deref() else { return Ok(None) };
    match pki::load_existing(dir, &cfg.node_id) {
        Ok(material) => Ok(Some(tls::TlsContext::new(&material)?)),
        Err(e) => {
            eprintln!("note: not telling peers now ({e:#}); the daemon will when it starts");
            Ok(None)
        }
    }
}

/// Push a locally changed config to the cluster.
async fn broadcast_config(cfg: &Config, path: &str) -> Result<()> {
    let tls_ctx = local_tls(cfg)?;
    let state = SharedState::new(cfg.clone(), path.to_string(), CancellationToken::new(), tls_ctx);
    gossip::broadcast_message(&state, Message::ConfigUpdate { config: cfg.dynamic_config.clone() }).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CA, and certificates for `names` issued by it: (dir, material by name).
    fn cluster_certs(names: &[&str]) -> (std::path::PathBuf, std::collections::HashMap<String, pki::PkiMaterial>) {
        let dir = std::env::temp_dir().join(format!("stouter_cli_{}_{}", std::process::id(), rand::random::<u64>()));
        let ca = dir.join("ca");
        pki::load_or_create(ca.to_str().unwrap(), "ca-node").unwrap();
        let mut certs = std::collections::HashMap::new();
        for name in names {
            let out = dir.join(name);
            pki::issue_cert_to_dir(ca.to_str().unwrap(), name, out.to_str().unwrap()).unwrap();
            certs.insert(name.to_string(), pki::load_existing(out.to_str().unwrap(), name).unwrap());
        }
        (dir, certs)
    }

    fn api_on_port(port: u16) -> config::DynamicConfig {
        serde_json::from_value(serde_json::json!({
            "version": 3,
            "serviceGroups": [{"name": "web", "services": [{"name": "api", "nodeId": "n1", "nodePort": port}]}],
        }))
        .unwrap()
    }

    fn port_of(dc: &config::DynamicConfig) -> u16 {
        dc.service_groups[0].services[0].node_port
    }

    #[test]
    fn the_cli_builds_only_on_what_an_admin_signed() {
        let (dir, certs) = cluster_certs(&["admin", "other-admin", "mallory"]);
        let sign = |dc: &config::DynamicConfig, who: &str| {
            signing::sign(dc, &signing::Signer::from_material(&certs[who]).unwrap()).unwrap()
        };
        let me = &certs["admin"];
        let tls_ctx = tls::TlsContext::new(me).unwrap();
        let no_admins = Config::default();

        let signed = sign(&api_on_port(3000), "admin");
        assert_eq!(port_of(&trusted_config(&signed, &no_admins, me, &tls_ctx).unwrap()), 3000);

        // A node that edits the config around the signature: what was
        // signed is what the change is made on.
        let mut edited = signed.clone();
        edited.service_groups[0].services[0].node_port = 6666;
        assert_eq!(port_of(&trusted_config(&edited, &no_admins, me, &tls_ctx).unwrap()), 3000);

        // Signed by a node with a cluster certificate, but not an admin we
        // trust (whatever adminCns the daemon claims): refused.
        let forged = sign(&api_on_port(6666), "mallory");
        let err = trusted_config(&forged, &no_admins, me, &tls_ctx).expect_err("refused");
        assert!(format!("{err:#}").contains("mallory"), "{err:#}");

        // Another admin's change: trusted once listed in the CLI's adminCns.
        let theirs = sign(&api_on_port(4000), "other-admin");
        assert!(trusted_config(&theirs, &no_admins, me, &tls_ctx).is_err());
        let listed = Config { admin_cns: vec!["other-admin".into()], ..Config::default() };
        assert_eq!(port_of(&trusted_config(&theirs, &listed, me, &tls_ctx).unwrap()), 4000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_signing_daemon_starts_with_what_was_signed() {
        let (dir, certs) = cluster_certs(&["admin", "n1"]);
        let tls_ctx = tls::TlsContext::new(&certs["n1"]).unwrap();
        let signed = signing::sign(&api_on_port(3000), &signing::Signer::from_material(&certs["admin"]).unwrap()).unwrap();
        let mut edited = signed.clone();
        edited.service_groups[0].services[0].node_port = 6666;
        let cfg = Config {
            require_signed_config: true,
            admin_cns: vec!["admin".into()],
            dynamic_config: edited.clone(),
            ..Config::default()
        };
        assert_eq!(port_of(&startup_config(&cfg, &tls_ctx, "stouter.json")), 3000, "the edit is not in force");
        // Unsigned (a cluster before its first signed change): kept.
        let unsigned = Config { dynamic_config: api_on_port(5000), ..cfg.clone() };
        assert_eq!(port_of(&startup_config(&unsigned, &tls_ctx, "stouter.json")), 5000);
        // Without requireSignedConfig the file is the config.
        let open = Config { require_signed_config: false, ..cfg };
        assert_eq!(port_of(&startup_config(&open, &tls_ctx, "stouter.json")), 6666);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_push_from_a_file_behind_the_cluster_is_refused_unless_forced() {
        let at = |version| config::DynamicConfig { version, ..Default::default() };
        // The file holds what the cluster had at 4; the cluster moved on.
        let refusal = stale_push(&at(5), &at(4), "stouter.json", false).expect("refused");
        assert!(refusal.contains("version 5") && refusal.contains("version 4") && refusal.contains("config pull"), "{refusal}");
        assert!(stale_push(&at(5), &at(4), "stouter.json", true).is_none(), "--force");
        // Up to date, or ahead (a daemon behind the rest): fine.
        assert!(stale_push(&at(5), &at(5), "stouter.json", false).is_none());
        assert!(stale_push(&at(5), &at(6), "stouter.json", false).is_none());
    }

    #[test]
    fn an_edit_made_while_the_daemon_was_stopped_gets_a_new_version() {
        let ran = api_on_port(3000); // version 3
        let entry = |dc: &config::DynamicConfig| history::HistoryEntry {
            version: dc.version,
            changed_by: String::new(),
            at_ms: 0,
            signed_by: None,
            changes: vec![],
            config: Some(dc.clone()),
        };
        let history = vec![entry(&ran)];
        let with = |dc: config::DynamicConfig| Config { node_id: "n1".into(), dynamic_config: dc, ..Config::default() };

        // Edited at the same version (a port, or a policy): the next version, by this node.
        let stamped = stamp_offline_edit(&with(api_on_port(4000)), &history).unwrap();
        assert_eq!((stamped.version, stamped.changed_by.as_str(), port_of(&stamped)), (4, "n1", 4000));
        let mut revoked = ran.clone();
        revoked.revoked.nodes.push("x".into());
        assert_eq!(stamp_offline_edit(&with(revoked), &history).unwrap().version, 4);

        // Unchanged, edited with its version raised, or no history: as is.
        assert!(stamp_offline_edit(&with(ran.clone()), &history).is_none());
        assert!(stamp_offline_edit(&with(config::DynamicConfig { version: 9, ..api_on_port(4000) }), &history).is_none());
        assert!(stamp_offline_edit(&with(api_on_port(4000)), &[]).is_none());
        // With requireSignedConfig, an edit isn't taken (startup_config).
        let signed = Config { require_signed_config: true, ..with(api_on_port(4000)) };
        assert!(stamp_offline_edit(&signed, &history).is_none());
    }

    #[test]
    fn same_settings_ignores_version_author_and_signature() {
        let a: config::DynamicConfig = serde_json::from_value(serde_json::json!({
            "version": 3, "changedBy": "admin-1",
            "serviceGroups": [{"name": "g", "services": [{"name": "s", "nodeId": "n", "nodePort": 1}]}],
        }))
        .unwrap();
        let b = config::DynamicConfig { version: 9, changed_by: "admin-2".into(), ..a.clone() };
        assert!(a.same_settings(&b));
        let mut c = a.clone();
        c.service_groups[0].services[0].node_port = 2;
        assert!(!a.same_settings(&c));
    }

    #[test]
    fn a_signed_local_change_is_made_only_on_what_was_signed() {
        let (dir, certs) = cluster_certs(&["admin"]);
        let admin_dir = dir.join("admin");
        let admin_dir = admin_dir.to_str();
        let path = dir.join("stouter.json");
        let path = path.to_str().unwrap();
        let base = Config { require_signed_config: true, admin_cns: vec!["admin".into()], ..Config::default() };
        // The file holds `before`; the command changes the port to 4000.
        let change = |before: &config::DynamicConfig| {
            Config { dynamic_config: before.clone(), ..base.clone() }.save(path).unwrap();
            let mut cfg = Config { dynamic_config: before.clone(), ..base.clone() };
            cfg.dynamic_config.service_groups[0].services[0].node_port = 4000;
            cfg.dynamic_config.version += 1;
            save_local_change(&mut cfg, before, path, admin_dir)
        };

        let err = change(&api_on_port(3000)).expect_err("nothing signed to build on");
        assert!(format!("{err:#}").contains("isn't admin-signed yet"), "{err:#}");

        let signed = signing::sign(&api_on_port(3000), &signing::Signer::from_material(&certs["admin"]).unwrap()).unwrap();
        let mut edited = signed.clone();
        edited.service_groups[0].services[0].node_port = 6666;
        let err = change(&edited).expect_err("an edit no admin signed");
        assert!(format!("{err:#}").contains("edits around its signature"), "{err:#}");

        change(&signed).unwrap();
        let saved = Config::load_or_default(path).unwrap().dynamic_config;
        let tls_ctx = tls::TlsContext::new(&certs["admin"]).unwrap();
        let verified = trusted_config(&saved, &base, &certs["admin"], &tls_ctx).unwrap();
        assert_eq!((port_of(&verified), verified.version), (4000, 4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_revoked_admin_signs_nothing_and_no_config_revokes_its_own_signer() {
        let (dir, certs) = cluster_certs(&["admin", "other-admin"]);
        let sign = |dc: &config::DynamicConfig, who: &str| {
            signing::sign(dc, &signing::Signer::from_material(&certs[who]).unwrap()).unwrap()
        };
        let me = &certs["admin"];
        let tls_ctx = tls::TlsContext::new(me).unwrap();
        let admins = Config { admin_cns: vec!["admin".into(), "other-admin".into()], ..Config::default() };

        // A config revoking the admin who signed it: refused.
        let mut revoking_self = api_on_port(3000);
        revoking_self.revoked.nodes.push("admin".into());
        let err = trusted_config(&sign(&revoking_self, "admin"), &admins, me, &tls_ctx).expect_err("refused");
        assert!(format!("{err:#}").contains("revokes the certificate that signed it"), "{err:#}");
        // Another admin can revoke it.
        assert!(trusted_config(&sign(&revoking_self, "other-admin"), &admins, me, &tls_ctx).is_ok());
        // By certificate too.
        let mut by_cert = api_on_port(3000);
        by_cert.revoked.certs.push(tls::fingerprint_hex(certs["other-admin"].node_cert_der.as_ref()));
        assert!(trusted_config(&sign(&by_cert, "other-admin"), &admins, me, &tls_ctx).is_err());

        // Once revoked here, an admin's signature counts for nothing.
        let theirs = sign(&api_on_port(4000), "other-admin");
        assert!(trusted_config(&theirs, &admins, me, &tls_ctx).is_ok());
        tls_ctx.revocations().set(&config::Revoked { nodes: vec!["other-admin".into()], certs: vec![] });
        let err = trusted_config(&theirs, &admins, me, &tls_ctx).expect_err("revoked");
        assert!(format!("{err:#}").contains("revoked") || format!("{err:#}").contains("Revoked"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cli_wont_revoke_its_own_certificate() {
        let (dir, certs) = cluster_certs(&["admin"]);
        let cert_dir = dir.join("admin");
        let cert_dir = cert_dir.to_str();
        let cfg = Config::default();
        let mine = tls::fingerprint_hex(certs["admin"].node_cert_der.as_ref());
        assert!(refuse_revoking_own_cert(&cfg, cert_dir, &["admin".into()], &[]).is_err());
        assert!(refuse_revoking_own_cert(&cfg, cert_dir, &[], std::slice::from_ref(&mine)).is_err());
        assert!(refuse_revoking_own_cert(&cfg, cert_dir, &["n1".into()], &[]).is_ok());

        // What to revoke: names as given, certificates by fingerprint,
        // from a file or written any common way.
        let file = dir.join("admin/node.crt");
        let colons: String = mine.as_bytes().chunks(2).map(|b| std::str::from_utf8(b).unwrap().to_uppercase()).collect::<Vec<_>>().join(":");
        let targets = RevokeTargets {
            nodes: vec!["n1".into()],
            cert_files: vec![file.to_str().unwrap().into()],
            fingerprints: vec![colons],
        };
        assert_eq!(targets.resolve().unwrap(), (vec!["n1".to_string()], vec![mine.clone(), mine]));
        let nothing = RevokeTargets { nodes: vec![], cert_files: vec![], fingerprints: vec![] };
        assert!(nothing.resolve().is_err());
        let bad = RevokeTargets { nodes: vec![], cert_files: vec![], fingerprints: vec!["abc".into()] };
        assert!(bad.resolve().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cli_takes_signing_from_its_own_config_too() {
        let open = Config::default();
        let strict = Config { require_signed_config: true, ..Config::default() };
        assert!(!signed_cluster(&open, false));
        assert!(signed_cluster(&open, true));
        // A daemon saying signatures aren't needed doesn't override the CLI.
        assert!(signed_cluster(&strict, false));
    }

    /// A daemon stand-in: answers each `StatusRequest` with `node_id` and
    /// `mode`, over TLS with `material`'s certificate. Returns its address.
    async fn status_answerer(material: &pki::PkiMaterial, node_id: &str, mode: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let tls_ctx = std::sync::Arc::new(tls::TlsContext::new(material).unwrap());
        let (node_id, mode) = (node_id.to_string(), mode.to_string());
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let (tls_ctx, node_id, mode) = (tls_ctx.clone(), node_id.clone(), mode.clone());
                tokio::spawn(async move {
                    let Ok(mut stream) = tls_ctx.accept(tcp).await else { return };
                    if let Ok(Message::StatusRequest) = gossip::recv_message(&mut stream).await {
                        let reply = Message::StatusResponse {
                            mode,
                            node_id,
                            bind: String::new(),
                            dynamic_config: config::DynamicConfig::default(),
                            known_nodes: vec![],
                            require_signed_config: false,
                            cert_expires: None,
                            issues_certs: false,
                        };
                        let _ = gossip::send_message(&mut stream, &reply).await;
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn a_service_goes_on_the_node_the_certificate_names() {
        let (dir, certs) = cluster_certs(&["admin", "n1", "mallory"]);
        let admin_dir = dir.join("admin");
        let admin_dir = admin_dir.to_str();
        let cfg = Config::default();

        let honest = status_answerer(&certs["n1"], "n1", "node").await;
        assert_eq!(owner_node(None, Some(&honest), &cfg, admin_dir).await.unwrap(), "n1");

        // Mallory claims to be n1, to have the CLI change n1's services.
        let lying = status_answerer(&certs["mallory"], "n1", "node").await;
        let err = owner_node(None, Some(&lying), &cfg, admin_dir).await.expect_err("refused");
        assert!(format!("{err:#}").contains("certificate is for 'mallory'"), "{err:#}");
        // --node says which, and nothing is asked.
        assert_eq!(owner_node(Some("n1".into()), Some(&lying), &cfg, admin_dir).await.unwrap(), "n1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An admin's CLI knows what its config revokes: a config signed with a
    /// revoked admin's key (say, leaked) isn't built on, and a daemon
    /// answering with a revoked certificate isn't talked to.
    #[tokio::test]
    async fn the_cli_refuses_what_its_config_revokes() {
        let (dir, certs) = cluster_certs(&["alice", "bob", "m"]);
        let alice_dir = dir.join("alice");
        let alice_dir = alice_dir.to_str();
        let mut cfg = Config {
            require_signed_config: true,
            admin_cns: vec!["alice".into(), "bob".into()],
            ..Config::default()
        };
        cfg.dynamic_config.revoked.nodes.push("bob".into());

        let mut forged = api_on_port(6666);
        forged.version = 11;
        let forged = signing::sign(&forged, &signing::Signer::from_material(&certs["bob"]).unwrap()).unwrap();
        // Without the revocation list, bob's signature would pass.
        let blind = tls::TlsContext::new(&certs["alice"]).unwrap();
        assert!(trusted_config(&forged, &cfg, &certs["alice"], &blind).is_ok());
        let tls_ctx = cli_tls_context(&cfg, &certs["alice"]).unwrap();
        let err = trusted_config(&forged, &cfg, &certs["alice"], &tls_ctx).expect_err("revoked");
        assert!(format!("{err:#}").to_lowercase().contains("revoked"), "{err:#}");

        // A daemon presenting bob's certificate: refused at the handshake.
        let as_bob = status_answerer(&certs["bob"], "bob", "node").await;
        let err = fetch_trusted_config(&as_bob, &cfg, alice_dir).await.expect_err("refused");
        assert!(!format!("{err:#}").contains("isn't admin-signed"), "got as far as the config: {err:#}");
        // A member's daemon is talked to (and its unsigned config refused).
        let as_m = status_answerer(&certs["m"], "m", "node").await;
        let err = fetch_trusted_config(&as_m, &cfg, alice_dir).await.expect_err("unsigned");
        assert!(format!("{err:#}").contains("isn't admin-signed"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_change_on_a_subscriber_host_goes_to_its_file() {
        let (dir, certs) = cluster_certs(&["s1"]);
        let path = dir.join("stouter.json");
        let path = path.to_str().unwrap();
        for (mode, to_daemon) in [("subscribe", false), ("node", true)] {
            let addr = status_answerer(&certs["s1"], "s1", mode).await;
            let cfg = Config {
                node_id: "s1".into(),
                bind: addr.clone(),
                tls_dir: Some(dir.join("s1").to_str().unwrap().into()),
                ..Config::default()
            };
            cfg.save(path).unwrap();
            let target = change_target(None, path, None).await.unwrap();
            assert_eq!(target, to_daemon.then_some(addr), "{mode}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_routes_fills_in_service_and_validates() {
        let routes = parse_routes("api", r#"[{"match": {"pathPrefix": "/v2"}, "backends": [{"labels": {"version": "v2"}}]}]"#).unwrap();
        assert_eq!(routes[0].service, "api");
        assert!(parse_routes("api", r#"[{"service": "web", "backends": [{}]}]"#).is_err());
        assert!(parse_routes("api", r#"{"not": "an array"}"#).is_err());
        assert!(parse_routes("api", r#"[{"backends": []}]"#).is_err());
    }

    #[test]
    fn parse_key_value_requires_key() {
        assert_eq!(parse_key_value("version=v2").unwrap(), ("version".into(), "v2".into()));
        assert_eq!(parse_key_value("traefik.scheme=h2c").unwrap(), ("traefik.scheme".into(), "h2c".into()));
        assert_eq!(parse_key_value("a=b=c").unwrap(), ("a".into(), "b=c".into()));
        assert_eq!(parse_key_value("empty=").unwrap(), ("empty".into(), String::new()));
        assert!(parse_key_value("=v2").is_err());
        assert!(parse_key_value("novalue").is_err());
    }
}
