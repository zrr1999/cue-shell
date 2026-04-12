//! EventBus actor — fan-out event delivery to subscribed clients.
//!
//! Supports exact channel matching and prefix wildcards (e.g. `"output:*"`
//! matches any channel starting with `"output:"`).

use std::collections::HashMap;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use cue_core::ipc::EventPayload;

use super::EventBusMsg;

/// Spawn the EventBus actor task.
pub fn spawn(mut rx: mpsc::Receiver<EventBusMsg>) {
    tokio::spawn(async move {
        // Exact channel subscriptions: channel_name → (client_id → sender)
        let mut subs: HashMap<String, HashMap<u64, mpsc::Sender<EventPayload>>> = HashMap::new();
        // Wildcard (prefix) subscriptions: prefix → (client_id → sender)
        // e.g. "output:" for subscribing to "output:*"
        let mut prefix_subs: HashMap<String, HashMap<u64, mpsc::Sender<EventPayload>>> =
            HashMap::new();

        debug!("event_bus: started");

        while let Some(msg) = rx.recv().await {
            match msg {
                EventBusMsg::Subscribe {
                    client_id,
                    channel,
                    sender,
                } => {
                    debug!(%client_id, %channel, "event_bus: subscribe");
                    if let Some(prefix) = channel.strip_suffix('*') {
                        prefix_subs
                            .entry(prefix.to_string())
                            .or_default()
                            .insert(client_id, sender);
                    } else {
                        subs.entry(channel).or_default().insert(client_id, sender);
                    }
                }

                EventBusMsg::Unsubscribe { client_id, channel } => {
                    debug!(%client_id, %channel, "event_bus: unsubscribe");
                    if let Some(prefix) = channel.strip_suffix('*') {
                        if let Some(clients) = prefix_subs.get_mut(prefix) {
                            clients.remove(&client_id);
                            if clients.is_empty() {
                                prefix_subs.remove(prefix);
                            }
                        }
                    } else if let Some(clients) = subs.get_mut(&channel) {
                        clients.remove(&client_id);
                        if clients.is_empty() {
                            subs.remove(&channel);
                        }
                    }
                }

                EventBusMsg::UnsubscribeAll { client_id } => {
                    debug!(%client_id, "event_bus: unsubscribe_all");
                    subs.retain(|_ch, clients| {
                        clients.remove(&client_id);
                        !clients.is_empty()
                    });
                    prefix_subs.retain(|_prefix, clients| {
                        clients.remove(&client_id);
                        !clients.is_empty()
                    });
                }

                EventBusMsg::Publish { payload, channel } => {
                    // Exact match.
                    if let Some(clients) = subs.get(&channel) {
                        for (&cid, sender) in clients {
                            if sender.try_send(payload.clone()).is_err() {
                                warn!(client_id = %cid, %channel, "event_bus: client channel full or closed");
                            }
                        }
                    }
                    // Prefix/wildcard match.
                    for (prefix, clients) in &prefix_subs {
                        if channel.starts_with(prefix.as_str()) {
                            for (&cid, sender) in clients {
                                if sender.try_send(payload.clone()).is_err() {
                                    warn!(client_id = %cid, %channel, "event_bus: client channel full or closed (wildcard)");
                                }
                            }
                        }
                    }
                }

                EventBusMsg::Shutdown => {
                    debug!("event_bus: shutting down");
                    break;
                }
            }
        }

        debug!("event_bus: stopped");
    });
}
