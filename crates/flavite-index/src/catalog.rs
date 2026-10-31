//! Immutable canonical resolver publication. Both persisted link resolution and
//! guarded Session commits call this algorithm; the publication contains no IO.
use crate::resolver::{Resolution, ranking};
use crate::schema::ResState;
use crate::types::{AttRef, NoteRef};
use rusqlite::{Connection, Result};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone)]
struct Entity {
    id: i64,
    path: String,
    path_key: String,
    native: Vec<u8>,
    note: bool,
    title: String,
    stable_id: Option<String>,
}
#[derive(Debug, Clone)]
pub struct ResolverCatalog {
    pub epoch: u64,
    pub generation: u64,
    notes: BTreeMap<i64, Entity>,
    attachments: BTreeMap<i64, Entity>,
    stable: BTreeMap<String, i64>,
    note_stems: BTreeMap<String, Vec<i64>>,
    attachment_stems: BTreeMap<String, Vec<i64>>,
    note_paths: BTreeMap<String, Vec<i64>>,
    attachment_paths: BTreeMap<String, Vec<i64>>,
    deep_notes: Vec<i64>,
    deep_attachments: Vec<i64>,
}
impl ResolverCatalog {
    /// Load inside the caller's consistent database transaction.
    ///
    /// # Errors
    /// Returns a SQLite error for malformed or unavailable catalog rows.
    pub fn load(conn: &Connection) -> Result<Self> {
        let mut catalog = Self {
            epoch: crate::schema::get_epoch(conn)?,
            generation: crate::schema::get_generation(conn)?,
            notes: BTreeMap::new(),
            attachments: BTreeMap::new(),
            stable: BTreeMap::new(),
            note_stems: BTreeMap::new(),
            attachment_stems: BTreeMap::new(),
            note_paths: BTreeMap::new(),
            attachment_paths: BTreeMap::new(),
            deep_notes: Vec::new(),
            deep_attachments: Vec::new(),
        };
        let mut stmt=conn.prepare("SELECT note_id,path,path_key,path_native,stem_key,stable_id_key,title,stable_id,id_conflict FROM notes ORDER BY path_key,note_id")?;
        for row in stmt.query_map([], |r| {
            Ok((
                Entity {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    path_key: r.get(2)?,
                    native: r.get(3)?,
                    note: true,
                    title: r.get(6)?,
                    stable_id: if r.get::<_, i64>(8)? == 0 {
                        r.get(7)?
                    } else {
                        None
                    },
                },
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })? {
            let (entity, stem, stable) = row?;
            catalog.note_stems.entry(stem).or_default().push(entity.id);
            if let Some(stable) = stable {
                catalog.stable.entry(stable).or_insert(entity.id);
            }
            if entity.path_key.matches('/').count() > 16 {
                catalog.deep_notes.push(entity.id);
            }
            path_suffixes(&mut catalog.note_paths, &entity);
            catalog.notes.insert(entity.id, entity);
        }
        let mut stmt = conn.prepare("SELECT note_id,alias_key FROM aliases")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (id, alias) = row?;
            catalog.note_stems.entry(alias).or_default().push(id);
        }
        let mut stmt=conn.prepare("SELECT att_id,path,path_key,path_native,stem_key FROM attachments ORDER BY path_key,att_id")?;
        for row in stmt.query_map([], |r| {
            Ok((
                Entity {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    path_key: r.get(2)?,
                    native: r.get(3)?,
                    note: false,
                    title: String::new(),
                    stable_id: None,
                },
                r.get::<_, String>(4)?,
            ))
        })? {
            let (entity, stem) = row?;
            catalog
                .attachment_stems
                .entry(stem)
                .or_default()
                .push(entity.id);
            path_suffixes(&mut catalog.attachment_paths, &entity);
            if entity.path_key.matches('/').count() > 16 {
                catalog.deep_attachments.push(entity.id);
            }
            catalog.attachments.insert(entity.id, entity);
        }
        Ok(catalog)
    }
    /// Resolve using the same D8 precedence and ranking as index ingestion.
    #[must_use]
    pub fn resolve(
        &self,
        target_key: &str,
        raw: &str,
        embed: bool,
        source: Option<i64>,
    ) -> Resolution {
        self.resolve_at_path(
            target_key,
            raw,
            embed,
            source
                .and_then(|id| self.notes.get(&id))
                .map(|entity| Path::new(&entity.path)),
        )
    }

    /// Canonical resolution for an open source that is not indexed yet.
    #[must_use]
    pub fn resolve_at_path(
        &self,
        target_key: &str,
        raw: &str,
        embed: bool,
        source: Option<&Path>,
    ) -> Resolution {
        let folded = flavite_syntax::text::fold_key(target_key);
        let raw_folded = flavite_syntax::text::fold_key(raw);
        if let Some(id) = raw_folded
            .strip_prefix("id:")
            .and_then(|key| self.stable.get(key))
            .and_then(|id| self.notes.get(id))
        {
            return resolved(id, false);
        }
        if raw.contains('/') {
            let mut notes = self
                .note_paths
                .get(&raw_folded)
                .cloned()
                .unwrap_or_default();
            notes.extend(
                self.note_paths
                    .get(&format!("{raw_folded}.md"))
                    .into_iter()
                    .flatten(),
            );
            notes.extend(self.deep_notes.iter().filter_map(|id| {
                self.notes
                    .get(id)
                    .filter(|e| {
                        e.path_key.ends_with(&format!("/{raw_folded}"))
                            || e.path_key.ends_with(&format!("/{raw_folded}.md"))
                    })
                    .map(|e| e.id)
            }));
            if let Some(result) = self.choose(notes, Vec::new(), source, false) {
                return result;
            }
            let mut attachments = self
                .attachment_paths
                .get(&raw_folded)
                .cloned()
                .unwrap_or_default();
            attachments.extend(self.deep_attachments.iter().filter_map(|id| {
                self.attachments
                    .get(id)
                    .filter(|e| e.path_key.ends_with(&format!("/{raw_folded}")))
                    .map(|e| e.id)
            }));
            if let Some(result) = self.choose(Vec::new(), attachments, source, false) {
                return result;
            }
        }
        let stem = Path::new(raw)
            .file_stem()
            .and_then(|s| s.to_str())
            .map_or_else(|| folded.clone(), flavite_syntax::text::fold_key);
        let mut notes = self.note_stems.get(&folded).cloned().unwrap_or_default();
        if stem != folded {
            notes.extend(self.note_stems.get(&stem).into_iter().flatten());
        }
        let mut attachments = Vec::new();
        if embed {
            attachments.extend(self.attachment_stems.get(&folded).into_iter().flatten());
            if stem != folded {
                attachments.extend(self.attachment_stems.get(&stem).into_iter().flatten());
            }
        }
        self.choose(notes, attachments, source, embed)
            .unwrap_or_else(Resolution::unresolved)
    }
    /// Locate a source's native path in this exact epoch.
    #[must_use]
    pub fn note_for_path(&self, path: &str) -> Option<i64> {
        let folded = flavite_syntax::text::fold_key(path);
        self.note_paths.get(&folded)?.iter().find_map(|id| {
            self.notes
                .get(id)
                .filter(|n| n.path_key == folded)
                .map(|n| n.id)
        })
    }
    /// Exact target data from this immutable publication.
    #[must_use]
    pub fn note_data(&self, id: i64) -> Option<(&str, &str, Option<&str>, &[u8])> {
        self.notes.get(&id).map(|n| {
            (
                n.path.as_str(),
                n.title.as_str(),
                n.stable_id.as_deref(),
                n.native.as_slice(),
            )
        })
    }
    pub(crate) fn invalidate_note(&mut self, id: i64) {
        self.notes.remove(&id);
        self.stable.retain(|_, value| *value != id);
        for ids in self
            .note_stems
            .values_mut()
            .chain(self.note_paths.values_mut())
        {
            ids.retain(|value| *value != id);
        }
    }
    pub(crate) fn invalidate_attachment(&mut self, id: i64) {
        self.attachments.remove(&id);
        for ids in self
            .attachment_stems
            .values_mut()
            .chain(self.attachment_paths.values_mut())
        {
            ids.retain(|value| *value != id);
        }
    }
    fn choose(
        &self,
        mut notes: Vec<i64>,
        mut attachments: Vec<i64>,
        source: Option<&Path>,
        embed: bool,
    ) -> Option<Resolution> {
        notes.sort_unstable();
        notes.dedup();
        attachments.sort_unstable();
        attachments.dedup();
        let prefer_attachments = embed && !attachments.is_empty();
        let count = if prefer_attachments {
            attachments.len()
        } else {
            notes.len() + attachments.len()
        };
        let source_dir = source.and_then(Path::parent);
        let mut candidates = Vec::new();
        if !prefer_attachments {
            for id in notes {
                if let Some(entity) = self.notes.get(&id) {
                    candidates.push(entity);
                }
            }
        }
        for id in attachments {
            if let Some(entity) = self.attachments.get(&id) {
                candidates.push(entity);
            }
        }
        let mut ranks: Vec<_> = candidates
            .iter()
            .map(|e| ranking::Candidate {
                same_directory: source_dir
                    .is_some_and(|dir| Path::new(&e.path).parent() == Some(dir)),
                depth: Path::new(&e.path).components().count(),
                path: e.path.clone(),
                is_note: e.note,
                id: e.id,
            })
            .collect();
        ranking::rank_candidates(&mut ranks);
        let best = ranks.first()?;
        candidates
            .into_iter()
            .find(|e| e.id == best.id && e.note == best.is_note)
            .map(|entity| resolved(entity, count > 1))
    }
}
fn path_suffixes(paths: &mut BTreeMap<String, Vec<i64>>, entity: &Entity) {
    paths
        .entry(entity.path_key.clone())
        .or_default()
        .push(entity.id);
    // Existing vault paths are bounded. Store at most 16 suffixes per record;
    // deeper paths still resolve by their complete vault-relative spelling.
    for (index, _) in entity.path_key.match_indices('/').rev().take(16) {
        paths
            .entry(entity.path_key[index + 1..].to_owned())
            .or_default()
            .push(entity.id);
    }
}
fn resolved(entity: &Entity, ambiguous: bool) -> Resolution {
    let mut result = if entity.note {
        Resolution::resolved_note(NoteRef::new(entity.id, entity.native.clone()))
    } else {
        Resolution::resolved_attachment(AttRef::new(entity.id, entity.native.clone()))
    };
    result.is_ambiguous = ambiguous;
    if ambiguous {
        result.state = ResState::Ambiguous;
    }
    result
}
