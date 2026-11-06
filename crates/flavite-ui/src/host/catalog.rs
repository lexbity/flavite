//! Bounded catalog acquisition and presentation preparation; App only routes.
use crate::app::{App, CATALOG_REFRESH_THROTTLE, CatalogStatus};
use flavite_api::{CompletionCandidate, NoteCatalogEntry, Provenance};
use std::sync::{Arc, mpsc};
use std::time::Instant;

#[derive(Debug)]
struct Prepared {
    sequence: u64,
    entries: Arc<[NoteCatalogEntry]>,
    candidates: Arc<[CompletionCandidate]>,
    picker: crate::picker::NoteCandidates,
    provenance: Provenance,
    truncated: bool,
}
type Work = (
    u64,
    Arc<[NoteCatalogEntry]>,
    Arc<[CompletionCandidate]>,
    Provenance,
    bool,
);

#[derive(Debug)]
pub(crate) struct CatalogHost {
    pub entries: Arc<[NoteCatalogEntry]>,
    pub catalog_status: CatalogStatus,
    pub catalog_last_submit: Option<Instant>,
    pub catalog_retry_due: bool,
    pub editor_candidates: Arc<[CompletionCandidate]>,
    pub note_candidates: crate::picker::NoteCandidates,
    sequence: u64,
    work: mpsc::SyncSender<Work>,
    ready: mpsc::Receiver<Prepared>,
}
impl Default for CatalogHost {
    fn default() -> Self {
        let (work, jobs) = mpsc::sync_channel::<Work>(1);
        let (results, ready) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("catalog-presentation".into())
            .spawn(move || {
                while let Ok((sequence, entries, candidates, provenance, truncated)) = jobs.recv() {
                    let picker = crate::picker::NoteCandidates::from_catalog(&entries);
                    if results
                        .send(Prepared {
                            sequence,
                            entries,
                            candidates,
                            picker,
                            provenance,
                            truncated,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("catalog worker");
        Self {
            entries: Arc::from([]),
            catalog_status: CatalogStatus::Empty,
            catalog_last_submit: None,
            catalog_retry_due: false,
            editor_candidates: Arc::from([]),
            note_candidates: crate::picker::NoteCandidates::default(),
            sequence: 0,
            work,
            ready,
        }
    }
}
impl CatalogHost {
    fn submit(
        &mut self,
        entries: Arc<[NoteCatalogEntry]>,
        candidates: Arc<[CompletionCandidate]>,
        provenance: Provenance,
        truncated: bool,
    ) -> Result<(), ()> {
        self.sequence = self.sequence.checked_add(1).ok_or(())?;
        self.work
            .try_send((self.sequence, entries, candidates, provenance, truncated))
            .map_err(|_| ())
    }
}
pub(crate) fn poll(app: &mut App) {
    while let Ok(prepared) = app.catalog_host.ready.try_recv() {
        if prepared.sequence != app.catalog_host.sequence {
            continue;
        }
        app.catalog_host.entries = prepared.entries;
        app.catalog_host.editor_candidates = prepared.candidates;
        app.catalog_host.note_candidates = prepared.picker;
        app.catalog_host.catalog_status = CatalogStatus::Ready {
            epoch: prepared.provenance.epoch,
            generation: prepared.provenance.generation,
            truncated: prepared.truncated,
        };
        app.link_verdict_requests.clear();
        app.link_verdicts.clear();
        app.run_pending_restore();
    }
}
impl App {
    /// Request a catalog refresh (replaces the pre-S5 blocking load): the
    /// whole read goes out as a QueryService ticket, so this turn pays only
    /// an admission (NFR-4). Results apply in the pump drain path.
    /// Explicit triggers (startup, vault-open completion, create/rename
    /// completions, retry) submit even while a ticket is in flight — the
    /// newest ticket supersedes, so responding to an event can never be
    /// swallowed by an older read that started before the event landed.
    pub(crate) fn request_catalog_refresh(&mut self) {
        self.try_submit_catalog_ticket();
    }

    /// A `VaultChanged` pulse refreshes at most once per throttle window
    /// (§6.4: ≥ 1 s); later pulses coalesce into one retry.
    pub(crate) fn note_vault_changed_pulse(&mut self) {
        if self
            .catalog_host
            .catalog_last_submit
            .is_none_or(|when| when.elapsed() >= CATALOG_REFRESH_THROTTLE)
        {
            self.try_submit_catalog_ticket();
        } else {
            self.catalog_host.catalog_retry_due = true;
        }
    }

    fn try_submit_catalog_ticket(&mut self) {
        let Some(flavite) = self.flavite.as_ref() else {
            return;
        };
        match flavite.submit_catalog() {
            Ok(_token) => {
                self.catalog_host.sequence = self
                    .catalog_host
                    .sequence
                    .checked_add(1)
                    .expect("catalog sequence exhausted");
                self.catalog_host.catalog_status = CatalogStatus::Loading;
                self.catalog_host.catalog_last_submit = Some(Instant::now());
                self.catalog_host.catalog_retry_due = false;
            }
            Err(flavite_api::QuerySubmitError::Busy) => {
                // No silent loss: keep the last status and snapshot, retry
                // from the pump once the throttle window has passed.
                self.catalog_host.catalog_retry_due = true;
            }
        }
    }

    /// Apply one drained catalog outcome. `Loading` and `Failed` never
    /// clear the existing snapshot (picker keeps serving candidates, no
    /// flicker); `Failed` surfaces visible retryable text.
    pub(crate) fn apply_catalog_outcome(&mut self, result: &flavite_api::ContextQueryResult) {
        match result {
            flavite_api::ContextQueryResult::Catalog {
                entries,
                candidates,
                provenance,
                truncated,
                ..
            } => {
                if self
                    .catalog_host
                    .submit(entries.clone(), candidates.clone(), *provenance, *truncated)
                    .is_err()
                {
                    self.catalog_host.catalog_retry_due = true;
                    self.catalog_host.catalog_status = CatalogStatus::Failed {
                        message: "Catalog presentation busy; retrying".into(),
                    };
                }
                return;
            }
            flavite_api::ContextQueryResult::Updating => {
                self.catalog_host.catalog_retry_due = true;
                self.catalog_host.catalog_status = CatalogStatus::Failed {
                    message: "Catalog updating; retrying".into(),
                };
            }
            flavite_api::ContextQueryResult::Failed { message } => {
                self.catalog_host.catalog_status = CatalogStatus::Failed {
                    message: message.clone(),
                };
            }
            other => {
                self.catalog_host.catalog_status = CatalogStatus::Failed {
                    message: format!("unexpected catalog outcome: {other:?}"),
                };
            }
        }
        // The first terminal outcome (page served or honest failure)
        // unblocks the deferred workspace restore.
        self.run_pending_restore();
    }

    /// Consume a coalesced retry once the previous ticket has settled and
    /// the throttle window has passed.
    pub(crate) fn catalog_retry_tick(&mut self) {
        if !self.catalog_host.catalog_retry_due
            || matches!(self.catalog_host.catalog_status, CatalogStatus::Loading)
            || !self
                .catalog_host
                .catalog_last_submit
                .is_some_and(|when| when.elapsed() >= CATALOG_REFRESH_THROTTLE)
        {
            return;
        }
        self.catalog_host.catalog_retry_due = false;
        self.try_submit_catalog_ticket();
    }

    /// The unified note-candidate snapshot (W10 matcher service).
    #[must_use]
    pub fn note_candidates(&self) -> &crate::picker::NoteCandidates {
        &self.catalog_host.note_candidates
    }

    /// Rich immutable alias/ID candidates shared with editor widgets.
    #[must_use]
    pub fn editor_candidates(&self) -> &std::sync::Arc<[flavite_api::CompletionCandidate]> {
        &self.catalog_host.editor_candidates
    }
}
