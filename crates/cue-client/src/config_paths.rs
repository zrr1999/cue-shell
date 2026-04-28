use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const APP_DIR: &str = "cue-shell";
const CLIENT_CONFIG_FILE: &str = "client.toml";
const LEGACY_CONFIG_FILE: &str = "config.toml";

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(dir).join(APP_DIR)
    } else {
        home_dir().join(".config").join(APP_DIR)
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::current_dir())
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn client_config_path() -> PathBuf {
    config_dir().join(CLIENT_CONFIG_FILE)
}

pub fn legacy_config_path() -> PathBuf {
    config_dir().join(LEGACY_CONFIG_FILE)
}

pub fn read_config_source(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }

    let text =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    Ok(Some(text))
}
