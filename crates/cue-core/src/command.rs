use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::resource::{Need, ResourceQuantity};

/// A typed parameter value used in mode params `()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ParamValue {
    Str(String),
    Bool(bool),
}

/// Mode parameters extracted from `:cmd(k=v, ...)` syntax.
///
/// Per-invocation overrides merged with daemon.toml defaults by the Resolver.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModeParams {
    pub params: BTreeMap<String, ParamValue>,
}

impl ModeParams {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&ParamValue> {
        self.params.get(key)
    }

    pub fn insert(&mut self, key: impl Into<String>, value: ParamValue) {
        self.params.insert(key.into(), value);
    }

    /// Get explicit working directory override, if specified.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        match self.get("cwd") {
            Some(ParamValue::Str(s)) => Some(std::path::PathBuf::from(s)),
            _ => None,
        }
    }

    /// Get runtime wrapper override, if specified.
    /// `None` means use the global config default.
    pub fn wrapper_enabled(&self) -> Option<bool> {
        match self.get("wrapper") {
            Some(ParamValue::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    /// Whether the submitted chain may apply scope-transform leaves (`cd`,
    /// `env set`) to its chain scope. Defaults to false when unspecified.
    pub fn scope(&self) -> Option<bool> {
        match self.get("scope") {
            Some(ParamValue::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    /// Whether to allocate a PTY for the spawned command.
    /// Defaults to `true` (PTY). Set `pty=false` for non-interactive
    /// commands that should run with plain pipes.
    pub fn pty_enabled(&self) -> bool {
        match self.get("pty") {
            Some(ParamValue::Bool(b)) => *b,
            _ => true, // default: PTY on
        }
    }

    /// Collect resource needs declared via `need.<X>=Y` mode params.
    ///
    /// `<X>` is opaque to cue-core; the daemon's `ProviderRegistry` is the
    /// only thing that decides what each key means. Values that fail
    /// `ResourceQuantity::from_str` are skipped silently — the parser is
    /// responsible for rejecting malformed `need.*` values up-front, and
    /// the daemon also re-validates the resulting `Need` against
    /// registered providers, so silent skip here keeps `needs()` total.
    ///
    /// Returns an empty `Need` when no `need.*` params are present.
    pub fn needs(&self) -> Need {
        let mut out = BTreeMap::new();
        for (k, v) in &self.params {
            let Some(suffix) = k.strip_prefix("need.") else {
                continue;
            };
            if suffix.is_empty() {
                continue;
            }
            let ParamValue::Str(s) = v else { continue };
            if let Ok(q) = s.parse::<ResourceQuantity>() {
                out.insert(suffix.to_owned(), q);
            }
        }
        Need::from_pairs(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pv_str(s: &str) -> ParamValue {
        ParamValue::Str(s.to_owned())
    }

    #[test]
    fn needs_extracts_namespaced_keys_only() {
        let mut mp = ModeParams::new();
        mp.insert("pty", ParamValue::Bool(false));
        mp.insert("need.gpu", pv_str("1"));
        mp.insert("need.gpu_mem", pv_str("24GiB"));
        mp.insert("cwd", pv_str("/tmp"));

        let n = mp.needs();
        assert_eq!(n.len(), 2);
        assert!(n.contains("gpu"));
        assert!(n.contains("gpu_mem"));
        // Non-`need.` keys must not leak in.
        assert!(!n.contains("pty"));
        assert!(!n.contains("cwd"));
        // Values are typed.
        assert_eq!(n.get("gpu"), Some(ResourceQuantity::Count(1)));
        assert_eq!(
            n.get("gpu_mem"),
            Some(ResourceQuantity::Bytes(24 * 1024 * 1024 * 1024))
        );
    }

    #[test]
    fn needs_is_empty_when_no_namespace_params() {
        let mut mp = ModeParams::new();
        mp.insert("pty", ParamValue::Bool(false));
        assert!(mp.needs().is_empty());
    }

    #[test]
    fn needs_silently_skips_malformed_quantities() {
        // The parser is the gate that rejects garbage; this method must
        // remain total because some call sites (Resolver) want to keep
        // making progress.
        let mut mp = ModeParams::new();
        mp.insert("need.gpu", pv_str("not-a-number"));
        mp.insert("need.gpu_mem", pv_str("24GiB"));
        let n = mp.needs();
        assert_eq!(n.len(), 1);
        assert!(n.contains("gpu_mem"));
    }

    #[test]
    fn needs_skips_bool_values() {
        let mut mp = ModeParams::new();
        mp.insert("need.gpu", ParamValue::Bool(true));
        assert!(mp.needs().is_empty());
    }

    #[test]
    fn needs_skips_empty_suffix() {
        let mut mp = ModeParams::new();
        mp.insert("need.", pv_str("1"));
        assert!(mp.needs().is_empty());
    }
}
