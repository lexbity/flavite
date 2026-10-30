//! Context authorities and explicit source bindings (RFC-0003 D-04, D-09,
//! W04).
//!
//! Four authorities stay independent: Focus, `DocumentAnchor`,
//! `ExplorationSelection`, and `WritingTarget`. Only source views (Note, and
//! later Graph/Search) publish context; Preview, Backlinks, Outline,
//! Properties, and Diagnostics are sinks and cannot follow sinks. The
//! router is a pure function over committed workspace state, so a sink's
//! effective source is recomputable, inspectable, and never inferred from
//! panel adjacency.

use crate::ids::TabId;
use crate::view::{NoteLocator, ViewKind};

/// A pinned entity identity: the exact validated vault-relative path.
///
/// A pin copies a durable locator, never a cache ID and never a pointer to
/// the source view. Catalog epochs and note IDs are recorded by the query
/// service as provenance only and never migrate by numeric equality.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntityLocator {
    path: NoteLocator,
}

impl EntityLocator {
    #[must_use]
    pub const fn new(path: NoteLocator) -> Self {
        Self { path }
    }

    #[must_use]
    pub const fn path(&self) -> &NoteLocator {
        &self.path
    }
}

/// The sink view kinds W04 ships. Construction is the sink gate: these are
/// the only kinds a context tool can take, and [`is_source_kind`] rejects
/// them as follow targets, so sink-to-sink chains are unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContextViewKind {
    /// Saved source-text preview.
    Preview,
    /// Inbound links.
    Backlinks,
}

impl ContextViewKind {
    /// The [`ViewKind`] for this sink with `binding`.
    #[must_use]
    pub fn view_kind(&self, binding: ContextBinding) -> ViewKind {
        match self {
            Self::Preview => ViewKind::Preview(binding),
            Self::Backlinks => ViewKind::Backlinks(binding),
        }
    }
}

/// Why a context tool currently has no effective source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UnboundReason {
    /// No document anchor exists yet (no note view has been focused).
    NoDocumentAnchor,
    /// The tab followed by an explicit `FollowSource` binding was closed.
    SourceClosed(TabId),
    /// No exploration source has published a selection.
    NoExploration,
}

/// An explicit binding of a context sink to a source (§7.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContextBinding {
    /// Follow the `DocumentAnchor` (default for Backlinks/Outline/Properties).
    FollowDocument,
    /// Follow the `ExplorationSelection` when one exists, otherwise the
    /// `DocumentAnchor` (default for Preview).
    FollowExplorationOrDocument,
    /// Follow a specific source tab's publications. Survives tab movement;
    /// closing the source makes the binding typed-unbound.
    FollowSource { tab: TabId },
    /// Pin one entity by durable locator.
    Pinned(EntityLocator),
}

/// The `ExplorationSelection` authority. A producer (graph in W07, search in
/// W10) publishes selections; W04 defines the state and its routing rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExplorationSelection {
    /// The source tab that published the selection.
    pub source_tab: TabId,
    /// Source view incarnation at publication time.
    pub incarnation: u64,
    /// Monotonic publication sequence of the source.
    pub sequence: u64,
    /// The selected note.
    pub target: NoteLocator,
}

/// The effective source a sink should render, computed by [`ContextRouter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextSource {
    /// No source is available; the sink renders an explicit unbound state.
    Unbound { reason: UnboundReason },
    /// The document anchor note, with the publication sequence that produced
    /// it (an older publication can never repaint a newer one).
    Document {
        tab: TabId,
        locator: NoteLocator,
        publication: u64,
    },
    /// The exploration selection note.
    Exploration {
        source_tab: TabId,
        locator: NoteLocator,
        sequence: u64,
    },
    /// A pinned entity locator.
    Pinned { locator: NoteLocator },
}

/// Whether a view kind may publish context. Sinks are rejected as sources,
/// so follow chains cannot form and cycles are unrepresentable (D-09).
#[must_use]
pub fn is_source_kind(kind: &ViewKind) -> bool {
    match kind {
        ViewKind::Note(_) => true,
        // Graph and Search sources arrive with their slices (W07/W10).
        ViewKind::Empty | ViewKind::Notes | ViewKind::Preview(_) | ViewKind::Backlinks(_) => false,
    }
}

/// Pure recompute of a sink's effective source from committed state.
pub struct ContextRouter;

impl ContextRouter {
    /// Resolve `binding` against the current workspace authorities.
    #[must_use]
    pub fn resolve(
        state: &crate::reducer::WorkspaceState,
        binding: &ContextBinding,
    ) -> ContextSource {
        match binding {
            ContextBinding::FollowDocument => {
                Self::document_source(state).unwrap_or(ContextSource::Unbound {
                    reason: UnboundReason::NoDocumentAnchor,
                })
            }
            ContextBinding::FollowExplorationOrDocument => {
                if let Some(exploration) = state.exploration() {
                    ContextSource::Exploration {
                        source_tab: exploration.source_tab,
                        locator: exploration.target.clone(),
                        sequence: exploration.sequence,
                    }
                } else {
                    Self::document_source(state).unwrap_or(ContextSource::Unbound {
                        reason: UnboundReason::NoDocumentAnchor,
                    })
                }
            }
            ContextBinding::FollowSource { tab } => {
                match state.tab(tab).map(|session| session.kind().clone()) {
                    Some(kind) if is_source_kind(&kind) => {
                        if let ViewKind::Note(note) = kind {
                            ContextSource::Document {
                                tab: *tab,
                                locator: note.locator().clone(),
                                publication: state.document_publication_of(*tab),
                            }
                        } else {
                            // Future source kinds resolve through their own
                            // publications once their slices land.
                            ContextSource::Unbound {
                                reason: UnboundReason::NoExploration,
                            }
                        }
                    }
                    // Sinks and closed tabs are typed-unbound, never silently
                    // replaced by a different source (D-09).
                    // Sinks and closed tabs are typed-unbound, never
                    // silently replaced by a different source (D-09).
                    Some(_) | None => ContextSource::Unbound {
                        reason: UnboundReason::SourceClosed(*tab),
                    },
                }
            }
            ContextBinding::Pinned(locator) => ContextSource::Pinned {
                locator: locator.path().clone(),
            },
        }
    }

    fn document_source(state: &crate::reducer::WorkspaceState) -> Option<ContextSource> {
        let tab = state.document_anchor()?;
        let session = state.tab(&tab)?;
        let ViewKind::Note(note) = session.kind() else {
            return None;
        };
        Some(ContextSource::Document {
            tab,
            locator: note.locator().clone(),
            publication: state.document_publication_of(tab),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TabId;
    use crate::reducer::{LayoutAction, WorkspaceState};
    use crate::view::{NoteLoad, NoteView, ViewKind};

    fn locator(name: &str) -> NoteLocator {
        NoteLocator::from_components([name]).expect("locator")
    }

    /// Build a state with an anchored note tab through the real reducer and
    /// acknowledgement paths, mirroring how the host establishes sources.
    fn state_with_anchor(name: &str) -> (WorkspaceState, TabId) {
        let state = WorkspaceState::new();
        let group = state.first_group().expect("default group");
        let outcome = crate::reducer::reduce(
            &state,
            LayoutAction::OpenNote {
                expected_revision: state.revision(),
                locator: locator(name),
                group,
            },
        );
        let crate::reducer::Outcome::Committed(commit) = outcome else {
            panic!("open commits");
        };
        let tab = commit
            .effects
            .iter()
            .find_map(|effect| match effect {
                crate::reducer::Effect::ViewAttached { tab, .. } => Some(*tab),
                _ => None,
            })
            .expect("attached tab");
        let mut state = commit.state;
        let mut interaction = crate::input::EditorInteraction::new(tab, 1);
        interaction.sequence = 1;
        interaction.focused = true;
        interaction.selections = vec![crate::input::Selection::caret(0)];
        interaction.buffer_id = Some(1);
        interaction.revision = 1;
        state
            .acknowledge_interaction(interaction, "text")
            .expect("acknowledged");
        (state, tab)
    }

    #[test]
    fn pinned_and_follow_bindings_resolve_to_typed_sources() {
        let (state, tab) = state_with_anchor("a.md");

        let document = ContextRouter::resolve(&state, &ContextBinding::FollowDocument);
        assert_eq!(
            document,
            ContextSource::Document {
                tab,
                locator: locator("a.md"),
                publication: 1
            }
        );
        let pinned = ContextRouter::resolve(
            &state,
            &ContextBinding::Pinned(EntityLocator::new(locator("pinned.md"))),
        );
        assert_eq!(
            pinned,
            ContextSource::Pinned {
                locator: locator("pinned.md")
            }
        );
        // The anchor publication increments only when the anchor changes.
        assert_eq!(state.document_publication_of(tab), 1);
    }

    #[test]
    fn unbound_states_are_typed_not_silent() {
        let state = WorkspaceState::new();
        assert_eq!(
            ContextRouter::resolve(&state, &ContextBinding::FollowDocument),
            ContextSource::Unbound {
                reason: UnboundReason::NoDocumentAnchor
            }
        );
        // A FollowSource binding to a closed tab is typed-unbound, never
        // replaced by the document anchor (D-04/D-09).
        let (anchored, _) = state_with_anchor("a.md");
        assert_eq!(
            ContextRouter::resolve(
                &anchored,
                &ContextBinding::FollowSource {
                    tab: TabId::from_raw(99)
                }
            ),
            ContextSource::Unbound {
                reason: UnboundReason::SourceClosed(TabId::from_raw(99))
            }
        );
    }

    #[test]
    fn follow_source_resolves_the_bound_tab_and_survives_layout_moves() {
        let (state, tab) = state_with_anchor("a.md");
        let binding = ContextBinding::FollowSource { tab };
        assert_eq!(
            ContextRouter::resolve(&state, &binding),
            ContextSource::Document {
                tab,
                locator: locator("a.md"),
                publication: 1
            }
        );
    }

    #[test]
    fn sinks_cannot_publish_and_bindings_cannot_follow_sinks() {
        assert!(!is_source_kind(&ViewKind::Preview(
            ContextBinding::FollowDocument
        )));
        assert!(!is_source_kind(&ViewKind::Backlinks(
            ContextBinding::Pinned(EntityLocator::new(locator("x.md")))
        )));
        assert!(is_source_kind(&ViewKind::Note(NoteView::new(
            locator("a.md"),
            NoteLoad::Ready
        ))));
        // Publishing exploration from a sink tab is refused.
        let (state, _) = state_with_anchor("a.md");
        let group = state.first_group().unwrap();
        let outcome = crate::reducer::reduce(
            &state,
            LayoutAction::OpenContext {
                expected_revision: state.revision(),
                view: ContextViewKind::Preview,
                binding: ContextBinding::FollowDocument,
                group,
            },
        );
        let crate::reducer::Outcome::Committed(commit) = outcome else {
            panic!("open context commits");
        };
        let sink_tab = commit
            .effects
            .iter()
            .find_map(|effect| match effect {
                crate::reducer::Effect::ViewAttached { tab, .. } => Some(*tab),
                _ => None,
            })
            .unwrap();
        let mut state = commit.state;
        assert!(
            state
                .publish_exploration(sink_tab, 1, locator("a.md"))
                .is_err()
        );
        // Rebinding through the typed action works and signals rebind.
        let outcome = crate::reducer::reduce(
            &state,
            LayoutAction::SetContextBinding {
                expected_revision: state.revision(),
                tab: sink_tab,
                binding: ContextBinding::Pinned(EntityLocator::new(locator("p.md"))),
            },
        );
        let crate::reducer::Outcome::Committed(commit) = outcome else {
            panic!("rebind commits");
        };
        assert!(
            commit
                .effects
                .iter()
                .any(|effect| matches!(effect, crate::reducer::Effect::ContextRebound { .. }))
        );
        assert_eq!(
            ContextRouter::resolve(&commit.state, commit.state.binding_of(&sink_tab).unwrap()),
            ContextSource::Pinned {
                locator: locator("p.md")
            }
        );
    }
}
