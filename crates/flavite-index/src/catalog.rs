//! Immutable canonical resolver publication. Both persisted link resolution and
//! guarded Session commits call this algorithm; the publication contains no IO.
//!
//! The catalog doubles as the persistent resolver base (RFC-0004 D2): apply
//! transactions mirror their entity mutations into a sparse [`ResolverOverlay`]
//! and fold it back here at commit, so `load` is only needed at backstops.
//! `upsert_note`/`upsert_attachment`/`upsert_alias`/`remove_note`/
//! `remove_attachment` mutate incrementally; `fold_overlay` applies one
//! transaction's overlay exactly as the transaction wrote the rows.
//!
//! Incremental upserts deliberately do NOT touch `stable`: a group's winner is
//! authoritative only via the overlay's recomputed `stable_winners` (one
//! indexed group query per touched `stable_id_key`), which `fold_overlay`
//! applies after the entity churn.

use crate::resolver::{AttImage, NoteImage, Resolution, ResolverOverlay, ranking};
use crate::schema::ResState;
use crate::types::{AttRef, NoteRef};
use rusqlite::{Connection, Result};
use std::collections::{BTreeMap, BTreeSet};
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
#[derive(Debug, Clone, Default)]
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
    /// An empty publication for the given version coordinates. Apply
    /// transactions build overlays and folds from these primitives instead
    /// of re-reading the vault (RFC-0004 D2).
    #[must_use]
    pub const fn empty(epoch: u64, generation: u64) -> Self {
        Self {
            epoch,
            generation,
            notes: BTreeMap::new(),
            attachments: BTreeMap::new(),
            stable: BTreeMap::new(),
            note_stems: BTreeMap::new(),
            attachment_stems: BTreeMap::new(),
            note_paths: BTreeMap::new(),
            attachment_paths: BTreeMap::new(),
            deep_notes: Vec::new(),
            deep_attachments: Vec::new(),
        }
    }

    /// Mirror a note row mutation exactly: replace any prior record of this
    /// id, then re-index stem, aliases, path suffixes, and deep-path
    /// membership. The `stable` winner map is intentionally untouched —
    /// group winners arrive only through the overlay's `stable_winners`
    /// at fold time, exactly like the SQL group recompute that follows the
    /// row mutation.
    pub fn upsert_note(&mut self, image: &NoteImage) {
        if self.notes.contains_key(&image.id) {
            self.remove_note(image.id);
        }
        let entity = Entity {
            id: image.id,
            path: image.path.clone(),
            path_key: image.path_key.clone(),
            native: image.path_native.clone(),
            note: true,
            title: image.title.clone(),
            stable_id: if image.id_conflict {
                None
            } else {
                image.stable_id.clone()
            },
        };
        self.note_stems
            .entry(image.stem_key.clone())
            .or_default()
            .push(entity.id);
        for alias_key in &image.aliases {
            self.upsert_alias(entity.id, alias_key);
        }
        if entity.path_key.matches('/').count() > 16 {
            self.deep_notes.push(entity.id);
        }
        path_suffixes(&mut self.note_paths, &entity);
        self.notes.insert(entity.id, entity);
    }

    /// Mirror an attachment row mutation exactly (path, suffixes, stem).
    pub fn upsert_attachment(&mut self, image: &AttImage) {
        if self.attachments.contains_key(&image.id) {
            self.remove_attachment(image.id);
        }
        let entity = Entity {
            id: image.id,
            path: image.path.clone(),
            path_key: image.path_key.clone(),
            native: image.path_native.clone(),
            note: false,
            title: String::new(),
            stable_id: None,
        };
        self.attachment_stems
            .entry(image.stem_key.clone())
            .or_default()
            .push(entity.id);
        if entity.path_key.matches('/').count() > 16 {
            self.deep_attachments.push(entity.id);
        }
        path_suffixes(&mut self.attachment_paths, &entity);
        self.attachments.insert(entity.id, entity);
    }

    /// Attach one alias key as a note candidate. Duplicates are prevented by
    /// the aliases primary key upstream; the index keeps one entry per
    /// (alias key, note id) like [`ResolverCatalog::load`].
    pub fn upsert_alias(&mut self, note_id: i64, alias_key: &str) {
        self.note_stems
            .entry(alias_key.to_owned())
            .or_default()
            .push(note_id);
    }

    /// Drop one note record entirely (renamed from `invalidate_note` for
    /// accuracy, RFC-0004 §6.2.1). Deep-path residue is harmless: candidate
    /// lookups always re-resolve the entity record, which is gone here.
    pub(crate) fn remove_note(&mut self, id: i64) {
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

    /// Drop one attachment record entirely (renamed from
    /// `invalidate_attachment`, RFC-0004 §6.2.1).
    pub(crate) fn remove_attachment(&mut self, id: i64) {
        self.attachments.remove(&id);
        for ids in self
            .attachment_stems
            .values_mut()
            .chain(self.attachment_paths.values_mut())
        {
            ids.retain(|value| *value != id);
        }
    }

    /// Fold one transaction's overlay into this base exactly as the
    /// transaction mutated rows (§6.2.4 commit fold). Shadowed ids lose
    /// their old contributions first; updated images are re-indexed from
    /// the overlay's sparse catalog; recomputed group winners apply last.
    pub(crate) fn fold_overlay(&mut self, overlay: &ResolverOverlay) {
        for id in &overlay.shadowed_notes {
            self.remove_note(*id);
        }
        for id in &overlay.shadowed_attachments {
            self.remove_attachment(*id);
        }
        for (stem, ids) in &overlay.current.note_stems {
            self.note_stems
                .entry(stem.clone())
                .or_default()
                .extend(ids.iter().copied());
        }
        for (path, ids) in &overlay.current.note_paths {
            self.note_paths
                .entry(path.clone())
                .or_default()
                .extend(ids.iter().copied());
        }
        for (stem, ids) in &overlay.current.attachment_stems {
            self.attachment_stems
                .entry(stem.clone())
                .or_default()
                .extend(ids.iter().copied());
        }
        for (path, ids) in &overlay.current.attachment_paths {
            self.attachment_paths
                .entry(path.clone())
                .or_default()
                .extend(ids.iter().copied());
        }
        for entity in overlay.current.notes.values() {
            self.notes.insert(entity.id, entity.clone());
        }
        for entity in overlay.current.attachments.values() {
            self.attachments.insert(entity.id, entity.clone());
        }
        self.deep_notes
            .extend(overlay.current.deep_notes.iter().copied());
        self.deep_attachments
            .extend(overlay.current.deep_attachments.iter().copied());
        for (key, winner) in &overlay.stable_winners {
            match winner {
                Some(id) => {
                    self.stable.insert(key.clone(), *id);
                }
                None => {
                    self.stable.remove(key);
                }
            }
        }
    }

    /// Resolve with the current apply transaction's overlay layered over
    /// this base (§6.2.4: overlay first, then base). This is the single D8
    /// body; the persisted and targeted walks both run through it so the
    /// precedence rule can never diverge between them.
    pub(crate) fn resolve_with_overlay(
        &self,
        overlay: Option<&ResolverOverlay>,
        target_key: &str,
        raw: &str,
        embed: bool,
        source: Option<i64>,
    ) -> Resolution {
        let source_path = self
            .note_entity_view(overlay, source)
            .map(|entity| Path::new(&entity.path));
        self.resolve_at_path_with_overlay(overlay, target_key, raw, embed, source_path)
    }

    /// Resolve a link target using the canonical D8 precedence and ranking.
    #[must_use]
    pub fn resolve(
        &self,
        target_key: &str,
        raw: &str,
        embed: bool,
        source: Option<i64>,
    ) -> Resolution {
        self.resolve_with_overlay(None, target_key, raw, embed, source)
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
        self.resolve_at_path_with_overlay(None, target_key, raw, embed, source)
    }

    /// The D8 algorithm proper, reading every index through the
    /// overlay-then-base merge so the no-overlay path is byte-equivalent
    /// to the historical full-load behavior.
    pub(crate) fn resolve_at_path_with_overlay(
        &self,
        overlay: Option<&ResolverOverlay>,
        target_key: &str,
        raw: &str,
        embed: bool,
        source: Option<&Path>,
    ) -> Resolution {
        let folded = flavite_syntax::text::fold_key(target_key);
        let raw_folded = flavite_syntax::text::fold_key(raw);
        if let Some(entity) = raw_folded
            .strip_prefix("id:")
            .and_then(|key| self.stable_winner_view(overlay, key))
            .and_then(|id| self.note_entity_view(overlay, Some(id)))
        {
            return resolved(entity, false);
        }
        if raw.contains('/') {
            let mut notes = self.merged_note_paths(overlay, &raw_folded);
            notes.extend(self.merged_note_paths(overlay, &format!("{raw_folded}.md")));
            notes.extend(
                self.deep_note_entities(overlay)
                    .iter()
                    .filter(|e| {
                        e.path_key.ends_with(&format!("/{raw_folded}"))
                            || e.path_key.ends_with(&format!("/{raw_folded}.md"))
                    })
                    .map(|e| e.id),
            );
            if let Some(result) = self.choose_with(overlay, notes, Vec::new(), source, false) {
                return result;
            }
            let mut attachments = self.merged_attachment_paths(overlay, &raw_folded);
            attachments.extend(
                self.deep_attachment_entities(overlay)
                    .iter()
                    .filter(|e| e.path_key.ends_with(&format!("/{raw_folded}")))
                    .map(|e| e.id),
            );
            if let Some(result) = self.choose_with(overlay, Vec::new(), attachments, source, false)
            {
                return result;
            }
        }
        let stem = crate::resolver::target_stem_key(raw, &folded);
        let mut notes = self.merged_note_stems(overlay, &folded);
        if stem != folded {
            notes.extend(self.merged_note_stems(overlay, &stem));
        }
        let mut attachments = Vec::new();
        if embed {
            attachments.extend(self.merged_attachment_stems(overlay, &folded));
            if stem != folded {
                attachments.extend(self.merged_attachment_stems(overlay, &stem));
            }
        }
        self.choose_with(overlay, notes, attachments, source, embed)
            .unwrap_or_else(Resolution::unresolved)
    }

    fn note_entity_view<'a>(
        &'a self,
        overlay: Option<&'a ResolverOverlay>,
        id: Option<i64>,
    ) -> Option<&'a Entity> {
        let id = id?;
        match overlay {
            Some(overlay) if overlay.shadowed_notes.contains(&id) => overlay.current.notes.get(&id),
            _ => self.notes.get(&id),
        }
    }

    fn attachment_entity_view<'a>(
        &'a self,
        overlay: Option<&'a ResolverOverlay>,
        id: i64,
    ) -> Option<&'a Entity> {
        match overlay {
            Some(overlay) if overlay.shadowed_attachments.contains(&id) => {
                overlay.current.attachments.get(&id)
            }
            _ => self.attachments.get(&id),
        }
    }

    /// Group winner for one `stable_id_key`. An overlay entry is
    /// authoritative even when `None` (the group lost its winner).
    fn stable_winner_view(&self, overlay: Option<&ResolverOverlay>, key: &str) -> Option<i64> {
        match overlay.and_then(|overlay| overlay.stable_winners.get(key)) {
            Some(winner) => *winner,
            None => self.stable.get(key).copied(),
        }
    }

    fn merged_note_stems(&self, overlay: Option<&ResolverOverlay>, key: &str) -> Vec<i64> {
        merge_ids(
            self.note_stems.get(key),
            overlay.map(|overlay| (&overlay.shadowed_notes, &overlay.current.note_stems)),
            key,
        )
    }

    fn merged_attachment_stems(&self, overlay: Option<&ResolverOverlay>, key: &str) -> Vec<i64> {
        merge_ids(
            self.attachment_stems.get(key),
            overlay.map(|overlay| {
                (
                    &overlay.shadowed_attachments,
                    &overlay.current.attachment_stems,
                )
            }),
            key,
        )
    }

    fn merged_note_paths(&self, overlay: Option<&ResolverOverlay>, key: &str) -> Vec<i64> {
        merge_ids(
            self.note_paths.get(key),
            overlay.map(|overlay| (&overlay.shadowed_notes, &overlay.current.note_paths)),
            key,
        )
    }

    fn merged_attachment_paths(&self, overlay: Option<&ResolverOverlay>, key: &str) -> Vec<i64> {
        merge_ids(
            self.attachment_paths.get(key),
            overlay.map(|overlay| {
                (
                    &overlay.shadowed_attachments,
                    &overlay.current.attachment_paths,
                )
            }),
            key,
        )
    }

    /// Merged deep-path note records: the base deep list minus shadowed
    /// ids, plus the overlay's own deep entries (both O(deep paths), never
    /// a full scan — the deep lists exist precisely to bound this branch).
    fn deep_note_entities<'a>(&'a self, overlay: Option<&'a ResolverOverlay>) -> Vec<&'a Entity> {
        let mut out = Vec::new();
        for id in &self.deep_notes {
            if overlay.is_some_and(|overlay| overlay.shadowed_notes.contains(id)) {
                continue;
            }
            if let Some(entity) = self.notes.get(id) {
                out.push(entity);
            }
        }
        if let Some(overlay) = overlay {
            for id in &overlay.current.deep_notes {
                if let Some(entity) = overlay.current.notes.get(id) {
                    out.push(entity);
                }
            }
        }
        out
    }

    fn deep_attachment_entities<'a>(
        &'a self,
        overlay: Option<&'a ResolverOverlay>,
    ) -> Vec<&'a Entity> {
        let mut out = Vec::new();
        for id in &self.deep_attachments {
            if overlay.is_some_and(|overlay| overlay.shadowed_attachments.contains(id)) {
                continue;
            }
            if let Some(entity) = self.attachments.get(id) {
                out.push(entity);
            }
        }
        if let Some(overlay) = overlay {
            for id in &overlay.current.deep_attachments {
                if let Some(entity) = overlay.current.attachments.get(id) {
                    out.push(entity);
                }
            }
        }
        out
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
    fn choose_with(
        &self,
        overlay: Option<&ResolverOverlay>,
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
                if let Some(entity) = self.note_entity_view(overlay, Some(id)) {
                    candidates.push(entity);
                }
            }
        }
        for id in attachments {
            if let Some(entity) = self.attachment_entity_view(overlay, id) {
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

/// Base index entries minus overlay-shadowed ids, plus the overlay's own
/// sparse list for the same key. One merge site for every index kind so
/// resolution and fold can never disagree about what "shadowed" means.
type ShadowedSparse<'a> = (&'a BTreeSet<i64>, &'a BTreeMap<String, Vec<i64>>);

/// Merge one index slice: base ids minus overlay-shadowed entries, plus the
/// overlay's own sparse list for the same key. Without an overlay this is
/// exactly the historical `cloned().unwrap_or_default()` behavior.
fn merge_ids(base: Option<&Vec<i64>>, overlay: Option<ShadowedSparse<'_>>, key: &str) -> Vec<i64> {
    let mut ids = match base {
        Some(list) => match overlay {
            Some((shadowed, _)) => list
                .iter()
                .copied()
                .filter(|id| !shadowed.contains(id))
                .collect(),
            None => list.clone(),
        },
        None => Vec::new(),
    };
    if let Some((_, sparse)) = overlay
        && let Some(extra) = sparse.get(key)
    {
        ids.extend(extra.iter().copied());
    }
    ids
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
