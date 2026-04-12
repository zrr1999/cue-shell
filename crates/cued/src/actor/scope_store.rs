//! ScopeStore actor — environment scope management.
//!
//! Maintains an in-memory cache backed by SQLite.  The "HEAD" pointer
//! tracks the current active scope (analogous to git HEAD).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use rusqlite::Connection;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use cue_core::ScopeHash;
use cue_core::scope::{EnvSnapshot, Scope};

use super::{ActorSystem, EventBusMsg, ScopeStoreMsg};
use crate::storage;

use cue_core::ipc::EventPayload;

/// Spawn the ScopeStore actor task.
///
/// Initialises a root scope from the current process environment.
pub fn spawn(mut rx: mpsc::Receiver<ScopeStoreMsg>, conn: Connection, sys: ActorSystem) {
    tokio::spawn(async move {
        let mut cache: HashMap<ScopeHash, Scope> = HashMap::new();

        // Build root scope from the real environment.
        let env: BTreeMap<String, String> = std::env::vars().collect();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let snapshot = EnvSnapshot { env, cwd };
        let root = Scope::root(snapshot);
        let head = root.hash;

        // Persist root.
        if let Err(e) = storage::insert_scope(&conn, &root) {
            error!("scope_store: failed to persist root scope: {e}");
        }
        if let Err(e) = storage::set_head(&conn, &head) {
            error!("scope_store: failed to set initial head: {e}");
        }
        cache.insert(root.hash, root);
        let mut current_head = head;

        info!(%current_head, "scope_store: started with root scope");

        while let Some(msg) = rx.recv().await {
            match msg {
                ScopeStoreMsg::GetHead { reply } => {
                    let _ = reply.send(current_head);
                }

                ScopeStoreMsg::GetScope { hash, reply } => {
                    // Check cache first, then SQLite.
                    let scope = if let Some(s) = cache.get(&hash) {
                        Some(s.clone())
                    } else {
                        match storage::get_scope(&conn, &hash) {
                            Ok(Some(s)) => {
                                cache.insert(s.hash, s.clone());
                                Some(s)
                            }
                            Ok(None) => None,
                            Err(e) => {
                                error!("scope_store: db error: {e}");
                                None
                            }
                        }
                    };
                    let _ = reply.send(scope);
                }

                ScopeStoreMsg::GetHeadSnapshot { reply } => {
                    let snap = cache.get(&current_head).and_then(|s| s.snapshot.clone());
                    let _ = reply.send(snap);
                }

                ScopeStoreMsg::CreateRoot { snapshot, reply } => {
                    let scope = Scope::root(snapshot);
                    let hash = scope.hash;
                    if let Err(e) = storage::insert_scope(&conn, &scope) {
                        error!("scope_store: persist root failed: {e}");
                    }
                    cache.insert(hash, scope);

                    let old_hash = current_head;
                    current_head = hash;
                    let _ = storage::set_head(&conn, &current_head);

                    let _ = sys
                        .event_bus
                        .send(EventBusMsg::Publish {
                            payload: EventPayload::HeadChanged {
                                old_hash: old_hash.to_string(),
                                new_hash: current_head.to_string(),
                            },
                            channel: "scopes".into(),
                        })
                        .await;

                    let _ = reply.send(hash);
                }

                ScopeStoreMsg::Fork { delta, reply } => {
                    let parent_scope = cache.get(&current_head).cloned();
                    let Some(parent) = parent_scope else {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "HEAD scope {} not in cache",
                            current_head
                        )));
                        continue;
                    };
                    let Some(ref parent_snap) = parent.snapshot else {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "HEAD scope {} has no snapshot",
                            current_head
                        )));
                        continue;
                    };

                    let child = Scope::fork(current_head, parent_snap, delta);
                    let child_hash = child.hash;
                    if let Err(e) = storage::insert_scope(&conn, &child) {
                        error!("scope_store: persist fork failed: {e}");
                    }
                    cache.insert(child_hash, child);

                    let old_hash = current_head;
                    current_head = child_hash;
                    let _ = storage::set_head(&conn, &current_head);

                    let _ = sys
                        .event_bus
                        .send(EventBusMsg::Publish {
                            payload: EventPayload::HeadChanged {
                                old_hash: old_hash.to_string(),
                                new_hash: current_head.to_string(),
                            },
                            channel: "scopes".into(),
                        })
                        .await;

                    let _ = reply.send(Ok(child_hash));
                }

                ScopeStoreMsg::Shutdown => {
                    debug!("scope_store: shutting down");
                    break;
                }
            }
        }

        debug!("scope_store: stopped");
    });
}
