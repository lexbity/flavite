//! Bounded worker preparation and short guarded Session commits for structure edits.
use crate::session::{Session, SessionResult};
use flavite_buffer::{BufferId, EditOp, Revision};
pub use flavite_syntax::properties::ScalarValue;
use flavite_syntax::{Span, TextByte};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Debug, Clone)]
pub enum StructureEdit {
    Property { key: String, value: ScalarValue },
    Task { start: usize, checked: bool },
}
#[derive(Debug)]
pub(crate) struct StructureJob {
    pub request: u64,
    pub buffer: BufferId,
    pub revision: Revision,
    text: Arc<str>,
    edit: StructureEdit,
}
#[derive(Debug)]
pub(crate) struct PreparedStructure {
    pub request: u64,
    pub buffer: BufferId,
    pub revision: Revision,
    pub patch: Result<EditOp, String>,
}
#[derive(Debug)]
pub(crate) struct StructureWorker {
    jobs: crossbeam::channel::Sender<StructureJob>,
    pub results: crossbeam::channel::Receiver<PreparedStructure>,
    outstanding: AtomicUsize,
}
impl StructureWorker {
    pub fn new() -> Self {
        let (jobs, receiver) = crossbeam::channel::bounded::<StructureJob>(64);
        let (sender, results) = crossbeam::channel::bounded(64);
        for index in 0..2 {
            let jobs = receiver.clone();
            let results = sender.clone();
            std::thread::Builder::new()
                .name(format!("vault-structure-{index}"))
                .spawn(move || {
                    while let Ok(job) = jobs.recv() {
                        let patch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            match job.edit {
                                StructureEdit::Property { key, value } => {
                                    flavite_syntax::properties::patch(&job.text, &key, &value)
                                }
                                StructureEdit::Task { start, checked } => {
                                    flavite_syntax::read::task_patch(&job.text, start, checked)
                                }
                            }
                        }))
                        .unwrap_or_else(|_| Err("Structure worker failed; retry explicitly".into()))
                        .map(|(range, replacement)| {
                            EditOp::new(
                                Span::new(TextByte(range.start), TextByte(range.end)),
                                replacement,
                            )
                        });
                        if results
                            .send(PreparedStructure {
                                request: job.request,
                                buffer: job.buffer,
                                revision: job.revision,
                                patch,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .expect("structure worker creation");
        }
        Self {
            jobs,
            results,
            outstanding: AtomicUsize::new(0),
        }
    }
    fn submit(&self, job: StructureJob) -> bool {
        if self
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 64).then_some(count + 1)
            })
            .is_err()
        {
            return false;
        }
        if self.jobs.try_send(job).is_err() {
            self.outstanding.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }
    pub fn retired(&self) {
        self.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}
impl Session {
    pub(crate) fn edit_structure(
        &self,
        buffer_id: BufferId,
        revision: Revision,
        edit: StructureEdit,
    ) -> SessionResult {
        let snapshot = match self.shared_buffer_snapshot_inner(buffer_id, Some(revision)) {
            Ok(snapshot) => snapshot,
            Err(_) => {
                return SessionResult::ActionRequired {
                    code: "structure.stale",
                    data: Some("Source changed; refresh before editing".into()),
                };
            }
        };
        if !matches!(
            snapshot.state,
            flavite_buffer::state::BufferState::Clean | flavite_buffer::state::BufferState::Dirty
        ) {
            return SessionResult::ActionRequired {
                code: "structure.state",
                data: Some("Resolve or recover before editing".into()),
            };
        }
        if snapshot.text.len() > 16 * 1024 * 1024 {
            return SessionResult::ActionRequired {
                code: "structure.limit",
                data: Some("Note exceeds structure edit limit; edit source".into()),
            };
        }
        if self.structure_completions_full() {
            return SessionResult::Busy;
        }
        let request = self.next_structure_request();
        if self.structure_worker.submit(StructureJob {
            request,
            buffer: buffer_id,
            revision,
            text: snapshot.text,
            edit,
        }) {
            SessionResult::Pending {
                request_id: request,
            }
        } else {
            SessionResult::Busy
        }
    }
}
