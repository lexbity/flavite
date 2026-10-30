//! Host-side context correlation (RFC-0003 §20, W04).
//!
//! Each context tool tracks its binding, its effective source (recomputed
//! through the pure [`ContextRouter`]), and a query state machine
//! correlated by (TabId, incarnation, binding sequence). A late or
//! superseded query result can never repaint a replacement view, and Busy,
//! no-backlinks, missing-note, and failure are distinct honest states.

use flavite_api::{BacklinkRow, ContextQueryResult as QueryResult, Provenance};
use flavite_workspace::{ContextSource, NoteLocator, TabId, UnboundReason};
use std::collections::BTreeMap;

/// One pending or served query identity, mirrored from the service token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PendingQuery {
    /// Binding sequence the query was issued for.
    pub binding_sequence: u64,
    /// Service request sequence.
    pub request_sequence: u64,
}

/// Content of a served query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolContent {
    /// Saved source text (never dirty buffer content).
    Preview {
        /// Saved text.
        text: String,
        /// Index provenance.
        provenance: Provenance,
    },
    /// Inbound links with exact source lines where available.
    Backlinks {
        /// Linking notes.
        rows: Vec<BacklinkRow>,
        /// Index provenance.
        provenance: Provenance,
    },
}

/// Observable state of one context tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    /// A source exists; the first query has not been submitted yet.
    Idle,
    /// A query is in flight.
    Querying,
    /// The query service rejected admission (all slots busy).
    Busy,
    /// Content arrived for the current source.
    Ready(ToolContent),
    /// The saved vault has no such note.
    Missing,
    /// Backlinks came back genuinely empty.
    NoBacklinks,
    /// The query failed; presentation shows a retry.
    Failed {
        /// Redacted failure text.
        message: String,
    },
    /// The binding has no effective source right now.
    Unbound {
        /// Why the binding is unbound.
        reason: UnboundReason,
    },
}

/// Host state behind one Preview/Backlinks tab.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextTool {
    /// Process-local incarnation of the tool view.
    incarnation: u64,
    /// Whether this tool queries preview or backlink payloads.
    backlinks: bool,
    /// Monotonic counter stamped onto submitted tokens; every new effective
    /// source increments it, so superseded results are dropped.
    binding_sequence: u64,
    /// Source identity the current state describes.
    observed: Option<SourceIdentity>,
    /// Outstanding query, if any.
    pending: Option<PendingQuery>,
    /// Observable state.
    state: ToolState,
}

/// The source identity a tool currently observes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceIdentity {
    /// The note locator of the source, when it resolves to a note.
    pub locator: NoteLocator,
    /// The publication/sequence marker of the source.
    pub marker: u64,
}

impl SourceIdentity {
    fn from_resolved(source: &ContextSource) -> Option<Self> {
        match source {
            ContextSource::Document {
                locator,
                publication,
                ..
            }
            | ContextSource::Exploration {
                locator,
                sequence: publication,
                ..
            } => Some(Self {
                locator: locator.clone(),
                marker: *publication,
            }),
            ContextSource::Pinned { locator } => Some(Self {
                locator: locator.clone(),
                marker: u64::MAX,
            }),
            ContextSource::Unbound { .. } => None,
        }
    }
}

/// Per-tool context registry owned by the host.
#[derive(Debug, Default)]
pub struct ContextRegistry {
    tools: BTreeMap<TabId, ContextTool>,
}

/// A query the host must submit to the API query service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryJob {
    /// The tool tab.
    pub tab: TabId,
    /// The tool's view incarnation.
    pub incarnation: u64,
    /// The binding sequence to stamp on the token.
    pub binding_sequence: u64,
    /// Vault-relative path components to query.
    pub path: Vec<String>,
    /// Whether the payload is backlinks (else preview).
    pub backlinks: bool,
}

impl ContextRegistry {
    /// Attach a newly opened context tool view.
    pub fn attach(&mut self, tab: TabId, incarnation: u64, backlinks: bool) {
        self.tools.insert(
            tab,
            ContextTool {
                incarnation,
                backlinks,
                binding_sequence: 0,
                observed: None,
                pending: None,
                state: ToolState::Idle,
            },
        );
    }

    /// Release one tool view's state.
    pub fn detach(&mut self, tab: TabId) {
        self.tools.remove(&tab);
    }

    /// Re-resolve one tool against its router result. Returns a query job
    /// when the effective source changed and a query must be submitted.
    /// Closing or rebinding before completion cannot repaint a replacement
    /// view: tokens carry tab, incarnation, and binding sequence.
    pub fn rebind(&mut self, tab: TabId, source: &ContextSource) -> Option<QueryJob> {
        let tool = self.tools.get_mut(&tab)?;
        let identity = SourceIdentity::from_resolved(source);
        if tool.observed.as_ref() == identity.as_ref()
            && !matches!(source, ContextSource::Unbound { .. })
        {
            return None;
        }
        tool.binding_sequence = tool.binding_sequence.saturating_add(1);
        tool.pending = None;
        let state = match source {
            ContextSource::Unbound { reason } => ToolState::Unbound {
                reason: reason.clone(),
            },
            _ => ToolState::Idle,
        };
        tool.state = state;
        tool.observed = identity.clone();
        identity.map(|identity| QueryJob {
            tab,
            incarnation: tool.incarnation,
            binding_sequence: tool.binding_sequence,
            path: identity.locator.components().to_vec(),
            backlinks: tool.backlinks,
        })
    }

    /// Record admission failure for the tool's in-flight attempt.
    pub fn busy(&mut self, tab: TabId, binding_sequence: u64) {
        if let Some(tool) = self.tools.get_mut(&tab)
            && tool.binding_sequence == binding_sequence
            && tool.state == ToolState::Idle
        {
            tool.state = ToolState::Busy;
            tool.pending = None;
        }
    }

    /// Record that the host submitted the job for this binding sequence;
    /// `request_sequence` comes from the accepted service token.
    pub fn submitted(&mut self, tab: TabId, binding_sequence: u64, request_sequence: u64) {
        if let Some(tool) = self.tools.get_mut(&tab)
            && tool.binding_sequence == binding_sequence
        {
            tool.pending = Some(PendingQuery {
                binding_sequence,
                request_sequence,
            });
            if tool.state == ToolState::Idle {
                tool.state = ToolState::Querying;
            }
        }
    }

    /// Observe a served query result. Returns true when the tool's visible
    /// state changed. Results from older sequences or replaced views are
    /// discarded without painting.
    #[must_use]
    pub fn observe(
        &mut self,
        tab: TabId,
        incarnation: u64,
        binding_sequence: u64,
        request_sequence: u64,
        result: &QueryResult,
    ) -> bool {
        let Some(tool) = self.tools.get_mut(&tab) else {
            return false;
        };
        if tool.incarnation != incarnation
            || tool.binding_sequence != binding_sequence
            || tool.pending.map(|pending| pending.request_sequence) != Some(request_sequence)
        {
            return false;
        }
        tool.pending = None;
        tool.state = match result {
            QueryResult::Preview {
                text, provenance, ..
            } => ToolState::Ready(ToolContent::Preview {
                text: text.clone(),
                provenance: *provenance,
            }),
            QueryResult::Backlinks {
                links, provenance, ..
            } => {
                if links.is_empty() {
                    ToolState::NoBacklinks
                } else {
                    ToolState::Ready(ToolContent::Backlinks {
                        rows: links.clone(),
                        provenance: *provenance,
                    })
                }
            }
            QueryResult::Missing => ToolState::Missing,
            QueryResult::Failed { message } => ToolState::Failed {
                message: message.clone(),
            },
        };
        true
    }

    /// Observable state for rendering and harness snapshots.
    #[must_use]
    pub fn state(&self, tab: &TabId) -> Option<&ToolState> {
        self.tools.get(tab).map(|tool| &tool.state)
    }

    /// The source identity a tool currently observes.
    #[must_use]
    pub fn observed(&self, tab: &TabId) -> Option<&SourceIdentity> {
        self.tools.get(tab).and_then(|tool| tool.observed.as_ref())
    }

    /// Whether any tool awaits a query result.
    #[must_use]
    pub fn any_pending(&self) -> bool {
        self.tools.values().any(|tool| tool.pending.is_some())
    }

    /// Whether any context tool is attached.
    #[must_use]
    pub fn no_tools(&self) -> bool {
        self.tools.is_empty()
    }

    /// Current incarnation of one tool.
    #[must_use]
    pub fn incarnation_of(&self, tab: &TabId) -> Option<u64> {
        self.tools.get(tab).map(|tool| tool.incarnation)
    }

    /// Force every tool to re-query on the next reconcile (saved vault
    /// content changed; the publication marker alone cannot see that).
    pub fn invalidate_all(&mut self) {
        for tool in self.tools.values_mut() {
            tool.observed = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_workspace::{ContextBinding, UnboundReason};

    fn locator(name: &str) -> NoteLocator {
        NoteLocator::from_components([name]).expect("locator")
    }

    fn document_source(name: &str) -> ContextSource {
        ContextSource::Document {
            tab: TabId::from_raw(3),
            locator: locator(name),
            publication: 1,
        }
    }

    #[test]
    fn rebind_issues_one_job_per_new_source_and_drops_stale_results() {
        let mut registry = ContextRegistry::default();
        let tab = TabId::from_raw(9);
        registry.attach(tab, 1, true);

        // First source identity issues a job with binding sequence 1.
        let _binding = ContextBinding::FollowDocument;
        let job = registry
            .rebind(tab, &document_source("a.md"))
            .expect("first source issues a job");
        assert_eq!(job.binding_sequence, 1);
        assert_eq!(job.path, vec!["a.md".to_owned()]);
        assert_eq!(registry.state(&tab), Some(&ToolState::Idle));

        // The same source re-resolves to no new job.
        assert!(registry.rebind(tab, &document_source("a.md")).is_none());

        // A result for a superseded binding sequence cannot paint.
        assert!(!registry.observe(tab, 1, 0, 0, &QueryResult::Missing));
        // A result for a replaced incarnation cannot paint either.
        assert!(!registry.observe(tab, 2, 1, 1, &QueryResult::Missing));

        // A new source rebinds; the old sequence's result is then stale.
        let job = registry
            .rebind(tab, &document_source("b.md"))
            .expect("changed source issues a job");
        assert_eq!(job.binding_sequence, 2);
        registry.submitted(tab, 2, 42);
        assert_eq!(registry.state(&tab), Some(&ToolState::Querying));
        assert!(!registry.observe(
            tab,
            1,
            1,
            1,
            &QueryResult::Preview {
                path: vec!["a.md".to_owned()],
                text: "stale".into(),
                provenance: Provenance {
                    epoch: 0,
                    generation: 0
                }
            }
        ));
        assert!(registry.observe(
            tab,
            1,
            2,
            42,
            &QueryResult::Preview {
                path: vec!["b.md".to_owned()],
                text: "fresh".into(),
                provenance: Provenance {
                    epoch: 1,
                    generation: 2
                }
            }
        ));
        assert!(matches!(
            registry.state(&tab),
            Some(ToolState::Ready(ToolContent::Preview { text, .. })) if text == "fresh"
        ));
    }

    #[test]
    fn unbound_sources_are_typed_and_busy_is_recorded() {
        let mut registry = ContextRegistry::default();
        let tab = TabId::from_raw(4);
        registry.attach(tab, 1, false);
        let binding = ContextBinding::FollowSource {
            tab: TabId::from_raw(77),
        };
        let _ = binding;
        let job = registry.rebind(
            tab,
            &ContextSource::Unbound {
                reason: UnboundReason::SourceClosed(TabId::from_raw(77)),
            },
        );
        assert!(job.is_none(), "unbound sources need no query");
        assert!(matches!(
            registry.state(&tab),
            Some(ToolState::Unbound { .. })
        ));

        // Busy is only recorded for the current binding attempt.
        let job = registry
            .rebind(tab, &document_source("a.md"))
            .expect("source issues a job");
        registry.busy(tab, job.binding_sequence);
        assert_eq!(registry.state(&tab), Some(&ToolState::Busy));
        registry.busy(tab, job.binding_sequence.wrapping_sub(1));
        assert_eq!(registry.state(&tab), Some(&ToolState::Busy));
    }
}
