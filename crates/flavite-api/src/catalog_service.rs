//! Toolkit-free completion snapshots prepared by the bounded catalog worker.

/// An immutable suggestion bound to the indexed target captured with its catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub identity: Option<crate::LinkTargetIdentity>,
    pub target: String,
    pub title: String,
    pub aliases: Vec<String>,
    pub stable_id: Option<String>,
}

impl CompletionCandidate {
    /// Construct presentation-only data; unbound candidates cannot authorize edits.
    pub fn new(target: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            identity: None,
            target: target.into(),
            title: title.into(),
            aliases: Vec::new(),
            stable_id: None,
        }
    }
}

pub(crate) fn completion_snapshot(
    entries: &[crate::NoteCatalogEntry],
) -> std::sync::Arc<[CompletionCandidate]> {
    entries
        .iter()
        .filter_map(|entry| {
            let path = entry.path.to_str()?;
            Some(CompletionCandidate {
                identity: entry.identity.clone(),
                target: path.strip_suffix(".md").unwrap_or(path).to_owned(),
                title: entry.title.clone(),
                aliases: entry.aliases.clone(),
                stable_id: entry.stable_id.clone(),
            })
        })
        .collect::<Vec<_>>()
        .into()
}

use crate::{ContextQueryResult, MAX_NOTE_CATALOG_LIMIT, NoteCatalogEntry, Provenance};
use flavite_vault::session::Session;
use std::sync::Arc;

pub(crate) fn execute_catalog(session: &Arc<Session>, limit: u64) -> ContextQueryResult {
    let limit = limit.clamp(1, u64::try_from(MAX_NOTE_CATALOG_LIMIT).unwrap_or(u64::MAX));
    let mut after = None;
    let mut entries = Vec::new();
    let mut snapshot_bytes = 0usize;
    loop {
        let page = match session.note_catalog(500, after) {
            Ok(page) => page,
            Err(message) => return ContextQueryResult::Failed { message },
        };
        if page.total > limit {
            return ContextQueryResult::Failed {
                message: "Catalog limit exceeded; reduce scope before retrying".into(),
            };
        }
        for item in &page.items {
            let fields = [
                item.path.len(),
                item.path_native.len(),
                item.title.len(),
                item.stable_id.as_ref().map_or(0, String::len),
            ];
            for bytes in fields
                .into_iter()
                .chain(item.aliases.iter().map(String::len))
            {
                // Reserve for the catalog plus its picker/editor presentation copies.
                snapshot_bytes = match bytes
                    .checked_mul(4)
                    .and_then(|n| snapshot_bytes.checked_add(n))
                {
                    Some(total) if total <= 64 * 1024 * 1024 => total,
                    _ => {
                        return ContextQueryResult::Failed {
                            message: "Catalog memory limit exceeded; reduce scope before retrying"
                                .into(),
                        };
                    }
                };
            }
        }
        let paths = match session.decode_graph_paths(
            page.items
                .iter()
                .map(|item| item.path_native.clone())
                .collect(),
        ) {
            Ok(paths) if paths.len() == page.items.len() => paths,
            _ => {
                return ContextQueryResult::Failed {
                    message: "Catalog contains an invalid native identity".into(),
                };
            }
        };
        let resolver = match session.resolver_catalog() {
            Ok(resolver)
                if (resolver.epoch, resolver.generation) == (page.epoch, page.generation) =>
            {
                resolver
            }
            _ => return ContextQueryResult::Updating,
        };
        entries.extend(
            page.items
                .into_iter()
                .zip(paths)
                .map(|(item, path)| NoteCatalogEntry {
                    identity: {
                        let raw = item.path.strip_suffix(".md").unwrap_or(&item.path);
                        resolver
                            .resolve(&flavite_syntax::text::fold_key(raw), raw, false, None)
                            .note
                            .filter(|note| note.path_native == item.path_native)
                            .and_then(|note| {
                                u64::try_from(note.note_id).ok().map(|entity_id| {
                                    crate::LinkTargetIdentity {
                                        epoch: page.epoch,
                                        entity_id,
                                        native_path: note.path_native,
                                    }
                                })
                            })
                    },
                    path,
                    native_path: item.path_native,
                    title: item.title,
                    aliases: item.aliases,
                    stable_id: item.stable_id,
                }),
        );
        after = page.next;
        if after.is_none() {
            return ContextQueryResult::Catalog {
                candidates: crate::catalog_service::completion_snapshot(&entries),
                total: page.total,
                provenance: Provenance {
                    epoch: page.epoch,
                    generation: page.generation,
                },
                entries: Arc::from(entries),
                truncated: false,
            };
        }
    }
}
