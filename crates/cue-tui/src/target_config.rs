use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cue_client::{
    client_config_path, default_socket_path, detected_ssh_hosts, home_dir, legacy_config_path,
    read_config_source,
};
use toml::Value;
use toml::map::Map;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetProfileSummary {
    pub name: String,
    pub transport: String,
    pub detail: String,
    pub source: TargetProfileSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetProfileSource {
    Local,
    Configured,
    AutoDetectedSsh,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSettingsSnapshot {
    pub source_path: PathBuf,
    pub using_legacy_config: bool,
    pub auto_detect_ssh: bool,
    pub default_profile: String,
    pub profiles: Vec<TargetProfileSummary>,
}

impl TargetSettingsSnapshot {
    pub fn contains_profile(&self, profile_name: &str) -> bool {
        self.profiles
            .iter()
            .any(|profile| profile.name == profile_name)
    }
}

pub fn load_target_settings() -> Result<TargetSettingsSnapshot> {
    let client_path = client_config_path();
    let legacy_path = legacy_config_path();
    let detected_hosts = detected_ssh_hosts();

    if let Some(text) = read_config_source(&client_path)? {
        return parse_snapshot(&client_path, false, &text, &detected_hosts);
    }
    if let Some(text) = read_config_source(&legacy_path)? {
        return parse_snapshot(&legacy_path, true, &text, &detected_hosts);
    }

    Ok(TargetSettingsSnapshot {
        source_path: client_path,
        using_legacy_config: false,
        auto_detect_ssh: true,
        default_profile: "local".into(),
        profiles: merged_profile_summaries(None, &detected_hosts),
    })
}

/// Build a [`cue_client::ClientConnector`] for the named target profile by
/// reading the on-disk configuration.
///
/// Only the `"unix"` transport is supported for live reconnects; attempting
/// to build a connector for an `"ssh"` profile returns an error.
pub fn connector_for_profile(profile_name: &str) -> anyhow::Result<cue_client::ClientConnector> {
    let client_path = client_config_path();
    let legacy_path = legacy_config_path();
    let detected_hosts = detected_ssh_hosts();

    let text = if let Some(t) = read_config_source(&client_path)? {
        t
    } else if let Some(t) = read_config_source(&legacy_path)? {
        t
    } else {
        // No config file — fall back to the default local socket.
        if profile_name == "local" {
            return Ok(cue_client::ClientConnector::unix(default_socket_path()));
        }
        if detected_hosts.contains(profile_name) {
            anyhow::bail!(
                "SSH transport does not support live reconnect; \
                     restart cue-tui to apply the profile change"
            );
        }
        anyhow::bail!("no configuration file found; cannot resolve profile `{profile_name}`");
    };

    let document: Value = toml::from_str(&text).context("parse client config")?;
    let auto_detect_ssh = transport_auto_detect_ssh(&document);

    let profiles_table = document
        .get("transport")
        .and_then(Value::as_table)
        .and_then(|t| t.get("profiles"))
        .and_then(Value::as_table);

    let profile_value = match profiles_table {
        Some(profiles) => profiles.get(profile_name),
        None => {
            if profile_name == "local" {
                return Ok(cue_client::ClientConnector::unix(default_socket_path()));
            }
            if auto_detect_ssh && detected_hosts.contains(profile_name) {
                anyhow::bail!(
                    "SSH transport does not support live reconnect; \
                     restart cue-tui to apply the profile change"
                );
            }
            anyhow::bail!("no [transport.profiles] section in config");
        }
    };

    let table = match profile_value {
        Some(v) => v
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("profile `{profile_name}` must be a TOML table"))?,
        None => {
            if profile_name == "local" {
                return Ok(cue_client::ClientConnector::unix(default_socket_path()));
            }
            if auto_detect_ssh && detected_hosts.contains(profile_name) {
                anyhow::bail!(
                    "SSH transport does not support live reconnect; \
                     restart cue-tui to apply the profile change"
                );
            }
            anyhow::bail!("profile `{profile_name}` not found in config");
        }
    };

    let transport = table
        .get("transport")
        .and_then(Value::as_str)
        .unwrap_or("unix");

    match transport {
        "unix" => {
            let socket = table
                .get("socket")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(default_socket_path);
            Ok(cue_client::ClientConnector::unix(socket))
        }
        "ssh" => anyhow::bail!(
            "SSH transport does not support live reconnect; \
             restart cue-tui to apply the profile change"
        ),
        other => anyhow::bail!("unknown transport `{other}` for profile `{profile_name}`"),
    }
}

pub fn save_default_profile(
    profile_name: &str,
    known_snapshot: &TargetSettingsSnapshot,
) -> Result<TargetSettingsSnapshot> {
    if !known_snapshot.contains_profile(profile_name) {
        bail!("unknown target profile `{profile_name}`");
    }

    let write_path = target_settings_write_path(known_snapshot);
    let mut document = match read_config_source(&write_path)? {
        Some(text) => toml::from_str::<Value>(&text)
            .with_context(|| format!("parse config {}", write_path.display()))?,
        None => Value::Table(Map::new()),
    };
    update_default_profile(&mut document, profile_name);

    let serialized =
        toml::to_string_pretty(&document).context("serialize target settings document")?;
    if let Some(parent) = write_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }
    std::fs::write(&write_path, serialized)
        .with_context(|| format!("write config {}", write_path.display()))?;

    let text = std::fs::read_to_string(&write_path)
        .with_context(|| format!("read config {}", write_path.display()))?;
    parse_snapshot(&write_path, false, &text, &detected_ssh_hosts())
}

pub fn display_path(path: &Path) -> String {
    let home = home_dir();
    if let Ok(rest) = path.strip_prefix(&home) {
        if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.display())
        }
    } else {
        path.display().to_string()
    }
}

fn parse_snapshot(
    path: &Path,
    using_legacy_config: bool,
    text: &str,
    detected_hosts: &std::collections::BTreeSet<String>,
) -> Result<TargetSettingsSnapshot> {
    let document: Value =
        toml::from_str(text).with_context(|| format!("parse config {}", path.display()))?;
    Ok(snapshot_from_value(
        path.to_path_buf(),
        using_legacy_config,
        &document,
        detected_hosts,
    ))
}

fn snapshot_from_value(
    source_path: PathBuf,
    using_legacy_config: bool,
    document: &Value,
    detected_hosts: &std::collections::BTreeSet<String>,
) -> TargetSettingsSnapshot {
    let auto_detect_ssh = transport_auto_detect_ssh(document);
    let default_profile = transport_table(document)
        .and_then(|transport| transport.get("default_profile"))
        .and_then(Value::as_str)
        .filter(|profile| !profile.is_empty())
        .unwrap_or("local")
        .to_string();

    let empty_detected = std::collections::BTreeSet::new();
    let mut profiles = merged_profile_summaries(
        transport_table(document)
            .and_then(|transport| transport.get("profiles"))
            .and_then(Value::as_table),
        if auto_detect_ssh {
            detected_hosts
        } else {
            &empty_detected
        },
    );

    if !profiles
        .iter()
        .any(|profile| profile.name == default_profile)
    {
        profiles.push(TargetProfileSummary {
            name: default_profile.clone(),
            transport: "missing".into(),
            detail: "profile is referenced by default_profile but not defined".into(),
            source: TargetProfileSource::Missing,
        });
        profiles.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.transport.cmp(&right.transport))
        });
    }

    TargetSettingsSnapshot {
        source_path,
        using_legacy_config,
        auto_detect_ssh,
        default_profile,
        profiles,
    }
}

fn merged_profile_summaries(
    profiles: Option<&Map<String, Value>>,
    detected_hosts: &std::collections::BTreeSet<String>,
) -> Vec<TargetProfileSummary> {
    let mut summaries = BTreeMap::new();
    summaries.insert(
        "local".to_string(),
        summarize_local_profile(profiles.and_then(|profiles| profiles.get("local"))),
    );

    if let Some(profiles) = profiles {
        for (name, profile) in profiles {
            if name == "local" {
                continue;
            }
            summaries.insert(name.clone(), summarize_profile(name, profile));
        }
    }

    for host in detected_hosts {
        if host == "local" {
            continue;
        }
        summaries
            .entry(host.clone())
            .or_insert_with(|| TargetProfileSummary {
                name: host.clone(),
                transport: "ssh".into(),
                detail: format!("{host} | cued gateway --stdio"),
                source: TargetProfileSource::AutoDetectedSsh,
            });
    }

    let mut profiles = Vec::with_capacity(summaries.len());
    if let Some(local) = summaries.remove("local") {
        profiles.push(local);
    }
    profiles.extend(summaries.into_values());
    profiles
}

fn summarize_profile(name: &str, profile: &Value) -> TargetProfileSummary {
    let Some(table) = profile.as_table() else {
        return TargetProfileSummary {
            name: name.to_string(),
            transport: "invalid".into(),
            detail: "profile value must be a TOML table".into(),
            source: TargetProfileSource::Configured,
        };
    };

    match table
        .get("transport")
        .and_then(Value::as_str)
        .unwrap_or("unix")
    {
        "unix" => TargetProfileSummary {
            name: name.to_string(),
            transport: "unix".into(),
            detail: format!(
                "socket: {}",
                table
                    .get("socket")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| default_socket_path().display().to_string())
            ),
            source: TargetProfileSource::Configured,
        },
        "ssh" => {
            let destination = table
                .get("destination")
                .and_then(Value::as_str)
                .unwrap_or("<missing destination>");
            let gateway_command = table
                .get("gateway_command")
                .and_then(Value::as_str)
                .unwrap_or("cued gateway --stdio");
            TargetProfileSummary {
                name: name.to_string(),
                transport: "ssh".into(),
                detail: format!("{destination} | {gateway_command}"),
                source: TargetProfileSource::Configured,
            }
        }
        other => TargetProfileSummary {
            name: name.to_string(),
            transport: other.to_string(),
            detail: "unrecognized transport kind".into(),
            source: TargetProfileSource::Configured,
        },
    }
}

fn summarize_local_profile(profile: Option<&Value>) -> TargetProfileSummary {
    let socket = profile
        .and_then(Value::as_table)
        .filter(|table| {
            table
                .get("transport")
                .and_then(Value::as_str)
                .unwrap_or("unix")
                == "unix"
        })
        .and_then(|table| table.get("socket"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| default_socket_path().display().to_string());

    TargetProfileSummary {
        name: "local".into(),
        transport: "unix".into(),
        detail: format!("socket: {socket}"),
        source: TargetProfileSource::Local,
    }
}

fn update_default_profile(document: &mut Value, profile_name: &str) {
    let root = ensure_table(document);
    let transport = ensure_child_table(root, "transport");
    transport.insert(
        "default_profile".into(),
        Value::String(profile_name.to_string()),
    );
    let profiles = ensure_child_table(transport, "profiles");
    if profiles.is_empty() {
        profiles.insert("local".into(), default_local_profile_value());
    }
}

fn ensure_table(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_table() {
        *value = Value::Table(Map::new());
    }
    value
        .as_table_mut()
        .expect("value was converted to a table above")
}

fn ensure_child_table<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
) -> &'a mut Map<String, Value> {
    let entry = parent
        .entry(key.to_string())
        .or_insert_with(|| Value::Table(Map::new()));
    if !entry.is_table() {
        *entry = Value::Table(Map::new());
    }
    entry
        .as_table_mut()
        .expect("entry was converted to a table above")
}

#[cfg(test)]
fn default_profiles() -> Vec<TargetProfileSummary> {
    vec![summarize_local_profile(None)]
}

fn default_local_profile_value() -> Value {
    let mut profile = Map::new();
    profile.insert("transport".into(), Value::String("unix".into()));
    Value::Table(profile)
}

fn transport_table(document: &Value) -> Option<&Map<String, Value>> {
    document.get("transport")?.as_table()
}

fn transport_auto_detect_ssh(document: &Value) -> bool {
    transport_table(document)
        .and_then(|transport| transport.get("auto_detect_ssh"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn target_settings_write_path(snapshot: &TargetSettingsSnapshot) -> PathBuf {
    if snapshot.using_legacy_config {
        client_config_path()
    } else {
        snapshot.source_path.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_prefers_client_shape_and_summarizes_profiles() {
        let document: Value = toml::from_str(
            r#"
[transport]
default_profile = "remote"

[transport.profiles.local]
transport = "unix"
socket = "/tmp/cue.sock"

[transport.profiles.remote]
transport = "ssh"
destination = "devbox"
gateway_command = "cued gateway --stdio --socket /tmp/remote.sock"
"#,
        )
        .unwrap();

        let snapshot = snapshot_from_value(
            PathBuf::from("client.toml"),
            false,
            &document,
            &Default::default(),
        );
        assert_eq!(snapshot.default_profile, "remote");
        assert!(snapshot.auto_detect_ssh);
        assert_eq!(snapshot.profiles.len(), 2);
        assert_eq!(
            snapshot.profiles[0],
            TargetProfileSummary {
                name: "local".into(),
                transport: "unix".into(),
                detail: "socket: /tmp/cue.sock".into(),
                source: TargetProfileSource::Local,
            }
        );
        assert_eq!(
            snapshot.profiles[1],
            TargetProfileSummary {
                name: "remote".into(),
                transport: "ssh".into(),
                detail: "devbox | cued gateway --stdio --socket /tmp/remote.sock".into(),
                source: TargetProfileSource::Configured,
            }
        );
    }

    #[test]
    fn update_default_profile_preserves_other_sections() {
        let mut document: Value = toml::from_str(
            r#"
[transport]
default_profile = "local"

[transport.profiles.local]
transport = "unix"

[weft]
socket_path = "./weft.sock"
"#,
        )
        .unwrap();

        update_default_profile(&mut document, "remote");

        assert_eq!(
            document
                .get("transport")
                .and_then(Value::as_table)
                .and_then(|transport| transport.get("default_profile"))
                .and_then(Value::as_str),
            Some("remote")
        );
        assert_eq!(
            document
                .get("weft")
                .and_then(Value::as_table)
                .and_then(|weft| weft.get("socket_path"))
                .and_then(Value::as_str),
            Some("./weft.sock")
        );
    }

    #[test]
    fn snapshot_surfaces_missing_default_profile() {
        let document: Value = toml::from_str(
            r#"
[transport]
default_profile = "remote"

[transport.profiles.local]
transport = "unix"
"#,
        )
        .unwrap();

        let snapshot = snapshot_from_value(
            PathBuf::from("client.toml"),
            false,
            &document,
            &Default::default(),
        );
        assert_eq!(snapshot.default_profile, "remote");
        assert!(
            snapshot
                .profiles
                .iter()
                .any(|profile| profile.name == "remote")
        );
        assert_eq!(
            snapshot
                .profiles
                .iter()
                .find(|profile| profile.name == "remote")
                .map(|profile| profile.transport.as_str()),
            Some("missing")
        );
    }

    #[test]
    fn legacy_snapshot_writes_into_client_toml_path() {
        let snapshot = TargetSettingsSnapshot {
            source_path: PathBuf::from("/tmp/config.toml"),
            using_legacy_config: true,
            auto_detect_ssh: true,
            default_profile: "local".into(),
            profiles: default_profiles(),
        };

        let write_path = target_settings_write_path(&snapshot);

        assert!(write_path.ends_with("cue-shell/client.toml"));
    }

    #[test]
    fn snapshot_adds_detected_ssh_hosts_and_keeps_local() {
        let document: Value = toml::from_str(
            r#"
[transport]
default_profile = "local"

[transport.profiles.remote]
transport = "ssh"
destination = "configured-remote"
"#,
        )
        .unwrap();

        let snapshot = snapshot_from_value(
            PathBuf::from("client.toml"),
            false,
            &document,
            &["devbox".to_string(), "remote".to_string()]
                .into_iter()
                .collect(),
        );

        assert!(
            snapshot
                .profiles
                .iter()
                .any(|profile| profile.name == "local"
                    && profile.transport == "unix"
                    && profile.source == TargetProfileSource::Local)
        );
        assert!(
            snapshot
                .profiles
                .iter()
                .any(|profile| profile.name == "devbox"
                    && profile.transport == "ssh"
                    && profile.source == TargetProfileSource::AutoDetectedSsh)
        );
        assert_eq!(
            snapshot
                .profiles
                .iter()
                .filter(|profile| profile.name == "remote")
                .count(),
            1
        );
    }

    #[test]
    fn local_profile_cannot_be_removed_by_non_unix_config() {
        let document: Value = toml::from_str(
            r#"
[transport]
default_profile = "local"

[transport.profiles.local]
transport = "ssh"
destination = "bad"
"#,
        )
        .unwrap();

        let snapshot = snapshot_from_value(
            PathBuf::from("client.toml"),
            false,
            &document,
            &Default::default(),
        );

        assert_eq!(
            snapshot.profiles.first(),
            Some(&TargetProfileSummary {
                name: "local".into(),
                transport: "unix".into(),
                detail: format!("socket: {}", default_socket_path().display()),
                source: TargetProfileSource::Local,
            })
        );
    }

    #[test]
    fn snapshot_respects_disabled_auto_detect_ssh() {
        let document: Value = toml::from_str(
            r#"
[transport]
auto_detect_ssh = false
"#,
        )
        .unwrap();

        let snapshot = snapshot_from_value(
            PathBuf::from("client.toml"),
            false,
            &document,
            &["devbox".to_string()].into_iter().collect(),
        );

        assert!(!snapshot.auto_detect_ssh);
        assert!(
            snapshot
                .profiles
                .iter()
                .any(|profile| profile.name == "local")
        );
        assert!(
            !snapshot
                .profiles
                .iter()
                .any(|profile| profile.name == "devbox")
        );
    }
}
