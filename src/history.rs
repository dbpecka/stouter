//! Config history: each version of the dynamic config a node took, with
//! when, who made it, and what changed, so a bad change can be found and
//! rolled back (`stouter config history`, `stouter config rollback`).
//!
//! A daemon records every version it takes (a CLI changing the file with
//! no daemon running does too) in `<config>.history` beside its config
//! file, newest last, keeping the last [`MAX_ENTRIES`] (and at most
//! [`MAX_BYTES`] of them). It is this node's view: a version that came and
//! went between two of its syncs isn't in it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::DynamicConfig;

/// Most versions kept.
pub const MAX_ENTRIES: usize = 50;
/// Most bytes of history kept (configs can be up to 2 MiB each).
const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Most change lines recorded for one version.
const MAX_CHANGES: usize = 20;

/// One version of the config, as this node took it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub version: u64,
    /// The node (or CLI host) that made the change.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub changed_by: String,
    /// When this node took it (Unix milliseconds).
    pub at_ms: u64,
    /// The admin whose certificate signed it, by its CN (as the signature
    /// names it: checked when it was taken, not here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_by: Option<String>,
    /// What changed from the version before it in this history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
    /// The config itself. Left out of listings sent over the network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<DynamicConfig>,
}

#[derive(Default, Serialize, Deserialize)]
struct HistoryFile {
    entries: Vec<HistoryEntry>,
}

/// The history file for the config at `config_path`.
pub fn path_for(config_path: &str) -> String {
    format!("{config_path}.history")
}

/// The recorded versions, oldest first. A missing or unreadable history is
/// empty (it is a convenience, never needed to run).
pub fn load(config_path: &str) -> Vec<HistoryEntry> {
    std::fs::read(path_for(config_path))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<HistoryFile>(&bytes).ok())
        .map(|file| file.entries)
        .unwrap_or_default()
}

/// Record `dc` as the config now in force, unless it is the version last
/// recorded. Failures are logged, not returned: history must never stop a
/// config change.
pub fn record(config_path: &str, dc: &DynamicConfig) {
    let mut entries = load(config_path);
    let last = entries.last().and_then(|e| e.config.as_ref());
    if last.is_some_and(|last| last.version == dc.version && last.same_settings(dc)) {
        return;
    }
    // Another config at a version already recorded: one that lost a
    // tie-break to this one. It replaces the entry (one per version), so
    // `config rollback --to` that version puts back what this node took.
    entries.retain(|e| e.version != dc.version);
    let last = entries.last().and_then(|e| e.config.as_ref());
    let signed_by = dc.signature.as_ref().and_then(|sig| {
        let der: Option<Vec<u8>> = (0..sig.signer_cert.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(sig.signer_cert.get(i..i + 2)?, 16).ok())
            .collect();
        crate::tls::cert_cn(&der?.into())
    });
    entries.push(HistoryEntry {
        version: dc.version,
        changed_by: dc.changed_by.clone(),
        at_ms: now_ms(),
        signed_by,
        changes: describe_changes(last, dc),
        config: Some(dc.clone()),
    });
    // Keep the newest that fit.
    let excess = entries.len().saturating_sub(MAX_ENTRIES);
    entries.drain(..excess);
    while entries.len() > 1 && serde_json::to_vec(&entries).map_or(0, |b| b.len()) > MAX_BYTES {
        entries.remove(0);
    }
    let path = path_for(config_path);
    let result = serde_json::to_vec_pretty(&HistoryFile { entries })
        .map_err(std::io::Error::other)
        .and_then(|bytes| crate::config::write_atomic(Path::new(&path), &bytes));
    if let Err(e) = result {
        tracing::warn!("can't record config version {} in {path}: {e}", dc.version);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// What changed from `before` to `after`, one short line each (services,
/// routes, policies, revocations), at most [`MAX_CHANGES`] lines.
pub fn describe_changes(before: Option<&DynamicConfig>, after: &DynamicConfig) -> Vec<String> {
    let Some(before) = before else { return vec!["(first version recorded here)".into()] };
    let mut changes = Vec::new();

    let services = |dc: &DynamicConfig| -> BTreeMap<(String, String, String), String> {
        dc.service_groups
            .iter()
            .flat_map(|g| g.services.iter().map(move |s| ((g.name.clone(), s.name.clone(), s.node_id.clone()), s)))
            .map(|(key, s)| (key, serde_json::to_string(s).unwrap_or_default()))
            .collect()
    };
    let (old, new) = (services(before), services(after));
    for (key @ (group, name, node), svc) in &new {
        match old.get(key) {
            None => changes.push(format!("service {group}/{name} on {node} added")),
            Some(prev) if prev != svc => changes.push(format!("service {group}/{name} on {node} changed")),
            _ => {}
        }
    }
    for (group, name, node) in old.keys().filter(|key| !new.contains_key(*key)) {
        changes.push(format!("service {group}/{name} on {node} removed"));
    }

    let routed = |dc: &DynamicConfig| -> BTreeMap<String, String> {
        let mut by_service: BTreeMap<String, Vec<&crate::config::Route>> = BTreeMap::new();
        for route in &dc.routes {
            by_service.entry(route.service.clone()).or_default().push(route);
        }
        by_service.into_iter().map(|(svc, routes)| (svc, serde_json::to_string(&routes).unwrap_or_default())).collect()
    };
    let (old, new) = (routed(before), routed(after));
    for svc in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        if old.get(svc) != new.get(svc) {
            changes.push(match new.get(svc) {
                None => format!("routes for {svc} cleared"),
                Some(_) => format!("routes for {svc} set"),
            });
        }
    }

    let policies = |dc: &DynamicConfig| -> BTreeMap<String, Vec<String>> {
        dc.policies.iter().map(|p| (p.service.clone(), p.allow.clone())).collect()
    };
    let (old, new) = (policies(before), policies(after));
    for svc in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        match (old.get(svc), new.get(svc)) {
            (None, Some(allow)) => changes.push(format!("policy for {svc} added: {}", list(allow))),
            (Some(_), None) => changes.push(format!("policy for {svc} removed: open to every node")),
            (Some(a), Some(b)) if a != b => changes.push(format!("policy for {svc}: {}", list(b))),
            _ => {}
        }
    }

    let short = |fp: &String| fp.chars().take(16).collect::<String>();
    for node in after.revoked.nodes.iter().filter(|n| !before.revoked.nodes.contains(n)) {
        changes.push(format!("node {node} revoked"));
    }
    for node in before.revoked.nodes.iter().filter(|n| !after.revoked.nodes.contains(n)) {
        changes.push(format!("node {node} unrevoked"));
    }
    for cert in after.revoked.certs.iter().filter(|c| !before.revoked.certs.contains(c)) {
        changes.push(format!("certificate {}… revoked", short(cert)));
    }
    for cert in before.revoked.certs.iter().filter(|c| !after.revoked.certs.contains(c)) {
        changes.push(format!("certificate {}… unrevoked", short(cert)));
    }

    if changes.is_empty() {
        changes.push("no change in services, routes, policies or revocations".into());
    }
    if changes.len() > MAX_CHANGES {
        let more = changes.len() - (MAX_CHANGES - 1);
        changes.truncate(MAX_CHANGES - 1);
        changes.push(format!("… and {more} more"));
    }
    changes
}

fn list(nodes: &[String]) -> String {
    if nodes.is_empty() {
        "only the nodes hosting it".into()
    } else {
        nodes.join(", ")
    }
}

/// `ms` (Unix milliseconds) as `YYYY-MM-DD HH:MM:SS UTC`.
pub fn format_time(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dc(json: serde_json::Value) -> DynamicConfig {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn each_version_is_recorded_once_with_what_changed() {
        let dir = std::env::temp_dir().join(format!("stouter_history_{}_{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stouter.json");
        let path = path.to_str().unwrap();
        assert!(load(path).is_empty(), "no history yet");

        let v1 = dc(serde_json::json!({"version": 1, "serviceGroups": [{"name": "g", "services": [
            {"name": "api", "nodeId": "n1", "nodePort": 80}]}]}));
        record(path, &v1);
        record(path, &v1);
        assert_eq!(load(path).len(), 1, "the same version again isn't recorded twice");

        let v2 = dc(serde_json::json!({"version": 2, "changedBy": "admin", "serviceGroups": [{"name": "g", "services": [
            {"name": "api", "nodeId": "n1", "nodePort": 81}, {"name": "db", "nodeId": "n2", "nodePort": 5432}]}],
            "policies": [{"service": "db", "allow": ["n1"]}], "revoked": {"nodes": ["old"]}}));
        record(path, &v2);
        let entries = load(path);
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[1].version, entries[1].changed_by.as_str()), (2, "admin"));
        assert_eq!(
            entries[1].changes,
            [
                "service g/api on n1 changed",
                "service g/db on n2 added",
                "policy for db added: n1",
                "node old revoked"
            ]
        );
        assert_eq!(entries[1].config.as_ref(), Some(&v2));

        // Another config at version 2 (the tie-break went the other way):
        // it replaces the entry, described against version 1.
        let v2b = DynamicConfig { changed_by: "other".into(), ..v1.clone() };
        let v2b = DynamicConfig { version: 2, ..v2b };
        record(path, &v2b);
        let entries = load(path);
        assert_eq!(entries.len(), 2, "one entry per version");
        assert_eq!((entries[1].version, entries[1].changed_by.as_str()), (2, "other"));
        assert_eq!(entries[1].changes, ["no change in services, routes, policies or revocations"], "against version 1");
        record(path, &v2);

        // Only the newest are kept.
        for v in 3..(MAX_ENTRIES as u64 + 10) {
            record(path, &DynamicConfig { version: v, ..v2.clone() });
        }
        let entries = load(path);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries.last().unwrap().version, MAX_ENTRIES as u64 + 9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removals_and_reopenings_are_described() {
        let before = dc(serde_json::json!({"version": 1, "serviceGroups": [{"name": "g", "services": [
            {"name": "api", "nodeId": "n1", "nodePort": 80, "protocol": "http"}]}],
            "routes": [{"service": "api", "backends": [{}]}],
            "policies": [{"service": "api", "allow": ["n2"]}]}));
        let after = dc(serde_json::json!({"version": 2, "serviceGroups": []}));
        assert_eq!(
            describe_changes(Some(&before), &after),
            ["service g/api on n1 removed", "routes for api cleared", "policy for api removed: open to every node"]
        );
        assert_eq!(describe_changes(Some(&after), &after), ["no change in services, routes, policies or revocations"]);
    }

    #[test]
    fn times_read_as_utc_dates() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_time(1_700_000_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(format_time(951_782_400_000), "2000-02-29 00:00:00 UTC");
    }
}
