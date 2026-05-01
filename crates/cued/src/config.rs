use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::dirs;

const SERVER_CONFIG_FILE: &str = "server.toml";
const LEGACY_CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub aliases: AliasConfig,
    #[serde(default)]
    pub weft: WeftConfig,
}

#[derive(Debug, Clone)]
pub struct AliasEntry {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default)]
pub struct AliasConfig {
    pub entries: Vec<AliasEntry>,
}

impl<'de> Deserialize<'de> for AliasConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let map = BTreeMap::<String, String>::deserialize(deserializer)?;
        let mut entries: Vec<AliasEntry> = map
            .into_iter()
            .map(|(from, to)| AliasEntry { from, to })
            .collect();
        entries.sort_by(|a, b| {
            b.from
                .split_whitespace()
                .count()
                .cmp(&a.from.split_whitespace().count())
        });
        Ok(AliasConfig { entries })
    }
}

impl AliasConfig {
    /// Apply the longest-matching alias to `input` and return the substituted string.
    ///
    /// Alias matching compares against the first N whitespace-separated tokens of
    /// `input`. Longer patterns take priority over shorter ones. Input that begins
    /// with `:` (an explicit colon-command such as `:run`) is intentionally **not**
    /// aliased — the caller is already using the explicit command syntax.
    pub fn apply(&self, input: &str) -> String {
        if self.entries.is_empty() || input.starts_with(':') {
            return input.to_string();
        }
        let input_tokens: Vec<&str> = input.split_whitespace().collect();
        for entry in &self.entries {
            let from_tokens: Vec<&str> = entry.from.split_whitespace().collect();
            let n = from_tokens.len();
            if input_tokens.len() >= n && input_tokens[..n] == from_tokens[..] {
                let rest = &input_tokens[n..];
                return if rest.is_empty() {
                    entry.to.clone()
                } else {
                    format!("{} {}", entry.to, rest.join(" "))
                };
            }
        }
        input.to_string()
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_dir = dirs::config_dir();
        let server_path = config_dir.join(SERVER_CONFIG_FILE);
        let legacy_path = config_dir.join(LEGACY_CONFIG_FILE);
        Self::load_from_sources(
            read_source(&server_path)?
                .as_deref()
                .map(|text| (server_path.as_path(), text)),
            read_source(&legacy_path)?
                .as_deref()
                .map(|text| (legacy_path.as_path(), text)),
        )
    }

    fn load_from_sources(
        server: Option<(&Path, &str)>,
        legacy: Option<(&Path, &str)>,
    ) -> Result<Self> {
        if let Some((path, text)) = server {
            return Self::parse(text, path);
        }
        if let Some((path, text)) = legacy {
            return Self::parse(text, path);
        }
        Ok(Self::default())
    }

    fn parse(text: &str, path: &Path) -> Result<Self> {
        toml::from_str(text).with_context(|| format!("parse config {}", path.display()))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WeftConfig {
    #[serde(default = "default_weft_socket_path")]
    pub socket_path: PathBuf,
}

impl Default for WeftConfig {
    fn default() -> Self {
        Self {
            socket_path: default_weft_socket_path(),
        }
    }
}

fn default_weft_socket_path() -> PathBuf {
    PathBuf::from("./weft.sock")
}

fn read_source(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }

    let text =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn default_config_sets_weft_socket() {
        let config = Config::default();
        assert_eq!(config.weft.socket_path, PathBuf::from("./weft.sock"));
    }

    #[test]
    fn server_toml_takes_precedence_over_legacy_config_toml() {
        let config = Config::load_from_sources(
            Some((
                Path::new("server.toml"),
                r#"
[weft]
socket_path = "/tmp/server.sock"
"#,
            )),
            Some((
                Path::new("config.toml"),
                r#"
[weft]
socket_path = "/tmp/legacy.sock"
"#,
            )),
        )
        .expect("load config");

        assert_eq!(config.weft.socket_path, PathBuf::from("/tmp/server.sock"));
    }

    #[test]
    fn legacy_config_toml_still_loads_weft_config() {
        let config = Config::load_from_sources(
            None,
            Some((
                Path::new("config.toml"),
                r#"
[weft]
socket_path = "/tmp/legacy.sock"
"#,
            )),
        )
        .expect("load config");

        assert_eq!(config.weft.socket_path, PathBuf::from("/tmp/legacy.sock"));
    }

    #[test]
    fn alias_no_match_passthrough() {
        let cfg = AliasConfig::default();
        assert_eq!(cfg.apply("pip install foo"), "pip install foo");
    }

    #[test]
    fn alias_single_word() {
        let cfg: AliasConfig = toml::from_str(r#"pip = "uv pip""#).unwrap();
        assert_eq!(cfg.apply("pip install foo"), "uv pip install foo");
        assert_eq!(cfg.apply("pip"), "uv pip");
    }

    #[test]
    fn alias_multi_word() {
        let cfg: AliasConfig = toml::from_str(r#""git clone" = "ein clone""#).unwrap();
        assert_eq!(
            cfg.apply("git clone https://github.com/foo/bar"),
            "ein clone https://github.com/foo/bar"
        );
    }

    #[test]
    fn alias_longer_match_takes_priority() {
        let cfg: AliasConfig = toml::from_str(
            r#"
git = "alt-git"
"git clone" = "ein clone"
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.apply("git clone https://github.com/foo/bar"),
            "ein clone https://github.com/foo/bar"
        );
        assert_eq!(cfg.apply("git status"), "alt-git status");
    }

    #[test]
    fn alias_no_match_in_middle() {
        let cfg: AliasConfig = toml::from_str(r#"pip = "uv pip""#).unwrap();
        assert_eq!(cfg.apply("run pip install foo"), "run pip install foo");
    }

    #[test]
    fn alias_empty_input() {
        let cfg: AliasConfig = toml::from_str(r#"pip = "uv pip""#).unwrap();
        assert_eq!(cfg.apply(""), "");
    }

    #[test]
    fn alias_parsed_from_server_toml() {
        let config = Config::load_from_sources(
            Some((
                Path::new("server.toml"),
                r#"
[aliases]
"git clone" = "ein clone"
pip = "uv pip"
"#,
            )),
            None,
        )
        .expect("load config");
        assert_eq!(
            config.aliases.apply("git clone https://example.com"),
            "ein clone https://example.com"
        );
        assert_eq!(
            config.aliases.apply("pip install foo"),
            "uv pip install foo"
        );
    }

    #[test]
    fn parses_weft_socket_path() {
        let config = Config::load_from_sources(
            Some((
                Path::new("server.toml"),
                r#"
[weft]
socket_path = "/var/run/weft.sock"
"#,
            )),
            None,
        )
        .expect("load config");

        assert_eq!(config.weft.socket_path, PathBuf::from("/var/run/weft.sock"));
    }
}
