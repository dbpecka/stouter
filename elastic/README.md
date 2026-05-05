# Stouter — Elastic Stack Observability

Everything-as-code artifacts for Kibana dashboards and alerting rules.

## Contents

| File | Purpose |
|------|---------|
| `saved-objects.ndjson` | Kibana data views, Lens visualizations, and dashboards |
| `alert-rules/` | Kibana alerting rules (one JSON file per rule) |
| `import.sh` | Script to push all artifacts into a running Kibana instance |

## Prerequisites

- Kibana 8.x (tested against 8.11+)
- `curl` and `jq` installed locally
- Logs and metrics being shipped into Elasticsearch (see **Data Ingestion** below)

## Data Ingestion

### Logs (`logs-stouter-*`)

Stouter writes structured text logs via `tracing-subscriber`. Collect them with
Elastic Agent or Filebeat using the Docker/container log input. The integration
should parse the log level into `log.level` and the timestamp into `@timestamp`.

Minimum expected fields:

| Field | Example |
|-------|---------|
| `@timestamp` | `2024-01-01T00:00:00.000Z` |
| `log.level` | `info`, `warn`, `error` |
| `message` | full log line |

### Metrics (`metrics-stouter-*`)

Stouter exposes a Prometheus endpoint on its listen address. Scrape it with
**Elastic Agent → Prometheus integration** (or Metricbeat prometheus module).
The integration produces documents with this field structure:

| Field | Example |
|-------|---------|
| `@timestamp` | `2024-01-01T00:00:00.000Z` |
| `prometheus.metrics.<name>` | `prometheus.metrics.stouter_known_nodes: 3` |
| `prometheus.labels.<label>` | `prometheus.labels.result: "ok"` |

Key metric families ingested by the dashboards:

- `stouter_known_nodes` — peer count (gauge)
- `stouter_in_flight_connections` — active connections (gauge)
- `stouter_mux_sessions` — yamux sessions (gauge)
- `stouter_config_version` — dynamic config version (gauge)
- `stouter_gossip_syncs_total{result}` — gossip sync counter
- `stouter_gossip_sync_duration_seconds_{sum,count}` — gossip timing histogram
- `stouter_tunnel_requests_total{service, node_id}` — tunnel request counter
- `stouter_tunnel_errors_total{service, reason}` — tunnel error counter
- `stouter_connections_total{type}` — connection counter by type
- `stouter_proxy_duration_seconds_{sum,count}{service}` — proxy timing histogram
- `stouter_tunnel_connect_duration_seconds_{sum,count}{method}` — connect timing histogram

> **Note:** The duration visualizations plot the histogram `_sum` field.
> Because Prometheus histograms use cumulative counters, these charts show the
> total time accumulated per scrape interval rather than instantaneous latency.
> For true rate-of-change you can refine the Lens formula using `differences()`
> or switch to the Observability custom threshold rule type once you have the
> appropriate license.

## Import

```sh
export KIBANA_URL=https://kibana.example.com:5601
export KIBANA_USER=elastic
export KIBANA_PASSWORD=changeme

./elastic/import.sh
```

To target a non-default Kibana space:

```sh
KIBANA_SPACE=production ./elastic/import.sh
```

The script is idempotent for **saved objects** (dashboards are overwritten on
re-import). Alert rules are created fresh on each run — remove duplicates in
Kibana → Stack Management → Rules if you re-import.

## Dashboards

### Stouter: Cluster Overview

Cluster health at a glance.

| Panel | What it shows |
|-------|--------------|
| Known Nodes | Current peer count (latest gauge value) |
| In-Flight Connections | Active connections right now |
| Active Mux Sessions | Open yamux multiplexed sessions |
| Config Version | Current dynamic config version across the cluster |
| Known Nodes Over Time | Peer count trend — drops indicate nodes leaving |
| Gossip Syncs by Result | Stacked bar of `ok` / `timeout` / `error` sync outcomes |
| Gossip Sync Duration | Average cumulative sync time — rising trend = slower gossip |

### Stouter: Traffic & Errors

Traffic flow and failure rates.

| Panel | What it shows |
|-------|--------------|
| Tunnel Requests by Service | Request volume per service over time |
| Tunnel Errors by Reason | Error breakdown: `no_nodes` vs `connection_error` |
| Connections by Type | Stacked bar: `gossip`, `tunnel`, `reverse`, `mux`, `admin` |
| Proxy Duration by Service | Cumulative time proxied connections spend per service |
| Tunnel Connect Duration by Method | Connect time per method: `mux`, `reverse`, `tunnel_pool`, `cold` |

### Stouter: Logs

Log signal overview.

| Panel | What it shows |
|-------|--------------|
| Log Level Distribution | Pie chart of info / warn / error proportions |
| Error and Warn Logs Over Time | Stacked bar of warn + error events — spikes signal incidents |

## Alert Rules

All rules use the `.es-query` rule type (available on all Elastic license tiers)
and fire on the **Alerts** consumer. Wire up notification actions in
Kibana → Stack Management → Rules after import.

| Rule | Severity | Fires when |
|------|----------|-----------|
| No Nodes Available for Service | warning | A `"no nodes available"` warn log appears in the last 5 min |
| Subscribe REST API Crashed | critical | A `"REST API server exited"` error log appears in the last 5 min |
| High Proxy Error Rate | warning | ≥ 5 `"proxy connection error"` warn logs in the last 5 min |
| Node Has No Known Peers | critical | Any metric sample shows `stouter_known_nodes ≤ 0` in the last 5 min |

Adjust the thresholds directly in the JSON files before importing to suit your
environment.
