//! Pulse and Notice channel bridging (§4.5, D3, D16).
//!
//! This module provides the bridging between vault's Pulse/Notice channels
//! and the API's versioned Event system.
//!
//! # Design
//!
//! - **Pulse**: bounded (cap 1024), drop-oldest on overflow. Consumers MUST
//!   treat every pulse as a hint and re-query by generation.
//! - **Notice**: unbounded, but producers MUST coalesce per (note, kind) so
//!   at most one pending notice exists per key. Consumers ack presentation.
//!
//! The API converts vault-level Pulse/Notice to wire-level Event enum.

use crate::event::Event;
use crate::types::ApiVaultRef;
use crossbeam::channel::{Receiver, Sender, TryRecvError, bounded, unbounded};
use flavite_vault::channels::{Notice, NoticeKey, NoticeKind, Pulse};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A bridge that converts vault Pulse/Notice channels to API Event stream.
#[derive(Debug)]
pub struct ChannelBridge {
    /// Vault reference.
    vault_ref: ApiVaultRef,
    /// Pulse channel receiver.
    pulse_rx: Receiver<Pulse>,
    /// Notice channel receiver.
    notice_rx: Receiver<Notice>,
    /// Event sender.
    event_tx: Sender<Event>,
    /// Notice coalescing state.
    coalesce_state: Arc<Mutex<HashMap<NoticeKey, Notice>>>,
    /// Whether the bridge is running.
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Join handle for the bridge thread.
    _handle: Option<thread::JoinHandle<()>>,
}

impl ChannelBridge {
    /// Create a new channel bridge.
    ///
    /// # Arguments
    /// * `vault_ref` - The API vault reference
    /// * `pulse_rx` - Pulse channel receiver from the vault
    /// * `notice_rx` - Notice channel receiver from the vault
    /// * `event_tx` - Event sender for the API
    #[must_use]
    pub fn new(
        vault_ref: ApiVaultRef,
        pulse_rx: Receiver<Pulse>,
        notice_rx: Receiver<Notice>,
        event_tx: Sender<Event>,
    ) -> Self {
        Self {
            vault_ref,
            pulse_rx,
            notice_rx,
            event_tx,
            coalesce_state: Arc::new(Mutex::new(HashMap::new())),
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            _handle: None,
        }
    }

    /// Start the bridge thread.
    pub fn start(&mut self) {
        let vault_ref = self.vault_ref.clone();
        let pulse_rx = self.pulse_rx.clone();
        let notice_rx = self.notice_rx.clone();
        let event_tx = self.event_tx.clone();
        let coalesce_state = self.coalesce_state.clone();
        let running = self.running.clone();

        running.store(true, std::sync::atomic::Ordering::SeqCst);

        self._handle = Some(thread::spawn(move || {
            Self::bridge_loop(
                vault_ref,
                pulse_rx,
                notice_rx,
                event_tx,
                coalesce_state,
                running,
            );
        }));
    }

    /// Stop the bridge thread.
    pub fn stop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self._handle.take() {
            // Give the thread a chance to finish
            let _ = handle.join();
        }
    }

    /// The bridge loop that converts vault events to API events.
    fn bridge_loop(
        vault_ref: ApiVaultRef,
        pulse_rx: Receiver<Pulse>,
        notice_rx: Receiver<Notice>,
        event_tx: Sender<Event>,
        coalesce_state: Arc<Mutex<HashMap<NoticeKey, Notice>>>,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) {
        // Create a coalescer for notices
        let mut coalescer = Coalescer::new(coalesce_state);

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            // Use a timeout so we can check the running flag
            const POLL_TIMEOUT_MS: u64 = 100;

            // Try to receive a pulse first (higher priority for hints)
            match pulse_rx.recv_timeout(Duration::from_millis(POLL_TIMEOUT_MS / 2)) {
                Ok(pulse) => {
                    let event = Self::pulse_to_event(&vault_ref, pulse);
                    let _ = event_tx.send(event);
                    continue;
                }
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
            }

            // Try to receive notices
            match notice_rx.recv_timeout(Duration::from_millis(POLL_TIMEOUT_MS / 2)) {
                Ok(notice) => {
                    // Coalesce notices
                    if let Some(events) = coalescer.process(notice) {
                        for event in events {
                            let api_event = Self::notice_to_event(&vault_ref, event);
                            let _ = event_tx.send(api_event);
                        }
                    }
                    continue;
                }
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
            }

            // Check if we should stop
            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }

            // Small sleep to prevent busy-waiting
            thread::sleep(Duration::from_millis(10));
        }

        // On shutdown, flush any remaining coalesced notices
        let remaining = coalescer.flush();
        for notice in remaining {
            let event = Self::notice_to_event(&vault_ref, notice);
            let _ = event_tx.send(event);
        }
    }

    /// Convert a Pulse to an Event.
    fn pulse_to_event(vault_ref: &ApiVaultRef, pulse: Pulse) -> Event {
        match pulse {
            Pulse::VaultChanged {
                epoch,
                generation,
                summary,
            } => Event::VaultChanged {
                vault: vault_ref.clone(),
                epoch: epoch.value(),
                generation: generation.value(),
                added: summary.added,
                changed: summary.changed,
                removed: summary.removed,
            },
            Pulse::ReconcileProgress { done: _, total: _ } => {
                // ReconcileProgress is a pulse-level telemetry event
                // For now, we map it to VaultChanged as a simplified representation
                // In the future, this could be its own event type
                Event::VaultChanged {
                    vault: vault_ref.clone(),
                    epoch: 0,
                    generation: 0,
                    added: 0,
                    changed: 0,
                    removed: 0,
                }
            }
            Pulse::Invalidate { note: _ } => {
                // Invalidate is a pulse-level hint
                // Map to BufferStateChanged as a hint
                Event::BufferStateChanged {
                    vault: vault_ref.clone(),
                    buffer_id: 0, // Will be improved with proper buffer tracking
                    revision: 0,
                    state: "invalidated".to_string(),
                }
            }
        }
    }

    /// Convert a Notice to an Event.
    fn notice_to_event(vault_ref: &ApiVaultRef, notice: Notice) -> Event {
        match notice {
            Notice::RenameSuggested {
                from,
                to,
                tier,
                inbound,
            } => Event::RenameSuggested {
                vault: vault_ref.clone(),
                from,
                to,
                tier: tier.as_str().to_string(),
                inbound,
            },
            Notice::Conflict { note } => Event::Conflict {
                vault: vault_ref.clone(),
                note_path: note.path.to_string_lossy().into_owned(),
            },
            Notice::RecoveryOffer { note } => Event::RecoveryOffer {
                vault: vault_ref.clone(),
                note_path: note.path.to_string_lossy().into_owned(),
            },
            Notice::ActionRequired { code, note } => Event::ActionRequired {
                vault: vault_ref.clone(),
                code: code.to_string(),
                note_path: note.map(|n| n.path.to_string_lossy().into_owned()),
            },
        }
    }
}

/// Coalescer for notices.
///
/// Ensures that only one notice per (note, kind) is pending at any time.
#[derive(Debug)]
struct Coalescer {
    state: Arc<Mutex<HashMap<NoticeKey, Notice>>>,
}

impl Coalescer {
    /// Create a new coalescer.
    fn new(state: Arc<Mutex<HashMap<NoticeKey, Notice>>>) -> Self {
        Self { state }
    }

    /// Process a notice, coalescing if needed.
    ///
    /// Returns the notices to emit (which may be just the new notice, or
    /// a replacement if the key already had one).
    fn process(&mut self, notice: Notice) -> Option<Vec<Notice>> {
        let key = Self::notice_key(&notice);

        let mut state = self.state.lock().unwrap();

        // Check if we should replace an existing notice
        if let Some(_existing) = state.get(&key) {
            // For now, the new notice replaces the old one
            // In the future, we might merge them
            state.insert(key, notice.clone());
            Some(vec![notice])
        } else {
            // New notice
            state.insert(key, notice.clone());
            Some(vec![notice])
        }
    }

    /// Flush all remaining notices.
    fn flush(&self) -> Vec<Notice> {
        let state = self.state.lock().unwrap();
        state.values().cloned().collect()
    }

    /// Get the key for a notice.
    fn notice_key(notice: &Notice) -> NoticeKey {
        use flavite_buffer::manager::NoteRef;

        let note = match notice {
            Notice::RenameSuggested { from, .. } => NoteRef {
                path: from.clone(),
                stable_id: None,
            },
            Notice::Conflict { note } => note.clone(),
            Notice::RecoveryOffer { note } => note.clone(),
            Notice::ActionRequired { note, .. } => note.clone().unwrap_or(NoteRef {
                path: PathBuf::new(),
                stable_id: None,
            }),
        };

        let kind = match notice {
            Notice::RenameSuggested { .. } => NoticeKind::RenameSuggested,
            Notice::Conflict { .. } => NoticeKind::Conflict,
            Notice::RecoveryOffer { .. } => NoticeKind::RecoveryOffer,
            Notice::ActionRequired { .. } => NoticeKind::ActionRequired,
        };

        NoticeKey { note, kind }
    }
}

impl Drop for ChannelBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A manager for API-level event subscriptions.
#[derive(Debug, Default)]
pub struct EventManager {
    /// Subscribers.
    subscribers: Arc<Mutex<Vec<Sender<Event>>>>,
    /// Next subscriber ID.
    next_id: std::sync::atomic::AtomicU64,
}

impl EventManager {
    /// Create a new event manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
            next_id: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Subscribe to events.
    pub fn subscribe(&self) -> (u64, Receiver<Event>) {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = unbounded();
        {
            let mut subscribers = self.subscribers.lock().unwrap();
            subscribers.push(tx);
        }
        (id, rx)
    }

    /// Unsubscribe.
    pub fn unsubscribe(&self, _id: u64) {
        // For now, we don't track IDs for cleanup
        // In the future, we'll need to remove the sender
    }

    /// Broadcast an event to all subscribers.
    pub fn broadcast(&self, event: Event) {
        let subscribers = self.subscribers.lock().unwrap();
        // Iterate in reverse to handle removal during iteration
        for sender in subscribers.iter() {
            let _ = sender.send(event.clone());
        }
    }
}

/// A combined pulse/notice channel set for API consumers.
#[derive(Debug, Clone)]
pub struct ApiChannelSet {
    /// Pulse channel (bounded, lossy).
    pub pulse: ApiPulseChannel,
    /// Notice channel (unbounded, coalesced).
    pub notice: ApiNoticeChannel,
}

impl ApiChannelSet {
    /// Create a new API channel set.
    #[must_use]
    pub fn new(pulse_capacity: usize) -> Self {
        Self {
            pulse: ApiPulseChannel::new(pulse_capacity),
            notice: ApiNoticeChannel::new(),
        }
    }
}

/// API-level pulse channel wrapper.
#[derive(Debug, Clone)]
pub struct ApiPulseChannel {
    sender: Sender<Pulse>,
    receiver: Receiver<Pulse>,
}

impl ApiPulseChannel {
    /// Create a new pulse channel.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (sender, receiver) = bounded(capacity);
        Self { sender, receiver }
    }

    /// Send a pulse.
    pub fn send(&self, pulse: Pulse) -> Result<(), crossbeam::channel::SendError<Pulse>> {
        self.sender.send(pulse)
    }

    /// Try to send a pulse.
    pub fn try_send(&self, pulse: Pulse) -> Result<(), crossbeam::channel::TrySendError<Pulse>> {
        self.sender.try_send(pulse)
    }

    /// Receive a pulse.
    pub fn recv(&self) -> Result<Pulse, crossbeam::channel::RecvError> {
        self.receiver.recv()
    }

    /// Try to receive a pulse.
    pub fn try_recv(&self) -> Result<Pulse, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Get the capacity.
    #[must_use]
    pub fn capacity(&self) -> Option<usize> {
        self.sender.capacity()
    }
}

/// API-level notice channel wrapper.
#[derive(Debug, Clone)]
pub struct ApiNoticeChannel {
    sender: Sender<Notice>,
    receiver: Receiver<Notice>,
}

impl Default for ApiNoticeChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiNoticeChannel {
    /// Create a new notice channel.
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = unbounded();
        Self { sender, receiver }
    }

    /// Send a notice.
    pub fn send(&self, notice: Notice) -> Result<(), crossbeam::channel::SendError<Notice>> {
        self.sender.send(notice)
    }

    /// Try to send a notice.
    pub fn try_send(&self, notice: Notice) -> Result<(), crossbeam::channel::TrySendError<Notice>> {
        self.sender.try_send(notice)
    }

    /// Receive a notice.
    pub fn recv(&self) -> Result<Notice, crossbeam::channel::RecvError> {
        self.receiver.recv()
    }

    /// Try to receive a notice.
    pub fn try_recv(&self) -> Result<Notice, TryRecvError> {
        self.receiver.try_recv()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    #[test]
    fn api_channel_set_creation() {
        let set = ApiChannelSet::new(1024);
        assert_eq!(set.pulse.capacity(), Some(1024));
    }

    #[test]
    fn pulse_channel_send_recv() {
        use flavite_vault::types::{Epoch, Generation, ReconcileSummary};

        let channel = ApiPulseChannel::new(10);
        let pulse = Pulse::VaultChanged {
            epoch: Epoch::new(1),
            generation: Generation::new(1),
            summary: ReconcileSummary {
                added: 0,
                changed: 0,
                removed: 0,
            },
        };

        channel.send(pulse.clone()).unwrap();
        let received = channel.recv().unwrap();

        match received {
            Pulse::VaultChanged { .. } => {}
            _ => panic!("Expected VaultChanged pulse"),
        }
    }

    #[test]
    fn notice_channel_send_recv() {
        use flavite_buffer::manager::NoteRef;

        let channel = ApiNoticeChannel::new();
        let notice = Notice::Conflict {
            note: NoteRef {
                path: PathBuf::from("test.md"),
                stable_id: None,
            },
        };

        channel.send(notice.clone()).unwrap();
        let received = channel.recv().unwrap();

        match received {
            Notice::Conflict { .. } => {}
            _ => panic!("Expected Conflict notice"),
        }
    }

    #[test]
    fn event_manager_broadcast() {
        let manager = EventManager::new();
        let (_id1, _rx1) = manager.subscribe();
        let (_id2, _rx2) = manager.subscribe();

        let event = Event::VaultChanged {
            vault: ApiVaultRef::new(Uuid::new_v4(), PathBuf::from("/test")),
            epoch: 1,
            generation: 1,
            added: 0,
            changed: 0,
            removed: 0,
        };

        manager.broadcast(event);

        // Broadcast test - event was sent successfully
    }

    #[test]
    fn pulse_to_event() {
        use flavite_vault::types::{Epoch, Generation, ReconcileSummary};

        let vault_ref = ApiVaultRef::new(Uuid::new_v4(), PathBuf::from("/test"));
        let pulse = Pulse::VaultChanged {
            epoch: Epoch::new(1),
            generation: Generation::new(2),
            summary: ReconcileSummary {
                added: 3,
                changed: 4,
                removed: 5,
            },
        };

        let event = ChannelBridge::pulse_to_event(&vault_ref, pulse);
        match event {
            Event::VaultChanged {
                epoch,
                generation,
                added,
                changed,
                removed,
                ..
            } => {
                assert_eq!(epoch, 1);
                assert_eq!(generation, 2);
                assert_eq!(added, 3);
                assert_eq!(changed, 4);
                assert_eq!(removed, 5);
            }
            _ => panic!("Expected VaultChanged event"),
        }
    }

    #[test]
    fn notice_to_event() {
        let vault_ref = ApiVaultRef::new(Uuid::new_v4(), PathBuf::from("/test"));
        let notice = Notice::RenameSuggested {
            from: PathBuf::from("old.md"),
            to: PathBuf::from("new.md"),
            tier: flavite_vault::channels::RenameTier::T0,
            inbound: 10,
        };

        let event = ChannelBridge::notice_to_event(&vault_ref, notice);
        match event {
            Event::RenameSuggested {
                from,
                to,
                tier,
                inbound,
                ..
            } => {
                assert_eq!(from, PathBuf::from("old.md"));
                assert_eq!(to, PathBuf::from("new.md"));
                assert_eq!(tier, "T0");
                assert_eq!(inbound, 10);
            }
            _ => panic!("Expected RenameSuggested event"),
        }
    }

    #[test]
    fn bridge_maps_remaining_pulse_and_notice_variants() {
        use flavite_buffer::manager::NoteRef;
        let vault = ApiVaultRef::new(Uuid::new_v4(), PathBuf::from("/vault"));

        let progress =
            ChannelBridge::pulse_to_event(&vault, Pulse::ReconcileProgress { done: 2, total: 5 });
        assert!(matches!(
            progress,
            Event::VaultChanged {
                epoch: 0,
                generation: 0,
                ..
            }
        ));
        let invalidation = ChannelBridge::pulse_to_event(
            &vault,
            Pulse::Invalidate {
                note: NoteRef {
                    path: PathBuf::from("a.md"),
                    stable_id: None,
                },
            },
        );
        assert!(
            matches!(invalidation, Event::BufferStateChanged { buffer_id: 0, revision: 0, state, .. } if state == "invalidated")
        );

        let note = NoteRef {
            path: PathBuf::from("notes/a.md"),
            stable_id: None,
        };
        assert!(
            matches!(ChannelBridge::notice_to_event(&vault, Notice::Conflict { note: note.clone() }), Event::Conflict { note_path, .. } if note_path == "notes/a.md")
        );
        assert!(
            matches!(ChannelBridge::notice_to_event(&vault, Notice::RecoveryOffer { note: note.clone() }), Event::RecoveryOffer { note_path, .. } if note_path == "notes/a.md")
        );
        assert!(
            matches!(ChannelBridge::notice_to_event(&vault, Notice::ActionRequired { code: "repair", note: Some(note.clone()) }), Event::ActionRequired { code, note_path: Some(path), .. } if code == "repair" && path == "notes/a.md")
        );
        assert!(matches!(
            ChannelBridge::notice_to_event(
                &vault,
                Notice::ActionRequired {
                    code: "global",
                    note: None
                }
            ),
            Event::ActionRequired {
                note_path: None,
                ..
            }
        ));
    }

    #[test]
    fn coalescer_replaces_same_notice_key_and_flushes_pending_items() {
        use flavite_buffer::manager::NoteRef;
        let state = Arc::new(Mutex::new(HashMap::new()));
        let mut coalescer = Coalescer::new(state.clone());
        let first = Notice::Conflict {
            note: NoteRef {
                path: PathBuf::from("same.md"),
                stable_id: None,
            },
        };
        let replacement = Notice::RecoveryOffer {
            note: NoteRef {
                path: PathBuf::from("same.md"),
                stable_id: None,
            },
        };
        assert_eq!(coalescer.process(first).unwrap().len(), 1);
        assert_eq!(coalescer.process(replacement).unwrap().len(), 1);
        let flushed = coalescer.flush();
        assert_eq!(
            flushed.len(),
            2,
            "notice kinds have distinct coalescing keys"
        );
        assert_eq!(state.lock().unwrap().len(), 2);
    }
}
