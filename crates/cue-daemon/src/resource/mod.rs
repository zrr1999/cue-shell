//! Resource scheduling for the cue-daemon.
//!
#![allow(dead_code, unused_imports)]
// Some provider/diagnostic APIs are intentionally introduced before the
// follow-up scheduler-admission, NVML, CLI-provider, and CLI-surface tasks
// consume every method/re-export.

//! Public surface:
//!
//! * [`Provider`] — the pluggable resource backend trait.
//! * [`ProviderRegistry`] — atomically routes a job's `Need` map to the
//!   right providers and tracks reservations per `JobId`.
//! * [`MockProvider`] — used by registry/scheduler tests; also reachable
//!   from cross-crate integration tests via `cue_daemon::resource::mock`.
//!
//! Specific provider implementations (NVML, CLI-shelled-out) live under
//! `providers/` and are introduced by later tasks. The registry itself
//! has no GPU/NVML/TPU knowledge.

pub mod mock;
pub mod provider;
pub mod registry;

#[cfg(test)]
pub(crate) use mock::{MockBehaviour, MockProvider, mock_provider};
#[cfg(test)]
pub(crate) use provider::Provider;
pub(crate) use provider::RejectGroup;
pub(crate) use registry::ProviderRegistry;
