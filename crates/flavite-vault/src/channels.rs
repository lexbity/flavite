//! Pulse and Notice channels (D3, §4.2, §4.5).
//!
//! The vault uses two channels with opposite loss policies:
//!
//! - **Pulse**: bounded (cap 1024), drop-oldest on overflow. Consumers MUST
//!   treat every pulse as a hint and re-query by generation.
//! - **Notice**: unbounded, but producers MUST coalesce per (note, kind) so
//!   at most one pending notice exists per key. Consumers ack presentation.
//!
//! This separation ensures conflict banners and recovery offers are never dropped
//! (R5) even when telemetry events flood the channel.

use crate::types::{Epoch, Generation, ReconcileSummary};
use crossbeam::channel::{Receiver, Sender, bounded, unbounded};
use flavite_buffer::manager::NoteRef;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A tier for rename inference (D8).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum RenameTier {
    /// In-app rename command.
    T0,
    /// OS rename event pair (inotify cookie / FSEvents flag / RENAMED_OLD+NEW).
    T1,
    /// Same stable_id on a vanished + created pair.
    T2,
    /// Same content hash, size ≥ 64 B, exactly one candidate on each side.
    T3,
    /// Fuzzy similarity ≥ 0.9, exactly one candidate.
    #[default]
    T4,
}

impl RenameTier {
    /// Get the tier as a static string.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::T0 => "T0",
            Self::T1 => "T1",
            Self::T2 => "T2",
            Self::T3 => "T3",
            Self::T4 => "T4",
        }
    }
}

/// A pulse event — lossy by design (D3).
///
/// Consumers MUST treat every pulse as a hint and re-query by generation.
#[derive(Debug, Clone, PartialEq)]
pub enum Pulse {
    /// Vault contents changed.
    VaultChanged {
        epoch: Epoch,
        generation: Generation,
        summary: ReconcileSummary,
    },
    /// Progress update for reconcile operations.
    ReconcileProgress { done: u32, total: u32 },
    /// A specific note was invalidated.
    Invalidate { note: NoteRef },
}

/// A notice — lossless, coalesced per (note, kind) (D3).
///
/// Producers MUST coalesce per (note, kind) so at most one pending notice
/// exists per key. Consumers ack presentation (display in GUI, reporting in
/// CLI). This ack never completes a command or journal item.
#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    /// A rename was suggested.
    RenameSuggested {
        from: PathBuf,
        to: PathBuf,
        tier: RenameTier,
        inbound: u32,
    },
    /// A conflict was detected.
    Conflict { note: NoteRef },
    /// A recovery offer is available.
    RecoveryOffer { note: NoteRef },
    /// An action is required from the user.
    ActionRequired {
        code: &'static str,
        note: Option<NoteRef>,
    },
}

/// Key for coalescing notices.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NoticeKey {
    pub note: NoteRef,
    pub kind: NoticeKind,
    /// Action-required notices with different recovery codes are distinct.
    pub action_code: Option<&'static str>,
}

/// The kind of notice (for coalescing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    RenameSuggested,
    Conflict,
    RecoveryOffer,
    ActionRequired,
}

/// The pulse channel — bounded, drop-oldest on overflow.
#[derive(Debug)]
pub struct PulseChannel {
    sender: Sender<Pulse>,
    receiver: Receiver<Pulse>,
    capacity: usize,
}

impl Clone for PulseChannel {
    fn clone(&self) -> Self {
        // For stub implementation, create a new channel with the same config
        // In production, we'd share the existing channel
        PulseChannel::new(self.capacity)
    }
}

impl PulseChannel {
    /// Create a new pulse channel with the specified capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (sender, receiver) = bounded(capacity);
        Self {
            sender,
            receiver,
            capacity,
        }
    }

    /// Send a pulse event. Drops oldest if full.
    pub fn send(&self, pulse: Pulse) -> Result<(), crossbeam::channel::SendError<Pulse>> {
        match self.sender.try_send(pulse) {
            Ok(()) => Ok(()),
            Err(crossbeam::channel::TrySendError::Disconnected(pulse)) => {
                Err(crossbeam::channel::SendError(pulse))
            }
            Err(crossbeam::channel::TrySendError::Full(pulse)) => {
                let _ = self.receiver.try_recv();
                self.sender.try_send(pulse).map_err(|error| match error {
                    crossbeam::channel::TrySendError::Full(pulse)
                    | crossbeam::channel::TrySendError::Disconnected(pulse) => {
                        crossbeam::channel::SendError(pulse)
                    }
                })
            }
        }
    }

    /// Try to send a pulse event. Returns the pulse if full.
    pub fn try_send(&self, pulse: Pulse) -> Result<(), crossbeam::channel::TrySendError<Pulse>> {
        self.sender.try_send(pulse)
    }

    /// Receive a pulse event.
    pub fn recv(&self) -> Result<Pulse, crossbeam::channel::RecvError> {
        self.receiver.recv()
    }

    /// Try to receive a pulse event.
    pub fn try_recv(&self) -> Result<Pulse, crossbeam::channel::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Get the capacity of the channel.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get a receiver for this channel (for consumers).
    #[must_use]
    pub fn receiver(&self) -> Receiver<Pulse> {
        self.receiver.clone()
    }

    /// Get a sender for this channel (for producers).
    #[must_use]
    pub fn sender(&self) -> Sender<Pulse> {
        self.sender.clone()
    }
}

/// Coalescing notice channel — unbounded but coalesced per key.
#[derive(Debug)]
pub struct NoticeChannel {
    sender: Sender<Notice>,
    receiver: Receiver<Notice>,
    /// Pending notices being coalesced.
    pending: Arc<Mutex<HashMap<NoticeKey, Notice>>>,
    /// Timeout for coalescing.
    coalesce_timeout: Duration,
}

impl Clone for NoticeChannel {
    fn clone(&self) -> Self {
        // For stub implementation, create a new channel with the same config
        // In production, we'd share the existing channel
        NoticeChannel::new(self.coalesce_timeout)
    }
}

impl NoticeChannel {
    /// Create a new notice channel.
    #[must_use]
    pub fn new(coalesce_timeout: Duration) -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver,
            pending: Arc::new(Mutex::new(HashMap::new())),
            coalesce_timeout,
        }
    }

    /// Send a notice, coalescing by key.
    ///
    /// If a notice with the same key is already pending, it is replaced.
    /// Otherwise, the notice is added to the pending map and sent immediately.
    pub fn send(&self, notice: Notice) {
        let key = Self::key_for_notice(&notice);

        let mut pending = self.pending.lock().unwrap();

        // Replace existing pending notice with the same key
        pending.insert(key.clone(), notice.clone());

        // If this is the first notice for this key, we need to send it
        // We'll send it immediately and rely on the consumer to coalesce
        // at the receiving end, or we can use a timer to batch sends.
        // For now, we send immediately to keep it simple.
        let _ = self.sender.try_send(notice);
    }

    /// Try to send a notice without coalescing.
    pub fn try_send(&self, notice: Notice) -> Result<(), crossbeam::channel::TrySendError<Notice>> {
        self.sender.try_send(notice)
    }

    /// Receive a notice.
    pub fn recv(&self) -> Result<Notice, crossbeam::channel::RecvError> {
        self.receiver.recv()
    }

    /// Try to receive a notice.
    pub fn try_recv(&self) -> Result<Notice, crossbeam::channel::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Get a receiver for this channel.
    #[must_use]
    pub fn receiver(&self) -> Receiver<Notice> {
        self.receiver.clone()
    }

    /// Get a sender for this channel.
    #[must_use]
    pub fn sender(&self) -> Sender<Notice> {
        self.sender.clone()
    }

    /// Acknowledge presentation of a notice.
    ///
    /// This removes the notice from the pending map. The ack never completes
    /// a command or journal item.
    pub fn ack(&self, notice: &Notice) {
        let key = Self::key_for_notice(notice);
        let mut pending = self.pending.lock().unwrap();
        pending.remove(&key);
    }

    /// Generate a key for a notice (for coalescing).
    fn key_for_notice(notice: &Notice) -> NoticeKey {
        match notice {
            Notice::RenameSuggested { from, .. } => {
                // For rename, we use the "from" path as the key
                NoticeKey {
                    note: NoteRef::new(from.clone()),
                    kind: NoticeKind::RenameSuggested,
                    action_code: None,
                }
            }
            Notice::Conflict { note } => NoticeKey {
                note: note.clone(),
                kind: NoticeKind::Conflict,
                action_code: None,
            },
            Notice::RecoveryOffer { note } => NoticeKey {
                note: note.clone(),
                kind: NoticeKind::RecoveryOffer,
                action_code: None,
            },
            Notice::ActionRequired { code, note } => NoticeKey {
                note: note
                    .clone()
                    .unwrap_or_else(|| NoteRef::new(PathBuf::from("unknown"))),
                kind: NoticeKind::ActionRequired,
                action_code: Some(*code),
            },
        }
    }
}

/// Channel set containing both pulse and notice channels.
#[derive(Debug)]
pub struct ChannelSet {
    pub pulse: PulseChannel,
    pub notice: NoticeChannel,
}

impl Default for ChannelSet {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelSet {
    /// Create a new channel set with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(
            1024,                       // pulse capacity
            Duration::from_millis(300), // coalesce timeout
        )
    }

    /// Create a new channel set with custom configuration.
    #[must_use]
    pub fn with_config(pulse_capacity: usize, coalesce_timeout: Duration) -> Self {
        Self {
            pulse: PulseChannel::new(pulse_capacity),
            notice: NoticeChannel::new(coalesce_timeout),
        }
    }

    /// Send a pulse.
    pub fn send_pulse(&self, pulse: Pulse) {
        let _ = self.pulse.send(pulse);
    }

    /// Send a notice.
    pub fn send_notice(&self, notice: Notice) {
        self.notice.send(notice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_capacity_clone_and_overflow_keep_the_latest_generation() {
        let channel = PulseChannel::new(2);
        assert_eq!(channel.capacity(), 2);
        let cloned = channel.clone();
        assert_eq!(cloned.capacity(), 2);

        for generation in 1..=3 {
            channel
                .send(Pulse::VaultChanged {
                    epoch: Epoch::new(4),
                    generation: Generation::new(generation),
                    summary: ReconcileSummary::default(),
                })
                .unwrap();
        }
        assert!(
            matches!(channel.recv().unwrap(), Pulse::VaultChanged { generation, .. } if generation == Generation::new(2))
        );
        assert!(
            matches!(channel.recv().unwrap(), Pulse::VaultChanged { generation, .. } if generation == Generation::new(3))
        );
        assert!(matches!(PulseChannel::new(0).capacity(), 1));
    }

    #[test]
    fn notice_keys_and_ack_cover_each_notice_kind() {
        let channel = NoticeChannel::new(Duration::ZERO);
        let note = NoteRef::with_stable_id(PathBuf::from("note.md"), "stable".into());
        let notices = [
            Notice::RenameSuggested {
                from: PathBuf::from("old.md"),
                to: PathBuf::from("new.md"),
                tier: RenameTier::T1,
                inbound: 2,
            },
            Notice::Conflict { note: note.clone() },
            Notice::RecoveryOffer { note: note.clone() },
            Notice::ActionRequired {
                code: "confirm",
                note: Some(note.clone()),
            },
            Notice::ActionRequired {
                code: "global",
                note: None,
            },
        ];
        for notice in &notices {
            let key = NoticeChannel::key_for_notice(notice);
            channel.send(notice.clone());
            assert!(channel.pending.lock().unwrap().contains_key(&key));
            channel.ack(notice);
            assert!(!channel.pending.lock().unwrap().contains_key(&key));
        }
        assert_eq!(
            NoticeChannel::key_for_notice(&notices[0]).note.path,
            PathBuf::from("old.md")
        );
        assert_eq!(
            NoticeChannel::key_for_notice(&notices[4]).note.path,
            PathBuf::from("unknown")
        );
        let (sender, receiver) = (channel.sender(), channel.receiver());
        assert_eq!(receiver.try_iter().count(), notices.len());
        sender.send(Notice::Conflict { note }).unwrap();
        assert!(matches!(
            receiver.try_recv().unwrap(),
            Notice::Conflict { .. }
        ));
    }

    #[test]
    fn rename_tier_display() {
        assert_eq!(RenameTier::T0.as_str(), "T0");
        assert_eq!(RenameTier::T1.as_str(), "T1");
        assert_eq!(RenameTier::T2.as_str(), "T2");
        assert_eq!(RenameTier::T3.as_str(), "T3");
        assert_eq!(RenameTier::T4.as_str(), "T4");
    }
}
