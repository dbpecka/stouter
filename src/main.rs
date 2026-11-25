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
    Check {
        /// Only what would stop the daemon from starting: this node's own
        /// settings, not the cluster's dynamic config (which the daemon
        /// starts with, warning) or unknown settings. What `deploy.sh`
        /// checks before replacing a running daemon.
        #[arg(long)]
        local: bool,
    },
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
        /// Take a config newer than the one in this file even though the
        /// certificate that signed it has expired (its key could have signed
        /// it after the fact): only if you know it was signed in time.
        #[arg(long)]
        accept_expired_signer: bool,
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
    let cli = Cli::parse();
    init_logging(cli.command.is_none());
    #[cfg(unix)]
    if cli.command.is_some() {
        end_quietly_when_output_is_closed();
    }

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
            for unknown in cfg.unknown_settings().into_iter().chain(Config::unknown_nested_settings_in(&cli.config)) {
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
            if let Some(warning) = new_ca_warning(&cfg, tls_dir) {
                tracing::warn!("{warning}");
            }
            let material = pki::load_or_create_with(tls_dir, &cfg.node_id, cfg.cert_lifetime())?;
            let tls_ctx = tls::TlsContext::new(&material)?;
            tls_ctx.revocations().set(&cfg.dynamic_config.revoked);

            cfg.dynamic_config = startup_config(&cfg, &tls_ctx, &cli.config);
            // Edited while the daemon was stopped: start on what it last
            // ran, and take the edit as the config poll takes any (checked,
            // as a new version), once caught up with the cluster.
            let edited_offline = edited_while_stopped(&cfg, &history::load(&cli.config));
            if let Some(ran) = &edited_offline {
                tracing::info!(
                    "{}: changed while the daemon was stopped; starting on version {} as it last ran, and taking the \
                     change once caught up with the cluster",
                    cli.config,
                    ran.version
                );
                cfg.dynamic_config = ran.clone();
            }

            let state = SharedState::new(cfg.clone(), cli.config.clone(), shutdown.clone(), Some(tls_ctx));
            // Its file now holds an edit not yet applied (see
            // `gossip::run_config_poll_loop`): what the file says isn't what
            // the daemon runs, which SharedState::new takes it to be.
            if edited_offline.is_some() {
                state.edited_offline.store(true, std::sync::atomic::Ordering::Relaxed);
            }
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
            state.spawn_essential("certificate renewal", renew::run_renewal_loop(state.clone()));

            if let Some(tracing_cfg) = &cfg.tracing {
                let _ = state.tracer.set(telemetry::Tracer::start(tracing_cfg, &cfg.node_id));
            }

            let metrics_handle = metrics::install();
            let upkeep = metrics_handle.clone();
            state.spawn_essential("metrics", metrics::run_gauge_loop(state.clone(), move || upkeep.run_upkeep()));
            state.spawn_essential("the pool sweeper", state::run_pool_sweeper(state.clone()));

            let daemon = async {
                match cfg.mode {
                    config::Mode::Node => node::run_node(state.clone(), metrics_handle).await,
                    config::Mode::Subscribe | config::Mode::NodeSubscribe => {
                        subscribe::run_subscribe(state.clone(), metrics_handle).await
                    }
                }
            };

            let failed = state.failed.clone();
            tokio::select! {
                result = daemon => result?,
                signal = shutdown_signal() => {
                    info!("received {signal}");
                    // A second signal while draining means don't wait.
                    tokio::select! {
                        () = graceful_drain(state) => {}
                        again = shutdown_signal() => {
                            tracing::warn!("received {again} again: exiting without waiting for the drain to finish");
                            std::process::exit(1);
                        }
                    }
                }
                // A loop the daemon needs stopped (see
                // `SharedState::spawn_essential`): drain, then exit with an
                // error so Docker or systemd starts it again.
                () = failed.cancelled() => {
                    tokio::select! {
                        () = graceful_drain(state) => {}
                        signal = shutdown_signal() => info!("received {signal}: not waiting for the drain to finish"),
                    }
                    std::process::exit(1);
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
            // Certificates beside the config, wherever the daemon runs from.
            if cfg.tls_dir.is_none() {
                let config_path = std::path::absolute(&cli.config).unwrap_or_else(|_| cli.config.clone().into());
                let dir = config_path.parent().unwrap_or(std::path::Path::new(".")).join("tls");
                cfg.tls_dir = Some(dir.to_string_lossy().into_owned());
            }

            cfg.save(&cli.config)?;

            println!(
                "Node initialized. node_id={}, tlsDir={}, config saved to {}",
                cfg.node_id,
                cfg.tls_dir.as_deref().unwrap_or_default(),
                cli.config
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
        Some(Commands::Check { local }) => {
            let result = if local {
                // What would stop the daemon starting: its own settings, and
                // the certificates in its tlsDir (an incomplete set, a key
                // that isn't its certificate's).
                Config::load(&cli.config).and_then(|cfg| {
                    cfg.validate_local()?;
                    if let Some(dir) = cfg.tls_dir.as_deref() {
                        pki::check_dir(dir, &cfg.node_id).with_context(|| format!("the certificates in {dir}"))?;
                        if let Some(warning) = new_ca_warning(&cfg, dir) {
                            eprintln!("warning: {warning}");
                        }
                    }
                    Ok(())
                })
            } else {
                Config::check(&cli.config).map(|_| ())
            };
            match result {
                Ok(()) if local => {
                    println!("{}: the daemon can start with it", cli.config);
                }
                Ok(()) => {
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
            // An outboundOnly daemon listens nowhere: there is nothing here
            // to ask, whether it runs or not.
            if remote.is_none() && cfg.outbound_only {
                eprintln!(
                    "{} is outboundOnly: its daemon has no listener to ask. Ask a node it syncs with instead \
                     (`stouter status --remote host:port`)",
                    cli.config
                );
                std::process::exit(1);
            }
            let target = remote.clone().unwrap_or_else(|| cfg.local_target());

            // No daemon (or no answer) is a failure, for scripts and health
            // checks alike.
            match tokio::time::timeout(state::CONNECT_TIMEOUT, tokio::net::TcpStream::connect(&target)).await {
                Err(_) => {
                    println!("No daemon answering on {target} (connect timed out)");
                    std::process::exit(1);
                }
                Ok(Err(_)) => {
                    println!("No daemon running on {target}");
                    std::process::exit(1);
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
                                    history::format_secs(until.max(0) as u64),
                                    if issues_certs { "; this node holds the CA's key and renews others'" } else { "" }
                                );
                            }
                            if require_signed_config {
                                println!("\nConfig changes must be signed by an admin (requireSignedConfig).");
                            }
                        }
                        _ => {
                            println!("Unexpected response from daemon");
                            std::process::exit(1);
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
            // `--domains ""` is no domains, and `a,,b` is a and b: never an
            // empty one, which every other empty one would clash with.
            let domains: Vec<String> =
                domains.into_iter().map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).collect();
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
                    &cli.config,
                    cli.cert_dir.as_deref(),
                )
                .await?;
                println!(
                    "Service '{}' added to group '{}' for node '{}', through {} (version {})",
                    name, group_name, owner_node_id, target, version
                );
            } else {
                let mut cfg = Config::load(&cli.config)?;
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
                    &cli.config,
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
                    &cli.config,
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
                    &cli.config,
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
                &cli.config,
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
                &cli.config,
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
                        &cli.config,
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
                // Unless its signer's certificate has expired: then it is
                // signed again, so that key can sign nothing more nodes take.
                let resign = known.as_ref().and_then(signing::expired_signer);
                if let Some((who, _)) = &resign {
                    println!("{target}'s config was signed by '{who}', whose certificate has expired: signing it again");
                }
                if let Some(current) = known.as_ref().filter(|current| current.same_settings(&desired) && resign.is_none()) {
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
            action: ConfigAction::Pull { remote, accept_expired_signer },
        }) => {
            let mut cfg = Config::load(&cli.config)?;
            let target = remote.unwrap_or_else(|| cfg.local_target());
            let material = load_cli_material(&cfg, cli.cert_dir.as_deref())?;
            let tls_ctx = cli_tls_context(&cfg, &material)?;
            let daemon = fetch_daemon_config(&target, &tls_ctx).await?;
            let current = if !signed_cluster(&cfg, daemon.require_signed_config) {
                daemon.dynamic_config
            } else if daemon.dynamic_config.signature.is_none() {
                refuse_unsigned_over_signed(&target, &cfg, &cli.config, &material, &tls_ctx)?;
                eprintln!(
                    "warning: the config on {target} isn't admin-signed yet; review it before you sign it with \
                     `stouter config push`"
                );
                daemon.dynamic_config
            } else {
                // Only what an admin signed: the file is what `config push`
                // signs next.
                let verified = trusted_config(&daemon.dynamic_config, &cfg, &material, &tls_ctx)
                    .with_context(|| format!("won't write the config from {target} to {}", cli.config))?;
                refuse_against_local(&verified, &target, &cfg, &material, &tls_ctx, accept_expired_signer)?;
                verified
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
                run_admin_rpc(target, op, &local, &cli.config, cli.cert_dir.as_deref()).await?
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
                    let config = entries.into_iter().rev().find_map(|e| e.config).ok_or_else(|| {
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
            // The latest at that version (a history written before versions
            // were kept unique may hold two).
            let earlier = entries.into_iter().rev().filter(|e| e.version == to).find_map(|e| e.config).ok_or_else(|| {
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
            // As signed (or, unsigned, as given): a history can't pass off
            // another version as the one asked for.
            if earlier.version != to {
                anyhow::bail!("the history's version {to} is really version {}: not putting it back", earlier.version);
            }
            let op = AdminOp::Restore { config: earlier };
            let version = if let Some(addr) = &target {
                run_admin_rpc(addr, op, &local, &cli.config, cli.cert_dir.as_deref()).await?
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
                let version = run_admin_rpc(target, op, &local, &cli.config, cli.cert_dir.as_deref()).await?;
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

/// A warning, if the daemon is about to make a new cluster CA (its tlsDir
/// holds no certificates) though its config lists peers to join: they
/// trust only the CA they share, so the node would run cut off from them.
/// Not refused, as the first node of a new cluster may list the others.
fn new_ca_warning(cfg: &Config, tls_dir: &str) -> Option<String> {
    (!pki::holds_certificates(tls_dir) && !cfg.known_nodes.is_empty()).then(|| {
        format!(
            "{tls_dir} holds no certificates, so the daemon makes a new cluster CA, though knownNodes lists peers: \
             unless this is the first node of a new cluster, it won't be able to join them. Stop it, put the ca.crt, \
             node.crt and node.key `stouter cert issue --node {}` made on the CA node in {tls_dir} (and no ca.key), \
             and start it again",
            cfg.node_id
        )
    })
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

/// The config the daemon last ran (`history`'s newest entry), if the config
/// file says something else: an edit made while it was stopped. Taken as
/// the daemon starts, such an edit could meet a change the cluster made
/// meanwhile at the same version (the tie-break then drops one, with no
/// trace) or skip the checks a running daemon makes; so the daemon starts
/// on what it last ran, and its config poll takes the edit once it has
/// caught up (see `gossip::run_config_poll_loop`). If the cluster moved on
/// meanwhile, the edit is kept aside as `.unapplied` instead. A CLI change
/// made with no daemon running is such an edit (see `save_local_change`).
///
/// With `requireSignedConfig`, only a signed one (`cfg` holds what
/// [`startup_config`] took from the file): a hand edit isn't taken at all.
/// Not without a history to compare with.
fn edited_while_stopped(cfg: &Config, history: &[history::HistoryEntry]) -> Option<config::DynamicConfig> {
    if cfg.require_signed_config && cfg.dynamic_config.signature.is_none() {
        return None;
    }
    let ran = history.last()?.config.as_ref()?;
    let file = &cfg.dynamic_config;
    (file.version != ran.version || !file.same_settings(ran)).then(|| ran.clone())
}

/// A CLI command whose output is cut short (`stouter config history |
/// head`) ends as Unix tools do, killed by SIGPIPE, rather than with a
/// panic about the closed pipe.
///
/// SIGPIPE itself stays ignored (as Rust sets it): at its default it would
/// also end the process, silently, when a daemon closes a connection the
/// CLI is writing to (TLS writes don't ask the kernel to hold the signal
/// back), as one refusing the CLI's certificate does. That is an error to
/// report. Only a failed print, which panics, is turned into the signal.
#[cfg(unix)]
fn end_quietly_when_output_is_closed() {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        let broken_pipe = std::io::Error::from_raw_os_error(libc::EPIPE).to_string();
        if message.starts_with("failed printing to std") && message.ends_with(&broken_pipe) {
            // SAFETY: restoring a signal's default action and raising it,
            // which ends the process.
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                libc::raise(libc::SIGPIPE);
            }
        }
        report(info);
    }));
}

/// Structured logging, filtered by `RUST_LOG` if set. The daemon logs to
/// stdout, from `info` up. A CLI command logs to stderr, only warnings and
/// errors by default, so what it prints on stdout (the JSON of `config
/// history --version N`, say) is all there is. Colour only on a terminal.
fn init_logging(daemon: bool) {
    use std::io::IsTerminal;
    let filter = |default: &str| {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default))
    };
    let logs = tracing_subscriber::fmt();
    if daemon {
        // A log line that can't be written (stdout closed: `stouter |
        // logger` whose reader died) is dropped. By default the subscriber
        // reports it on stderr, and printing to a closed stderr panics,
        // which would end whatever task was logging.
        logs.with_env_filter(filter("info"))
            .with_ansi(std::io::stdout().is_terminal())
            .log_internal_errors(false)
            .init();
    } else {
        logs.with_env_filter(filter("warn"))
            .with_writer(std::io::stderr)
            .with_ansi(std::io::stderr().is_terminal())
            .init();
    }
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
    // The local config's revocations are its own cluster's: with
    // certificates from another cluster's CA (`service move`, or --remote
    // with another cluster's --cert-dir), its names would wrongly refuse
    // that cluster's nodes and admins. Applied unless the local tlsDir's
    // CA is known to be another one.
    let local_ca = cfg.tls_dir.as_deref().and_then(|dir| pki::read_cert(&format!("{dir}/ca.crt")).ok());
    if local_ca.is_none_or(|ca| ca == material.ca_cert_der) {
        tls_ctx.revocations().set(&cfg.dynamic_config.revoked);
    }
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

/// Refuse `served` (a verified config from `target`) to make a change on,
/// or to pull, where the admin-signed config in the CLI's own file says it
/// can't be the cluster's: a node behind the cluster, or lying about its
/// config, would otherwise have the admin sign over what came since (a
/// revocation, say). Refused: an older version; another config at the same
/// version; one nodes holding the local config would refuse (signed well
/// before it, or too far ahead); and a newer one signed with a certificate
/// that has expired since (its key may have signed it now, dated back),
/// unless `accept_expired`. With no signed config in the file to compare
/// with, only that last is said, as a warning.
fn refuse_against_local(
    served: &config::DynamicConfig,
    target: &str,
    cfg: &Config,
    material: &pki::PkiMaterial,
    tls_ctx: &tls::TlsContext,
    accept_expired: bool,
) -> Result<()> {
    let expired = signing::expired_signer(served);
    let Ok(local) = trusted_config(&cfg.dynamic_config, cfg, material, tls_ctx) else {
        if let Some((who, until)) = &expired {
            eprintln!(
                "warning: the config on {target} (version {}) was signed by '{who}', whose certificate expired {}: \
                 if that key may be in the wrong hands, check the config before changing it",
                served.version,
                history::format_secs(*until)
            );
        }
        return Ok(());
    };
    let behind = "that node is behind the cluster (or not telling the truth); make the change through another node \
                  (--remote)";
    if served.version < local.version {
        anyhow::bail!(
            "the config on {target} is version {}, older than the admin-signed version {} in this CLI's config file: \
             {behind}",
            served.version,
            local.version
        );
    }
    if served.same_settings(&local) {
        return Ok(());
    }
    if served.version == local.version {
        anyhow::bail!(
            "the config on {target} is another config at version {}, not the one in this CLI's config file: two \
             changes were made at once, or {behind}",
            served.version
        );
    }
    if let Some(why) = signing::signed_too_early(served, &local).or_else(|| config::too_far_ahead(served, &local)) {
        anyhow::bail!("the config on {target}: {why}");
    }
    if let Some((who, until)) = expired.filter(|_| !accept_expired) {
        anyhow::bail!(
            "the config on {target} (version {}) is newer than the one in this CLI's config file (version {}), and \
             was signed by '{who}', whose certificate expired {}: that key may have signed it after the fact, dated \
             back. If you know '{who}' signed it while the certificate was valid, take it with `stouter config pull \
             --accept-expired-signer`",
            served.version,
            local.version,
            history::format_secs(until)
        );
    }
    Ok(())
}

/// Refuse to pull `target`'s unsigned config over a file holding an
/// admin-signed one. Fine before the first signed change; but over a signed
/// file, the file's config (and its revocations, which the CLI trusts)
/// would be replaced by whatever an unverified node says, ready for
/// `config push` to sign.
fn refuse_unsigned_over_signed(
    target: &str,
    cfg: &Config,
    path: &str,
    material: &pki::PkiMaterial,
    tls_ctx: &tls::TlsContext,
) -> Result<()> {
    if trusted_config(&cfg.dynamic_config, cfg, material, tls_ctx).is_ok() {
        anyhow::bail!(
            "the config on {target} isn't admin-signed, but the one in {path} is: not writing it over that (that \
             node is behind the cluster's signed configs, or not telling the truth)"
        );
    }
    Ok(())
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
    config_path: &str,
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
        refuse_against_local(&current, target, cfg, &material, &tls_ctx, false)?;
        let mut next = current.clone();
        if let Err(e) = op.clone().apply(&mut next) {
            return Ok(Err(((&e).into(), e.to_string())));
        }
        if let Some(problem) = config::new_problem(&current, &next) {
            return Ok(Err((AdminErrCode::Validation, problem)));
        }
        let push = signed_push(next, current.version, &material)?;
        let AdminOp::ConfigPush { config: pushed, .. } = &push else { unreachable!("signed_push makes a ConfigPush") };
        let pushed = pushed.clone();
        match send_admin(target, &tls_ctx, push).await? {
            Err((AdminErrCode::Conflict, _)) if attempt < PUSH_ATTEMPTS => continue,
            Ok(version) => {
                keep_pushed(config_path, &pushed, &material);
                return Ok(Ok(version));
            }
            result => return Ok(result),
        }
    }
    unreachable!("the last attempt always returns")
}

/// After a signed change was taken: the CLI's config file takes the config
/// it signed, as `config push` leaves it, so what this CLI trusts from now
/// on is at least that. Above all its revocations: the CLI refuses configs
/// signed with a certificate its file revokes (see [`cli_tls_context`]),
/// and an admin who ran `cert revoke --remote` and kept an older file
/// would sign their next change on top of a config forged with the revoked
/// key, undoing the revocation.
///
/// Only over a config of the same cluster, as an admin signed it with no
/// edits made around it, and older: never over edits not yet pushed (say
/// so, if they keep a revocation out of the file), or another cluster's
/// config (`service move`).
fn keep_pushed(config_path: &str, pushed: &config::DynamicConfig, material: &pki::PkiMaterial) {
    let Ok(mut file) = Config::load(config_path) else { return };
    let held = &file.dynamic_config;
    if held.version >= pushed.version {
        return;
    }
    let unedited = cli_tls_context(&file, material)
        .and_then(|tls_ctx| trusted_config(held, &file, material, &tls_ctx))
        .is_ok_and(|signed| signed.version == held.version && signed.same_settings(held));
    if !unedited {
        if held.revoked != pushed.revoked && held.signature.is_some() {
            eprintln!(
                "note: {config_path} holds edits not yet pushed, so it was left at version {}: until it holds \
                 version {} (`stouter config pull`), this CLI doesn't know that version's revocations",
                held.version, pushed.version
            );
        }
        return;
    }
    file.dynamic_config = pushed.clone();
    if let Err(e) = file.save(config_path) {
        eprintln!("warning: could not update {config_path} to version {}: {e:#}", pushed.version);
    }
}

/// Like `try_admin_rpc`, but on `AdminResult::Err` exits the process with a
/// code derived from `AdminErrCode` so single-step callers don't need to
/// handle it.
async fn run_admin_rpc(
    target: &str,
    op: AdminOp,
    cfg: &Config,
    config_path: &str,
    cert_dir: Option<&str>,
) -> Result<u64> {
    match try_admin_rpc(target, op, cfg, config_path, cert_dir).await? {
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
    let when = dc.signed_at.map(|at| format!(" at {}", history::format_secs(at))).unwrap_or_default();
    let cert = match pki::validity(&der) {
        Some((_, until)) if until * 1000 < now_ms() as i64 => format!(
            "; that certificate expired {} (nodes holding this config keep it, but sign it again with \
             `stouter config push` so that key can sign nothing more nodes take)",
            history::format_secs(until.max(0) as u64)
        ),
        Some((_, until)) => format!("; that certificate is valid until {}", history::format_secs(until.max(0) as u64)),
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
/// on this host for this config (so the change is made where the config is
/// in force, not in its file behind its back, where a change the daemon
/// makes at the same time could undo it); else, with no daemon listening,
/// `None`: edit the file.
///
/// Only a refused connection means no daemon. One that is listening but
/// won't take this CLI's request (its certificate refused, say, or expired)
/// or doesn't answer is an error, never a reason to edit the file anyway.
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
    match tokio::time::timeout(LOCAL_DAEMON_CONNECT_TIMEOUT, tokio::net::TcpStream::connect(&target)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => return Ok(None),
        Ok(Err(e)) => anyhow::bail!("can't tell whether this host's daemon is running: connecting to {target}: {e}"),
        Err(_) => anyhow::bail!(
            "can't tell whether this host's daemon is running: {target} didn't accept a connection within {}s",
            LOCAL_DAEMON_CONNECT_TIMEOUT.as_secs()
        ),
    }
    let not_made = || format!("a daemon is running at {target} (the bind in {config_path}); the change was not made");
    let tls_ctx = build_cli_tls(&cfg, cert_dir).with_context(not_made)?;
    let asked = tokio::time::timeout(LOCAL_DAEMON_STATUS_TIMEOUT, daemon_request(&target, &tls_ctx, &Message::StatusRequest))
        .await
        .map_err(|_| anyhow::anyhow!("no answer within {}s", LOCAL_DAEMON_STATUS_TIMEOUT.as_secs()))
        .and_then(|answer| answer)
        .with_context(not_made)?;
    match asked {
        // A subscribe-mode daemon takes no admin requests (it hosts no
        // services, and its config is only ever what the cluster sends): a
        // change there is to the file, as with no daemon running.
        Message::StatusResponse { node_id, mode, .. } if node_id == cfg.node_id => {
            Ok((mode != "subscribe").then_some(target))
        }
        Message::StatusResponse { node_id, .. } => anyhow::bail!(
            "the daemon at {target} (the bind in {config_path}) is node '{node_id}', not '{}': is this the config of the \
             daemon running here? The change was not made",
            cfg.node_id
        ),
        other => anyhow::bail!("{}: it answered {other:?}", not_made()),
    }
}

/// How long connecting to this host's daemon may take before the CLI gives
/// up (an unanswered port is refused at once; this is a port that drops).
const LOCAL_DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long this host's daemon may take to say who it is.
const LOCAL_DAEMON_STATUS_TIMEOUT: Duration = Duration::from_secs(10);

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
    // Not into the history: that lists what the daemon ran, and it hasn't
    // run this. When it starts, it sees the file differs from what it last
    // ran and takes the change once caught up with the cluster, or, if the
    // cluster moved on meanwhile, keeps it aside as `.unapplied` (see
    // `edited_while_stopped`). Recorded as ran, the change would be in
    // force at once and lost at the first sync to the cluster's newer
    // config, with no trace. A node that never ran has no history: the
    // config the change was made on goes in, as its starting point.
    if history::load(path).is_empty() {
        history::record(path, before);
    }
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
    fn an_edit_made_while_the_daemon_was_stopped_waits_for_the_poll() {
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

        // Edited (a port, a revocation, or with its version raised): start
        // on what ran.
        assert_eq!(edited_while_stopped(&with(api_on_port(4000)), &history), Some(ran.clone()));
        let mut revoked = ran.clone();
        revoked.revoked.nodes.push("x".into());
        assert_eq!(edited_while_stopped(&with(revoked), &history), Some(ran.clone()));
        let raised = config::DynamicConfig { version: 9, ..api_on_port(4000) };
        assert_eq!(edited_while_stopped(&with(raised), &history), Some(ran.clone()));

        // Unchanged, or no history: as is.
        assert!(edited_while_stopped(&with(ran.clone()), &history).is_none());
        assert!(edited_while_stopped(&with(api_on_port(4000)), &[]).is_none());
        // With requireSignedConfig, an unsigned edit isn't taken
        // (startup_config); a signed one (the CLI's) waits like any other.
        let unsigned = Config { require_signed_config: true, ..with(api_on_port(4000)) };
        assert!(edited_while_stopped(&unsigned, &history).is_none());
        let mut signed_edit = api_on_port(4000);
        signed_edit.version = 4;
        signed_edit.signature =
            Some(config::ConfigSignature { config_json: String::new(), signer_cert: String::new(), signature: String::new() });
        let signed = Config { require_signed_config: true, ..with(signed_edit) };
        assert_eq!(edited_while_stopped(&signed, &history), Some(ran.clone()));
    }

    #[test]
    fn a_cli_change_made_with_no_daemon_running_waits_for_it_like_any_edit() {
        let dir = std::env::temp_dir().join(format!("stouter_offline_cli_{}_{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stouter.json");
        let path = path.to_str().unwrap();
        let add_service = |path: &str| {
            let mut cfg = Config::load(path).unwrap();
            let before = cfg.dynamic_config.clone();
            let mut dc = before.clone();
            dc.version = before.next_version().unwrap();
            dc.service_groups[0].services.push(serde_json::from_value(serde_json::json!(
                {"name": "added", "nodeId": "n1", "nodePort": 5000}
            )).unwrap());
            cfg.dynamic_config = dc;
            save_local_change(&mut cfg, &before, path, None).unwrap();
            Config::load(path).unwrap()
        };

        // A node that ran version 3: the change isn't recorded as run, so
        // the daemon starts on 3 and takes it once caught up (or keeps it
        // aside if the cluster moved on meanwhile).
        let ran = api_on_port(3000);
        Config { node_id: "n1".into(), dynamic_config: ran.clone(), ..Config::default() }.save(path).unwrap();
        history::record(path, &ran);
        let changed = add_service(path);
        assert_eq!(changed.dynamic_config.version, 4);
        assert_eq!(history::load(path).last().unwrap().version, 3, "history still ends at what ran");
        assert_eq!(edited_while_stopped(&changed, &history::load(path)), Some(ran.clone()));

        // A node that never ran: what the change was made on is its start.
        let _ = std::fs::remove_file(history::path_for(path));
        Config { node_id: "n1".into(), dynamic_config: ran.clone(), ..Config::default() }.save(path).unwrap();
        let changed = add_service(path);
        assert_eq!(history::load(path).iter().map(|e| e.version).collect::<Vec<_>>(), vec![3]);
        assert_eq!(edited_while_stopped(&changed, &history::load(path)), Some(ran));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn making_a_new_ca_while_listing_peers_is_warned_about() {
        let dir = std::env::temp_dir().join(format!("stouter_new_ca_{}_{}", std::process::id(), rand::random::<u64>()));
        let tls = dir.to_str().unwrap();
        let joining = Config { node_id: "n2".into(), known_nodes: vec!["10.0.0.1:7946".into()], ..Config::default() };
        let warning = new_ca_warning(&joining, tls).expect("warned");
        assert!(warning.contains("cert issue --node n2"), "{warning}");
        // The first node of a cluster, listing no one: nothing to say.
        assert!(new_ca_warning(&Config { known_nodes: vec![], ..joining.clone() }, tls).is_none());
        // Certificates in place: nothing made, nothing to say.
        pki::issue_cert_to_dir(pki_ca(&dir).as_str(), "n2", tls).unwrap();
        assert!(new_ca_warning(&joining, tls).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A CA beside `dir`, for issuing into it.
    fn pki_ca(dir: &std::path::Path) -> String {
        let ca = dir.with_extension("ca");
        pki::load_or_create(ca.to_str().unwrap(), "ca-node").unwrap();
        ca.to_str().unwrap().to_string()
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
    fn an_unsigned_config_is_not_pulled_over_a_signed_one() {
        let (dir, certs) = cluster_certs(&["admin"]);
        let me = &certs["admin"];
        let tls_ctx = tls::TlsContext::new(me).unwrap();
        let unsigned_file = Config { require_signed_config: true, admin_cns: vec!["admin".into()], ..Config::default() };
        assert!(refuse_unsigned_over_signed("m:1", &unsigned_file, "f.json", me, &tls_ctx).is_ok(), "before the first signed change");
        let signed = signing::sign(&api_on_port(3000), &signing::Signer::from_material(me).unwrap()).unwrap();
        let signed_file = Config { dynamic_config: signed, ..unsigned_file };
        let err = refuse_unsigned_over_signed("m:1", &signed_file, "f.json", me, &tls_ctx).expect_err("refused");
        assert!(err.to_string().contains("isn't admin-signed"), "{err}");
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

    /// A config's revocations are its own cluster's: a CLI using another
    /// cluster's certificates (another CA) doesn't apply them there.
    #[test]
    fn revocations_stay_with_their_own_cluster() {
        let (here, certs) = cluster_certs(&["alice"]);
        let (there, other) = cluster_certs(&["carol"]);
        let mut cfg = Config { tls_dir: Some(here.join("ca").to_str().unwrap().into()), ..Config::default() };
        cfg.dynamic_config.revoked.nodes.push("bob".into());
        let revoked = |ctx: &tls::TlsContext| ctx.revocations().is_revoked(Some("bob"), &[0; 32]);
        assert!(revoked(&cli_tls_context(&cfg, &certs["alice"]).unwrap()), "this cluster");
        assert!(!revoked(&cli_tls_context(&cfg, &other["carol"]).unwrap()), "another cluster");
        // Not knowing this cluster's CA, they apply (refusing is the safe side).
        let unknown = Config { tls_dir: None, ..cfg.clone() };
        assert!(revoked(&cli_tls_context(&unknown, &other["carol"]).unwrap()));
        let _ = (std::fs::remove_dir_all(&here), std::fs::remove_dir_all(&there));
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

    /// A daemon stand-in serving `serve` as its (signed) config, recording
    /// the configs pushed to it. Returns its address and those.
    async fn config_server(
        material: &pki::PkiMaterial,
        serve: config::DynamicConfig,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<config::DynamicConfig>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let tls_ctx = std::sync::Arc::new(tls::TlsContext::new(material).unwrap());
        let pushed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let got = pushed.clone();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let (tls_ctx, serve, got) = (tls_ctx.clone(), serve.clone(), got.clone());
                tokio::spawn(async move {
                    let Ok(mut stream) = tls_ctx.accept(tcp).await else { return };
                    let reply = match gossip::recv_message(&mut stream).await {
                        Ok(Message::StatusRequest) => Message::StatusResponse {
                            mode: "node".into(),
                            node_id: "m".into(),
                            bind: String::new(),
                            dynamic_config: serve,
                            known_nodes: vec![],
                            require_signed_config: true,
                            cert_expires: None,
                            issues_certs: false,
                        },
                        Ok(Message::AdminRequest { op: AdminOp::ConfigPush { config, .. } }) => {
                            let new_version = config.version;
                            got.lock().unwrap().push(config);
                            Message::AdminResponse { result: gossip::messages::AdminResult::Ok { new_version } }
                        }
                        _ => return,
                    };
                    let _ = gossip::send_message(&mut stream, &reply).await;
                });
            }
        });
        (addr, pushed)
    }

    /// A node serving a config older than the one the admin's own file holds
    /// (it is behind, or lying to drop a revocation made since) doesn't get
    /// the admin to sign a change on it.
    #[tokio::test]
    async fn the_cli_wont_build_on_a_config_older_than_its_own() {
        let (dir, certs) = cluster_certs(&["alice", "zoe", "m"]);
        let sign = |dc: &config::DynamicConfig| {
            signing::sign(dc, &signing::Signer::from_material(&certs["alice"]).unwrap()).unwrap()
        };
        let v9 = sign(&config::DynamicConfig { version: 9, ..api_on_port(3000) });
        let mut v10 = config::DynamicConfig { version: 10, ..api_on_port(3000) };
        v10.revoked.nodes.push("x".into());
        let v10 = sign(&v10);
        let zoe = Config {
            require_signed_config: true,
            admin_cns: vec!["alice".into(), "zoe".into()],
            dynamic_config: v10.clone(),
            ..Config::default()
        };
        let zoe_dir = dir.join("zoe");
        let zoe_dir = zoe_dir.to_str();
        let op = || AdminOp::PolicyAllow { service: "api".into(), nodes: vec!["n2".into()] };

        let (behind, pushed) = config_server(&certs["m"], v9.clone()).await;
        let err = try_admin_rpc(&behind, op(), &zoe, "", zoe_dir).await.expect_err("refused");
        assert!(format!("{err:#}").contains("older than the admin-signed version 10"), "{err:#}");
        assert!(pushed.lock().unwrap().is_empty(), "nothing signed");

        let (current, pushed) = config_server(&certs["m"], v10).await;
        assert_eq!(try_admin_rpc(&current, op(), &zoe, "", zoe_dir).await.unwrap(), Ok(11));
        assert_eq!(pushed.lock().unwrap()[0].revoked.nodes, ["x"], "built on v10, its revocation kept");

        // With no signed config of its own to compare with, as before.
        let laptop = Config { dynamic_config: config::DynamicConfig::default(), ..zoe.clone() };
        assert_eq!(try_admin_rpc(&behind, op(), &laptop, "", zoe_dir).await.unwrap(), Ok(10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An admin revokes a leaked admin key with `cert revoke --remote`; a
    /// node holding that key then serves a config forged with it (the key
    /// not revoked, its own policy in force). The admin's next change isn't
    /// signed on top of it: the revocation went into the admin's own file.
    #[tokio::test]
    async fn a_revocation_made_with_remote_protects_the_admin_who_made_it() {
        let (dir, certs) = cluster_certs(&["alice", "rob", "m"]);
        let path = dir.join("alice.json");
        let path = path.to_str().unwrap();
        let alice = signing::Signer::from_material(&certs["alice"]).unwrap();
        let rob = signing::Signer::from_material(&certs["rob"]).unwrap();
        let v10 = signing::sign(&config::DynamicConfig { version: 10, ..api_on_port(3000) }, &alice).unwrap();
        let admins = vec!["alice".to_string(), "rob".to_string()];
        Config { require_signed_config: true, admin_cns: admins, dynamic_config: v10.clone(), ..Config::default() }
            .save(path)
            .unwrap();
        let alice_dir = dir.join("alice");
        let alice_dir = alice_dir.to_str();

        let (honest, _) = config_server(&certs["m"], v10.clone()).await;
        let revoke = AdminOp::Revoke { nodes: vec!["rob".into()], certs: vec![] };
        let alice_cfg = Config::load(path).unwrap();
        assert_eq!(try_admin_rpc(&honest, revoke, &alice_cfg, path, alice_dir).await.unwrap(), Ok(11));
        let kept = Config::load(path).unwrap().dynamic_config;
        assert_eq!((kept.version, kept.revoked.nodes.clone()), (11, vec!["rob".to_string()]), "the file holds what was pushed");

        // Rob's forgery, newer, with rob not revoked.
        let forged = config::DynamicConfig { version: 12, changed_by: "rob".into(), ..api_on_port(6666) };
        let (lying, pushed) = config_server(&certs["m"], signing::sign(&forged, &rob).unwrap()).await;
        let op = AdminOp::PolicyAllow { service: "api".into(), nodes: vec!["n2".into()] };
        let alice_cfg = Config::load(path).unwrap();
        try_admin_rpc(&lying, op, &alice_cfg, path, alice_dir).await.expect_err("the forgery is refused");
        assert!(pushed.lock().unwrap().is_empty(), "nothing signed on it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pushed_config_is_kept_only_over_an_older_unedited_one_of_its_cluster() {
        let (dir, certs) = cluster_certs(&["alice"]);
        let (other_dir, other_certs) = cluster_certs(&["alice"]);
        let path = dir.join("alice.json");
        let path = path.to_str().unwrap();
        let alice = signing::Signer::from_material(&certs["alice"]).unwrap();
        let v10 = signing::sign(&config::DynamicConfig { version: 10, ..api_on_port(3000) }, &alice).unwrap();
        let v11 = signing::sign(&config::DynamicConfig { version: 11, ..api_on_port(3001) }, &alice).unwrap();
        let save = |dc: &config::DynamicConfig| {
            let admins = vec!["alice".to_string()];
            Config { require_signed_config: true, admin_cns: admins, dynamic_config: dc.clone(), ..Config::default() }
                .save(path)
                .unwrap();
        };
        let held = || Config::load(path).unwrap().dynamic_config;

        // Another cluster's config (`service move` to it): not kept.
        save(&v10);
        let theirs = signing::sign(&v11, &signing::Signer::from_material(&other_certs["alice"]).unwrap()).unwrap();
        keep_pushed(path, &theirs, &other_certs["alice"]);
        assert_eq!(held(), v10);
        // Edits not yet pushed, around the signature: not written over.
        let mut edited = v10.clone();
        edited.service_groups[0].services[0].node_port = 4000;
        save(&edited);
        keep_pushed(path, &v11, &certs["alice"]);
        assert_eq!(held(), edited);
        // A newer config: kept.
        save(&v11);
        keep_pushed(path, &v10, &certs["alice"]);
        assert_eq!(held(), v11);
        // An older unedited one of its cluster: replaced.
        save(&v10);
        keep_pushed(path, &v11, &certs["alice"]);
        assert_eq!(held(), v11);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    /// A key whose certificate expired (and was never revoked) signs configs
    /// dated back, inside its life; a node serves one to an admin. The admin's
    /// CLI doesn't build on it (and so sign it over) where its own signed
    /// config says it can't be the cluster's.
    #[tokio::test]
    async fn an_expired_admin_key_cant_get_the_cli_to_sign_its_config() {
        let (dir, certs) = cluster_certs(&["alice", "m"]);
        let issuer = pki::Issuer::load(dir.join("ca").to_str().unwrap(), ::time::Duration::seconds(2)).unwrap().unwrap();
        let (bob_cert, bob_key) = issuer.issue_new("bob").unwrap();
        let bob = signing::Signer::new(bob_cert, &bob_key).unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let admins = vec!["alice".to_string(), "bob".to_string()];
        let alice = signing::Signer::from_material(&certs["alice"]).unwrap();
        let mut v10 = config::DynamicConfig { version: 10, ..api_on_port(3000) };
        v10.revoked.nodes.push("x".into());
        let v10 = signing::sign_at(&v10, &alice, now - 3 * 3600).unwrap();
        let alice_cfg = Config { require_signed_config: true, admin_cns: admins, dynamic_config: v10, ..Config::default() };
        let alice_dir = dir.join("alice");
        let op = || AdminOp::PolicyAllow { service: "api".into(), nodes: vec!["n2".into()] };
        let forge = |version: u64| {
            let dc = config::DynamicConfig { version, changed_by: "bob".into(), ..api_on_port(6666) };
            signing::sign_at(&dc, &bob, now - 2 * 3600).unwrap()
        };

        // Another config at alice's version: refused.
        let (lying, pushed) = config_server(&certs["m"], forge(10)).await;
        let err = try_admin_rpc(&lying, op(), &alice_cfg, "", alice_dir.to_str()).await.expect_err("refused");
        assert!(format!("{err:#}").contains("another config at version 10"), "{err:#}");
        // A newer one, which every other check passes: refused for its
        // expired signer.
        let (lying_newer, pushed_newer) = config_server(&certs["m"], forge(11)).await;
        let err = try_admin_rpc(&lying_newer, op(), &alice_cfg, "", alice_dir.to_str()).await.expect_err("refused");
        assert!(format!("{err:#}").contains("whose certificate expired"), "{err:#}");
        assert!(pushed.lock().unwrap().is_empty() && pushed_newer.lock().unwrap().is_empty(), "nothing signed");
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

    #[tokio::test]
    async fn a_change_goes_to_the_file_only_when_no_daemon_is_listening() {
        let (dir, certs) = cluster_certs(&["s1"]);
        let (other_dir, _) = cluster_certs(&["s1"]);
        let path = dir.join("stouter.json");
        let path = path.to_str().unwrap();
        let config_for = |bind: &str| {
            let cfg = Config {
                node_id: "s1".into(),
                bind: bind.into(),
                tls_dir: Some(dir.join("s1").to_str().unwrap().into()),
                ..Config::default()
            };
            cfg.save(path).unwrap();
        };

        // Nothing listening: the file. (A port held, bound but not
        // listening, so connecting is refused; a port merely let go could
        // go to another test running at the same time.)
        let held = tokio::net::TcpSocket::new_v4().unwrap();
        held.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        config_for(&held.local_addr().unwrap().to_string());
        assert_eq!(change_target(None, path, None).await.unwrap(), None);
        drop(held);

        // A daemon that refuses this CLI's certificate (another cluster's,
        // or an expired one): an error, not an edit behind its back.
        let addr = status_answerer(&certs["s1"], "s1", "node").await;
        config_for(&addr);
        let other = other_dir.join("s1");
        let err = change_target(None, path, other.to_str()).await.expect_err("refused");
        assert!(format!("{err:#}").contains("the change was not made"), "{err:#}");

        // Something else at this config's address: not this daemon's.
        let addr = status_answerer(&certs["s1"], "s2", "node").await;
        config_for(&addr);
        let err = change_target(None, path, None).await.expect_err("another node");
        assert!(format!("{err:#}").contains("is node 's2', not 's1'"), "{err:#}");

        // A listener that never says who it is.
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        config_for(&silent.local_addr().unwrap().to_string());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((tcp, _)) = silent.accept().await {
                held.push(tcp);
            }
        });
        tokio::time::pause();
        let err = change_target(None, path, None).await.expect_err("no answer");
        assert!(format!("{err:#}").contains("the change was not made"), "{err:#}");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other_dir);
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
