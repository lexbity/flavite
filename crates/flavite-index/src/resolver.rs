//! Link resolver (§4.3, D8, RFC-0004 W-A).
//!
//! This module implements the 5-step resolution algorithm from the specification:
//!
//! 1. `id:` prefix → `stable_id` lookup.
//! 2. Path-suffix match if the target contains `/` (notes, then attachments).
//! 3. Stem match over `notes.stem_key ∪ aliases.alias_key ∪ attachments.stem_key`.
//!    For `![[embed]]`, an attachment match wins over a note match.
//!    For `[[link]]`, a note match wins.
//! 4. If multiple candidates → rank: same directory as source, then shallowest,
//!    then lexicographic. Mark **`ambiguous`**, pick deterministically, and
//!    surface it in Diagnostics.
//! 5. Zero candidates → `unresolved`. Clicking a note-link creates the note.
//!    Clicking an embed whose target has an attachment-like extension offers
//!    *locate file*; clicking an attachment link opens it with the OS default
//!    application.
//!
//! Duplicate IDs are co-ingested. The lowest folded path key wins `id:` lookup;
//! remaining rows carry `id_conflict` and are reported by doctor.
//!
//! # Backstop policy (D13, RFC-0004 §6.2.5)
//!
//! Targeted resolution is superset-sound by construction (§4.2, FR-3): a
//! changed entity E re-resolves every link whose `target_key` is in
//! `K(pre) ∪ K(post) ∪ fresh-own-outgoing-keys`, fetched through the
//! `links_target` index. Redundant re-resolution is acceptable (idempotent);
//! a missed link is a correctness bug — the equality property test is the
//! arbiter. The whole-vault `re_resolve_all` walk is backstop-only: epoch
//! change/database recreate (fresh [`ResolverState`] load), every
//! [`RESOLVER_BACKSTOP_GENERATIONS`] applied generations, doctor/repair, and
//! parity failure. Rollback safety is structural: overlays and pending
//! backstop catalogs are dropped, never inverted; SQLite remains the sole
//! truth.

use crate::catalog::ResolverCatalog;
use crate::schema::ResState;
use crate::types::{AttRef, NoteRef};
use rusqlite::{Connection, OptionalExtension, Result, Transaction, params};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Pure candidate ordering used by persisted and on-demand resolution.
/// Candidate collection remains index-owned; this module owns the ranking
/// rule itself.
pub mod ranking {
    /// Candidate attributes used by the deterministic resolver ranking.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Candidate {
        pub same_directory: bool,
        pub depth: usize,
        pub path: String,
        pub is_note: bool,
        pub id: i64,
    }

    /// Sort same-directory candidates first, then shallowest path, then
    /// lexicographic folded display path.
    pub fn rank_candidates(candidates: &mut [Candidate]) {
        candidates.sort_by(|a, b| {
            b.same_directory
                .cmp(&a.same_directory)
                .then(a.depth.cmp(&b.depth))
                .then(a.path.cmp(&b.path))
        });
    }
}

/// Link resolution result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The resolved note reference (if resolved to a note).
    pub note: Option<NoteRef>,
    /// The resolved attachment reference (if resolved to an attachment).
    pub attachment: Option<AttRef>,
    /// Whether the resolution is ambiguous.
    pub is_ambiguous: bool,
    /// Whether the target was unresolved.
    pub is_unresolved: bool,
    /// The resolution state.
    pub state: ResState,
}

impl Resolution {
    #[must_use]
    pub fn resolved_note(note: NoteRef) -> Self {
        Self {
            note: Some(note),
            attachment: None,
            is_ambiguous: false,
            is_unresolved: false,
            state: ResState::Resolved,
        }
    }

    #[must_use]
    pub fn resolved_attachment(attachment: AttRef) -> Self {
        Self {
            note: None,
            attachment: Some(attachment),
            is_ambiguous: false,
            is_unresolved: false,
            state: ResState::Resolved,
        }
    }

    #[must_use]
    pub fn ambiguous() -> Self {
        Self {
            note: None,
            attachment: None,
            is_ambiguous: true,
            is_unresolved: false,
            state: ResState::Ambiguous,
        }
    }

    #[must_use]
    pub fn unresolved() -> Self {
        Self {
            note: None,
            attachment: None,
            is_ambiguous: false,
            is_unresolved: true,
            state: ResState::Unresolved,
        }
    }
}

/// Generations between whole-vault re-resolves (D13). Amortized cost at the
/// design bound is < 0.2 ms per apply (NFR-7).
pub const RESOLVER_BACKSTOP_GENERATIONS: u64 = 4096;

/// IN-list chunk size for targeted candidate fetches (§6.2.3). Headroom
/// below SQLite's parameter limit.
const AFFECTED_KEY_CHUNK: usize = 900;

/// A note's matchable identity, as an image before (`pre`) or after
/// (`post`) a mutation (§6.2.2). Plain data: the apply paths build it from
/// rows they read or write, and both images feed `note_identity_keys`
/// (D3 pre-image).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteImage {
    pub id: i64,
    pub path: String,
    pub path_native: Vec<u8>,
    pub path_key: String,
    pub stem_key: String,
    pub title: String,
    /// Stored `alias_key` values (the resolver's indexed form), ascending.
    pub aliases: Vec<String>,
    pub stable_id: Option<String>,
    pub stable_id_key: Option<String>,
    pub id_conflict: bool,
    pub content_hash: Option<Vec<u8>>,
}

/// An attachment's matchable identity image (§6.2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttImage {
    pub id: i64,
    pub path: String,
    pub path_native: Vec<u8>,
    pub path_key: String,
    pub stem_key: String,
}

/// Every stored `target_key` form that can resolve to note E (§6.2.2).
/// Superset by construction; the equality gate (FR-2) is the arbiter.
/// Co-located with the resolver so future target forms extend in one place.
#[must_use]
pub fn note_identity_keys(image: &NoteImage) -> BTreeSet<String> {
    let mut keys = path_key_forms(&image.path_key);
    keys.insert(image.stem_key.clone());
    for alias in &image.aliases {
        keys.insert(alias.clone());
        keys.insert(flavite_syntax::text::fold_key(alias));
    }
    if let Some(stable_id) = &image.stable_id {
        keys.insert(flavite_syntax::text::fold_key(stable_id));
        keys.insert(flavite_syntax::text::fold_key(&format!("id:{stable_id}")));
    }
    if let Some(stable_id_key) = &image.stable_id_key {
        keys.insert(stable_id_key.clone());
        keys.insert(flavite_syntax::text::fold_key(&format!(
            "id:{stable_id_key}"
        )));
    }
    keys
}

/// Every stored `target_key` form that can resolve to attachment E:
/// path, every path suffix (and their `.md`-stripped consult inverses —
/// the catalog tries `raw + ".md"` on the note side, so exact spellings
/// stay on the attachment side; keeping the stripped forms is harmless
/// superset), stem (§6.2.2).
#[must_use]
pub fn attachment_identity_keys(image: &AttImage) -> BTreeSet<String> {
    let mut keys = path_key_forms(&image.path_key);
    keys.insert(image.stem_key.clone());
    keys
}

/// Every suffix of a folded vault path after each `/`, unbounded (the
/// catalog itself caps stored suffixes; the candidate fetch must not).
fn path_suffix_forms(path_key: &str) -> impl Iterator<Item = String> + '_ {
    path_key
        .match_indices('/')
        .map(move |(index, _)| path_key[index + 1..].to_owned())
}

/// Every path form an exact-`target_key` fetch can hit for this folded
/// path: the path, every suffix, and the catalog's `raw + ".md"` consult
/// inverse — each minus a trailing `.md` (a link written as `folder/idea`
/// resolves through `note_paths["folder/idea.md"]`, so `folder/idea` is a
/// matchable stored form of the entity).
fn path_key_forms(path_key: &str) -> BTreeSet<String> {
    let mut forms = BTreeSet::new();
    forms.insert(path_key.to_owned());
    forms.extend(path_suffix_forms(path_key));
    for base in forms.clone() {
        if let Some(stripped) = base.strip_suffix(".md") {
            forms.insert(stripped.to_owned());
        }
    }
    forms
}

/// Final-component stem of a raw target key, exactly as `resolve_at_path`
/// computes it — one implementation for the catalog's stem-fallback branch
/// and the stored `links.target_stem` column, so the two can never drift.
#[must_use]
pub fn target_stem_key(target_raw: &str, folded_target_key: &str) -> String {
    Path::new(target_raw)
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(
            || folded_target_key.to_owned(),
            flavite_syntax::text::fold_key,
        )
}

/// The stem-space keys an entity is discoverable through: its stem plus its
/// alias keys — the consult surface of the catalog's stem-fallback branch.
/// Links whose final-component stem equals one of these can resolve to the
/// entity from any directory or extension spelling, which is what the
/// indexed `target_stem` arm of the candidate fetch exists to capture.
fn entity_stem_keys(image: &EntityPre) -> BTreeSet<String> {
    match image {
        EntityPre::Note(note) => {
            let mut keys = BTreeSet::new();
            keys.insert(note.stem_key.clone());
            keys.extend(note.aliases.iter().cloned());
            keys
        }
        EntityPre::Attachment(attachment) => BTreeSet::from([attachment.stem_key.clone()]),
    }
}

/// The entity this apply transaction mutated, before and/or after the
/// mutation. Removals carry only the pre-image; upserts carry both.
#[derive(Debug, Clone)]
pub enum EntityPre {
    Note(NoteImage),
    Attachment(AttImage),
}

impl EntityPre {
    /// `stable_id_key` whose group membership can move for this image.
    #[must_use]
    pub fn stable_id_key(&self) -> Option<&String> {
        match self {
            Self::Note(image) => image.stable_id_key.as_ref(),
            Self::Attachment(_) => None,
        }
    }
}

/// The candidate-fetch key set for one mutation (FR-3):
/// `K(pre) ∪ K(post) ∪ fresh outgoing keys`, split into the two indexed
/// arms the fetch runs: exact `target_key` forms and the stem-fallback
/// surface (`target_stem`, schema 3.1). A pass with both sets empty has
/// nothing to re-resolve (the D4 short-circuit).
#[derive(Debug, Default)]
pub struct AffectedKeys {
    pub exact: BTreeSet<String>,
    pub stems: BTreeSet<String>,
}

impl AffectedKeys {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.stems.is_empty()
    }
}

/// Union of both identity-key sides plus every freshly written outgoing
/// target key (FR-3: `K(pre) ∪ K(post) ∪ fresh outgoing keys`). When the
/// mutation is identity-irrelevant (D4) the caller passes `pre`/`post`
/// `None`, leaving only the fresh-outgoing component (whose own rows are
/// always exact-key hits — the stem arm contributes only on relevance).
#[must_use]
pub fn affected_resolution_keys(
    pre: Option<&EntityPre>,
    post: Option<&EntityPre>,
    fresh_target_keys: &BTreeSet<String>,
) -> AffectedKeys {
    let mut keys = AffectedKeys::default();
    for image in pre.into_iter().chain(post) {
        match image {
            EntityPre::Note(note) => keys.exact.extend(note_identity_keys(note)),
            EntityPre::Attachment(attachment) => {
                keys.exact.extend(attachment_identity_keys(attachment));
            }
        }
        keys.stems.extend(entity_stem_keys(image));
    }
    keys.exact.extend(fresh_target_keys.iter().cloned());
    keys
}

/// Identity-relevance test (D4): if neither existence nor any of
/// `{path, path_key, stem_key, alias set, stable_id, stable_id_key}`
/// changed, no *other* note's link resolution can move, and the
/// `K`-derived pass is skipped. Freshly re-inserted own outgoing links are
/// always resolved regardless (their keys join the set unconditionally).
#[must_use]
pub fn identity_relevant(pre: Option<&EntityPre>, post: Option<&EntityPre>) -> bool {
    match (pre, post) {
        (None, None) => false,
        (Some(EntityPre::Note(pre)), Some(EntityPre::Note(post))) => {
            pre.id != post.id
                || pre.path != post.path
                || pre.path_key != post.path_key
                || pre.stem_key != post.stem_key
                || pre.aliases != post.aliases
                || pre.stable_id != post.stable_id
                || pre.stable_id_key != post.stable_id_key
        }
        (Some(EntityPre::Attachment(pre)), Some(EntityPre::Attachment(post))) => {
            pre.id != post.id
                || pre.path != post.path
                || pre.path_key != post.path_key
                || pre.stem_key != post.stem_key
        }
        // Existence flipped — or (defensively) the kind did, which no
        // mutation path can do on one native path.
        _ => true,
    }
}

/// Per-transaction overlay over the persistent base catalog (§6.2.4).
/// Lookup order is overlay first, then base; the overlay is built from
/// exactly the rows this transaction wrote (including removals). On commit
/// it folds into the base; on rollback it is dropped — SQLite remains the
/// sole truth, and rollback safety is structural (drop, not invert).
#[derive(Debug, Default)]
pub struct ResolverOverlay {
    /// Note ids removed or replaced in this transaction. The merged view
    /// hides their base contributions entirely.
    pub(crate) shadowed_notes: BTreeSet<i64>,
    pub(crate) shadowed_attachments: BTreeSet<i64>,
    /// Sparse catalog of the post-mutation images written by this
    /// transaction. Indexing rules are shared with the base by construction.
    pub(crate) current: ResolverCatalog,
    /// Authoritative group winners for every touched `stable_id_key`,
    /// recomputed from the transaction after the row mutations. `None`
    /// means the group lost its winner (or emptied).
    pub(crate) stable_winners: BTreeMap<String, Option<i64>>,
}

impl ResolverOverlay {
    #[must_use]
    pub fn new(epoch: u64, generation: u64) -> Self {
        Self {
            current: ResolverCatalog::empty(epoch, generation),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shadowed_notes.is_empty()
            && self.shadowed_attachments.is_empty()
            && self.stable_winners.is_empty()
    }

    pub fn upsert_note(&mut self, image: &NoteImage) {
        self.shadowed_notes.insert(image.id);
        self.current.upsert_note(image);
    }

    pub fn upsert_attachment(&mut self, image: &AttImage) {
        self.shadowed_attachments.insert(image.id);
        self.current.upsert_attachment(image);
    }

    pub fn remove_note(&mut self, id: i64) {
        self.shadowed_notes.insert(id);
        self.current.remove_note(id);
    }

    pub fn remove_attachment(&mut self, id: i64) {
        self.shadowed_attachments.insert(id);
        self.current.remove_attachment(id);
    }

    pub fn set_stable_winner(&mut self, stable_id_key: &str, winner: Option<i64>) {
        self.stable_winners.insert(stable_id_key.to_owned(), winner);
    }

    /// Resolve a stored link through the merged overlay-then-base view.
    #[must_use]
    pub fn resolve(
        &self,
        base: &ResolverCatalog,
        target_key: &str,
        raw: &str,
        embed: bool,
        source: Option<i64>,
    ) -> Resolution {
        base.resolve_with_overlay(Some(self), target_key, raw, embed, source)
    }

    /// Resolve an open, unindexed source's link through the merged view.
    #[must_use]
    pub fn resolve_at_path(
        &self,
        base: &ResolverCatalog,
        target_key: &str,
        raw: &str,
        embed: bool,
        source: Option<&Path>,
    ) -> Resolution {
        base.resolve_at_path_with_overlay(Some(self), target_key, raw, embed, source)
    }
}

/// The persistent resolver base plus the per-transaction overlay and the
/// backstop bookkeeping (§6.2.1). Lives on the index writer owner.
#[derive(Debug)]
pub struct ResolverState {
    base: ResolverCatalog,
    overlay: ResolverOverlay,
    generations_since_full: u64,
    backstop_interval: u64,
    /// Explicit full-verify forcing (S3): the next apply runs the
    /// whole-vault walk regardless of the generation cadence.
    backstop_forced: bool,
    resolve_full_total: u64,
    resolve_targeted_total: u64,
    affected_links_last: u64,
    id_conflict_group_updates_total: u64,
    apply_generations_total: u64,
    changes_rows_total: u64,
}

/// Counter surface for doctor diagnostics (§6.7; the vault actor drains
/// these to meta, `change-log-health`/`resolver-parity` surface them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolverDiagnostics {
    pub generations_since_full: u64,
    pub resolve_full_total: u64,
    pub resolve_targeted_total: u64,
    pub affected_links_last: u64,
    pub id_conflict_group_updates_total: u64,
    pub apply_generations_total: u64,
    pub changes_rows_total: u64,
}

impl ResolverState {
    /// Fresh persistent base from a consistent read of the writer.
    ///
    /// # Errors
    /// Returns a SQLite error when the catalog queries fail.
    pub fn load(conn: &Connection) -> Result<Self> {
        Ok(Self {
            base: ResolverCatalog::load(conn)?,
            overlay: ResolverOverlay::default(),
            generations_since_full: 0,
            backstop_interval: RESOLVER_BACKSTOP_GENERATIONS,
            backstop_forced: false,
            resolve_full_total: 0,
            resolve_targeted_total: 0,
            affected_links_last: 0,
            id_conflict_group_updates_total: 0,
            apply_generations_total: 0,
            changes_rows_total: 0,
        })
    }

    /// Measurement seam: a state whose backstop fires on a custom
    /// generation cadence (bench comparison; lowered-const unit tests).
    ///
    /// # Errors
    /// Returns a SQLite error when the catalog queries fail.
    pub fn with_backstop_interval(conn: &Connection, generations: u64) -> Result<Self> {
        Ok(Self {
            backstop_interval: generations.max(1),
            ..Self::load(conn)?
        })
    }

    #[must_use]
    pub fn diagnostics(&self) -> ResolverDiagnostics {
        ResolverDiagnostics {
            generations_since_full: self.generations_since_full,
            resolve_full_total: self.resolve_full_total,
            resolve_targeted_total: self.resolve_targeted_total,
            affected_links_last: self.affected_links_last,
            id_conflict_group_updates_total: self.id_conflict_group_updates_total,
            apply_generations_total: self.apply_generations_total,
            changes_rows_total: self.changes_rows_total,
        }
    }

    #[must_use]
    pub const fn base(&self) -> &ResolverCatalog {
        &self.base
    }

    #[must_use]
    pub const fn overlay(&self) -> &ResolverOverlay {
        &self.overlay
    }

    /// Stage this transaction's overlay before the resolution pass.
    pub fn overlay_mut(&mut self) -> &mut ResolverOverlay {
        &mut self.overlay
    }

    /// Whether this apply must run the whole-vault backstop instead of the
    /// targeted pass (D13): the generation cadence, or an explicit forcing
    /// (full-verify reconcile jobs — S3; graph-mirror parity failure — S6).
    #[must_use]
    pub const fn backstop_due(&self) -> bool {
        self.backstop_forced || self.generations_since_full >= self.backstop_interval
    }

    /// Force the next apply to run the whole-vault backstop walk. Consumed
    /// when the forced walk commits; a rolled-back apply leaves the demand
    /// standing so the verification still lands.
    pub fn force_backstop(&mut self) {
        self.backstop_forced = true;
    }

    /// Install a completed backstop walk at commit and reset the counter.
    /// The walk itself runs in the apply spine (pool.rs) so the whole-vault
    /// call site stays the single grep-gated `re_resolve_all` invocation.
    pub fn commit_backstop(&mut self, catalog: ResolverCatalog) {
        self.base = catalog;
        self.generations_since_full = 0;
        self.backstop_forced = false;
        self.resolve_full_total += 1;
    }

    /// Record a committed structural apply and its change-log row volume
    /// (§6.7: `apply_generations_total`, `changes_rows_total`).
    pub fn note_committed_apply(&mut self, change_rows: u64) {
        self.apply_generations_total += 1;
        self.changes_rows_total += change_rows;
    }

    /// Record a committed targeted pass; `visited` counts candidate rows read.
    pub fn note_targeted_pass(&mut self, visited: u64) {
        self.resolve_targeted_total += 1;
        self.affected_links_last = visited;
    }

    /// Fold the current transaction's overlay into the base at commit and
    /// advance the backstop counter (§6.2.4). Never call on rollback: the
    /// overlay is simply dropped instead.
    pub fn fold_overlay(&mut self, epoch: u64, generation: u64) {
        let overlay = std::mem::take(&mut self.overlay);
        if !overlay.is_empty() {
            self.base.fold_overlay(&overlay);
            self.base.generation = generation;
            self.base.epoch = epoch;
        }
        self.generations_since_full += 1;
    }

    /// Record `id_conflict` group recomputes executed in one apply.
    pub fn note_group_updates(&mut self, groups: u64) {
        self.id_conflict_group_updates_total += groups;
    }
}

/// Runs inside the writer transaction AFTER entity rows are mutated and
/// outgoing links re-inserted (`dst NULL`). Returns the sources whose link
/// rows changed (feeds `ChangeLogWriter`, D5) and the number of candidate
/// rows visited (counter plumbing). Candidates are fetched through two
/// indexed arms (schema 3.1): `target_key IN (…)` (`links_target`) for
/// every exact spelling, and `target_stem IN (…)` (`links_target_stem`)
/// for every directory/extension spelling that resolves through the
/// catalog's stem-fallback branch — together a sound superset (FR-3).
/// IN lists are chunked at 900 keys; each candidate resolves through the
/// overlay-then-base merge, and a row is written only when the projected
/// state differs from the stored one (redundant re-resolution is a no-op,
/// never a spurious `outgoing` row). State is read-only here: the caller
/// books counters at commit so a rolled-back apply cannot drift them.
///
/// # Errors
/// Returns the error text when a candidate fetch or batched update fails.
pub fn resolve_affected(
    tx: &Transaction<'_>,
    state: &ResolverState,
    affected_keys: &AffectedKeys,
) -> Result<(Vec<i64>, u64), String> {
    if affected_keys.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let base = &state.base;
    let overlay = &state.overlay;
    let mut changed: BTreeSet<i64> = BTreeSet::new();
    let mut visited: u64 = 0;
    let mut update = tx
        .prepare("UPDATE links SET dst_note=?1, dst_att=?2, res_state=?3 WHERE src=?4 AND ord=?5")
        .map_err(|error| error.to_string())?;
    for (column, keys) in [
        ("target_key", &affected_keys.exact),
        ("target_stem", &affected_keys.stems),
    ] {
        let keys: Vec<String> = keys.iter().cloned().collect();
        for chunk in keys.chunks(AFFECTED_KEY_CHUNK) {
            let sql = format!(
                "SELECT src, ord, kind, dst_raw, target_key, dst_note, dst_att, res_state \
                 FROM links WHERE {column} IN ({})",
                (1..=chunk.len())
                    .map(|index| format!("?{index}"))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let rows = {
                let mut statement = tx.prepare(&sql).map_err(|error| error.to_string())?;
                statement
                    .query_map(
                        rusqlite::params_from_iter(chunk.iter().map(String::as_str)),
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, i32>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, Option<i64>>(5)?,
                                row.get::<_, Option<i64>>(6)?,
                                row.get::<_, String>(7)?,
                            ))
                        },
                    )
                    .map_err(|error| error.to_string())?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.to_string())?
            };
            for (src, ord, kind, dst_raw, target_key, dst_note, dst_att, res_state) in rows {
                visited += 1;
                let resolution = base.resolve_with_overlay(
                    Some(overlay),
                    &target_key,
                    &dst_raw,
                    kind == "embed",
                    Some(src),
                );
                let projected = projected_resolution(&resolution);
                if dst_note != projected.0 || dst_att != projected.1 || res_state != projected.2 {
                    update
                        .execute(params![projected.0, projected.1, projected.2, src, ord])
                        .map_err(|error| error.to_string())?;
                    changed.insert(src);
                }
            }
        }
    }
    Ok((changed.into_iter().collect(), visited))
}

/// `(dst_note, dst_att, res_state)` as stored for one `Resolution` —
/// shared by the targeted pass, the backstop walk, and
/// `update_link_resolution` so comparison and write can never drift apart.
#[must_use]
pub fn projected_resolution(resolution: &Resolution) -> (Option<i64>, Option<i64>, &'static str) {
    match resolution.state {
        ResState::Resolved => {
            if let Some(note) = &resolution.note {
                (Some(note.note_id), None, ResState::Resolved.as_str())
            } else if let Some(attachment) = &resolution.attachment {
                (None, Some(attachment.att_id), ResState::Resolved.as_str())
            } else {
                (None, None, ResState::Unresolved.as_str())
            }
        }
        ResState::Ambiguous => (
            resolution.note.as_ref().map(|note| note.note_id),
            resolution.attachment.as_ref().map(|att| att.att_id),
            ResState::Ambiguous.as_str(),
        ),
        ResState::Unresolved => (None, None, ResState::Unresolved.as_str()),
    }
}

/// One stored link row with its resolution state, shared by the backstop
/// walk and the read-only parity preview so the two compare exactly the
/// same projection of the same rows.
struct StoredLink {
    src: i64,
    ord: i32,
    dst_raw: String,
    target_key: String,
    embed: bool,
    dst_note: Option<i64>,
    dst_att: Option<i64>,
    res_state: String,
}

impl StoredLink {
    fn stored(&self) -> (Option<i64>, Option<i64>, String) {
        (self.dst_note, self.dst_att, self.res_state.clone())
    }
}

fn fetch_all_links(conn: &Connection) -> Result<Vec<StoredLink>, String> {
    let mut statement = conn
        .prepare(
            "SELECT src, ord, dst_raw, target_key, kind, dst_note, dst_att, res_state FROM links",
        )
        .map_err(|error| error.to_string())?;
    statement
        .query_map([], |row| {
            Ok(StoredLink {
                src: row.get(0)?,
                ord: row.get(1)?,
                dst_raw: row.get(2)?,
                target_key: row.get(3)?,
                embed: row.get::<_, String>(4)? == "embed",
                dst_note: row.get(5)?,
                dst_att: row.get(6)?,
                res_state: row.get(7)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

fn project_link(
    catalog: &ResolverCatalog,
    row: &StoredLink,
) -> (Option<i64>, Option<i64>, &'static str) {
    let resolution = catalog.resolve(&row.target_key, &row.dst_raw, row.embed, Some(row.src));
    projected_resolution(&resolution)
}

/// One link row whose stored resolution differs from the from-scratch
/// projection — the unit of the `resolver-parity` doctor finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkParityMismatch {
    pub src: i64,
    pub ord: i32,
    pub target_key: String,
    /// Stored `(dst_note, dst_att, res_state)`.
    pub stored: (Option<i64>, Option<i64>, String),
    /// Projected `(dst_note, dst_att, res_state)` from a fresh full walk.
    pub projected: (Option<i64>, Option<i64>, String),
}

/// Read-only parity recomputation (§6.2.5, doctor `resolver-parity`): the
/// from-scratch `re_resolve_all` projection over every stored link,
/// collected as mismatches without writing anything. A non-empty result
/// means the incremental state drifted from the full-walk truth (FR-2) —
/// the fix is an explicit full verify, never an implicit doctor repair.
///
/// # Errors
/// Returns the error text when the catalog or link rows cannot be read.
pub fn re_resolve_preview(conn: &Connection) -> Result<Vec<LinkParityMismatch>, String> {
    let catalog = ResolverCatalog::load(conn).map_err(|error| error.to_string())?;
    let rows = fetch_all_links(conn)?;
    let mut mismatches = Vec::new();
    for row in rows {
        let projected = project_link(&catalog, &row);
        let stored = row.stored();
        if stored != (projected.0, projected.1, projected.2.to_owned()) {
            mismatches.push(LinkParityMismatch {
                src: row.src,
                ord: row.ord,
                target_key: row.target_key,
                stored,
                projected: (projected.0, projected.1, projected.2.to_owned()),
            });
        }
    }
    Ok(mismatches)
}

/// Targeted `id_conflict` maintenance (§6.2.6): recompute the flag only for
/// the groups whose membership can have moved — the changed entity's
/// pre/post `stable_id_key`s — plus the applied row itself (it may have
/// left its group with a stale flag). Same CASE semantics as the retired
/// full-table statements, scoped by `WHERE stable_id_key`. Returns the
/// executed statement count plus the note ids whose stored `id_conflict`
/// value actually flipped — the group UPDATE rewrites *other* members'
/// rows too, and those flags are graph-visible entity state
/// (`GraphNodeRecord.id_conflict` reads them), so the change feed MUST
/// describe each flipped member (FR-1) or derived views drift (the S6
/// mirror-equivalence gate is what makes this omission impossible).
///
/// # Errors
/// Returns the error text when an update fails.

pub fn refresh_id_conflict_groups(
    tx: &Transaction<'_>,
    stable_id_keys: &BTreeSet<String>,
    applied_note: Option<i64>,
) -> Result<(u64, BTreeSet<i64>), String> {
    let mut executed: u64 = 0;
    let mut flipped: BTreeSet<i64> = BTreeSet::new();
    {
        let mut statement = tx
            .prepare(
                "UPDATE notes SET id_conflict = \
                 CASE WHEN stable_id_key IS NOT NULL \
                      AND path_key <> (SELECT MIN(n2.path_key) FROM notes n2 \
                                       WHERE n2.stable_id_key = notes.stable_id_key) \
                 THEN 1 ELSE 0 END \
                 WHERE stable_id_key = ?1",
            )
            .map_err(|error| error.to_string())?;
        let mut group_select = tx
            .prepare(
                "SELECT note_id, id_conflict FROM notes WHERE stable_id_key = ?1 ORDER BY note_id",
            )
            .map_err(|error| error.to_string())?;
        let capture = |group_select: &mut rusqlite::Statement<'_>,
                       key: &str|
         -> Result<Vec<(i64, i64)>, String> {
            group_select
                .query_map(params![key], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())
        };
        for key in stable_id_keys {
            let before = capture(&mut group_select, key)?;
            statement
                .execute(params![key])
                .map_err(|error| error.to_string())?;
            executed += 1;
            let after = capture(&mut group_select, key)?;
            for (id, flag) in after {
                if before
                    .iter()
                    .any(|(other, other_flag)| *other == id && *other_flag != flag)
                {
                    flipped.insert(id);
                }
            }
        }
    }
    if let Some(note_id) = applied_note {
        let before = tx
            .query_row(
                "SELECT id_conflict FROM notes WHERE note_id = ?1",
                params![note_id],
                |row| row.get::<_, i64>(0),
            )
            .ok();
        tx.execute(
            "UPDATE notes SET id_conflict = \
             CASE WHEN stable_id_key IS NOT NULL \
                  AND path_key <> (SELECT MIN(n2.path_key) FROM notes n2 \
                                   WHERE n2.stable_id_key = notes.stable_id_key) \
             THEN 1 ELSE 0 END \
             WHERE note_id = ?1",
            params![note_id],
        )
        .map_err(|error| error.to_string())?;
        executed += 1;
        let after = tx
            .query_row(
                "SELECT id_conflict FROM notes WHERE note_id = ?1",
                params![note_id],
                |row| row.get::<_, i64>(0),
            )
            .ok();
        if let (Some(before), Some(after)) = (before, after)
            && before != after
        {
            flipped.insert(note_id);
        }
    }
    Ok((executed, flipped))
}

/// Winner query for one touched `stable_id_key`, run on the writer
/// transaction after the row mutations: the same first-wins rule the full
/// catalog load applies (`ORDER BY path_key, note_id`).
///
/// # Errors
/// Returns the error text when the query fails.
pub fn group_winner_in_tx(
    tx: &Transaction<'_>,
    stable_id_key: &str,
) -> Result<Option<i64>, String> {
    tx.query_row(
        "SELECT note_id FROM notes WHERE stable_id_key = ?1 ORDER BY path_key, note_id LIMIT 1",
        params![stable_id_key],
        |row| row.get(0),
    )
    .optional()
    .map_err(|error| error.to_string())
}

/// The link resolver.
pub struct Resolver<'a> {
    conn: &'a Connection,
    catalog: Option<crate::catalog::ResolverCatalog>,
}

impl<'a> Resolver<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self {
            conn,
            catalog: None,
        }
    }

    /// Initialize or refresh the cache from the database.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when the backing queries cannot be prepared
    /// or their rows cannot be read.
    pub fn refresh_cache(&mut self) -> Result<()> {
        self.catalog = Some(crate::catalog::ResolverCatalog::load(self.conn)?);
        Ok(())
    }

    /// Resolve a new unsaved source with the same directory-ranking rule.
    #[must_use]
    pub fn resolve_at_path(
        &self,
        target_key: &str,
        raw: &str,
        embed: bool,
        source: &Path,
    ) -> Resolution {
        self.catalog
            .as_ref()
            .map_or_else(Resolution::unresolved, |catalog| {
                catalog.resolve_at_path(target_key, raw, embed, Some(source))
            })
    }

    /// Resolve a link target using the canonical D8 precedence and ranking.
    #[must_use]
    pub fn resolve(
        &self,
        target_key: &str,
        raw_target: &str,
        is_embed: bool,
        source_note_id: Option<i64>,
    ) -> Resolution {
        self.catalog
            .as_ref()
            .map_or_else(Resolution::unresolved, |catalog| {
                catalog.resolve(target_key, raw_target, is_embed, source_note_id)
            })
    }

    /// Invalidate canonical note candidates after an index change.
    ///
    /// # Errors
    /// Retains the existing fallible API; the immutable in-memory removal cannot fail.
    pub fn invalidate_for_note(&mut self, note_id: i64) -> Result<()> {
        if let Some(catalog) = &mut self.catalog {
            catalog.remove_note(note_id);
        }
        Ok(())
    }

    /// Invalidate canonical attachment candidates after an index change.
    ///
    /// # Errors
    /// Retains the existing fallible API; the immutable in-memory removal cannot fail.
    pub fn invalidate_for_attachment(&mut self, att_id: i64) -> Result<()> {
        if let Some(catalog) = &mut self.catalog {
            catalog.remove_attachment(att_id);
        }
        Ok(())
    }

    /// Update the resolution state for a link in the database.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when the update cannot be executed.
    pub fn update_link_resolution(
        &self,
        src: i64,
        ord: i32,
        resolution: &Resolution,
    ) -> Result<()> {
        let (dst_note, dst_att, res_state) = projected_resolution(resolution);
        self.conn.execute(
            "UPDATE links SET dst_note = ?, dst_att = ?, res_state = ? WHERE src = ? AND ord = ?",
            params![dst_note, dst_att, res_state, src, ord,],
        )?;

        Ok(())
    }

    /// Re-resolve every link in the database. Backstop-only (D13, §6.2.5):
    /// refreshes the whole catalog, then writes each link row only when the
    /// projected state differs from the stored state, returning the sources
    /// whose link rows flipped (feeds the change log, D5). Apply paths MUST
    /// NOT call this; they route through `resolve_affected` instead, and a
    /// grep gate enforces the rule.
    ///
    /// # Errors
    ///
    /// Returns the error text when the cache refresh or any link
    /// re-resolution query fails.
    pub fn re_resolve_all(&mut self) -> Result<Vec<i64>, String> {
        self.refresh_cache().map_err(|error| error.to_string())?;
        let catalog = self
            .catalog
            .as_ref()
            .ok_or("resolver catalog missing after refresh")?;
        let rows = fetch_all_links(self.conn)?;
        let mut changed: BTreeSet<i64> = BTreeSet::new();
        let mut update = self
            .conn
            .prepare(
                "UPDATE links SET dst_note=?1, dst_att=?2, res_state=?3 WHERE src=?4 AND ord=?5",
            )
            .map_err(|error| error.to_string())?;
        for row in rows {
            let projected = project_link(catalog, &row);
            let stored = row.stored();
            if stored.0 != projected.0 || stored.1 != projected.1 || stored.2 != projected.2 {
                update
                    .execute(params![
                        projected.0,
                        projected.1,
                        projected.2,
                        row.src,
                        row.ord
                    ])
                    .map_err(|error| error.to_string())?;
                changed.insert(row.src);
            }
        }
        Ok(changed.into_iter().collect())
    }

    /// Hand the refreshed catalog to the persistent base (backstop fold-in).
    #[must_use]
    pub fn take_catalog(&mut self) -> Option<ResolverCatalog> {
        self.catalog.take()
    }
}

/// Extract the stem from a path (filename without extension).
#[must_use]
pub fn extract_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map_or_else(|| path.to_string(), |s| s.to_string_lossy().to_string())
}

/// Check if a file extension is an attachment-like extension.
#[must_use]
pub fn is_attachment_extension(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);

    matches!(
        ext.as_deref(),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "svg"
                | "avif"
                | "bmp"
                | "ico"
                | "pdf"
                | "mp3"
                | "wav"
                | "ogg"
                | "flac"
                | "m4a"
                | "mp4"
                | "webm"
                | "mov"
                | "mkv"
                | "zip"
                | "tar"
                | "gz"
                | "tgz"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::create_schema;
    use tempfile::NamedTempFile;

    #[test]
    fn test_basic_resolution() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        // Insert test note
        conn.execute(
            "INSERT INTO notes (note_id, path, path_native, path_key, stem_key, title, content_hash, size, mtime_ns, scan_ns, generation) 
             VALUES (1, 'note1.md', X'6E6F7465312E6D64', 'note1.md', 'note1', 'Note 1', X'00', 100, 0, 0, 0)",
            [],
        ).unwrap();

        // Insert alias
        conn.execute(
            "INSERT INTO aliases (note_id, alias, alias_key) VALUES (1, 'my-note', 'my-note')",
            [],
        )
        .unwrap();

        let mut resolver = Resolver::new(&conn);
        resolver.refresh_cache().unwrap();

        // Test stem resolution
        let res = resolver.resolve("note1", "note1", false, None);
        assert!(res.note.is_some());
        assert_eq!(res.note.unwrap().note_id, 1);

        // Test alias resolution
        let res = resolver.resolve("my-note", "my-note", false, None);
        assert!(res.note.is_some());
        assert_eq!(res.note.unwrap().note_id, 1);
    }

    #[test]
    fn test_unresolved() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        let mut resolver = Resolver::new(&conn);
        resolver.refresh_cache().unwrap();

        let res = resolver.resolve("nonexistent", "nonexistent", false, None);
        assert!(res.is_unresolved);
        assert!(res.note.is_none());
        assert!(res.attachment.is_none());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn resolver_handles_paths_ids_embeds_ambiguity_and_invalidation() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open(temp.path()).unwrap();
        create_schema(&mut conn).unwrap();
        for (id, path, native, stem, stable) in [
            (
                1,
                "notes/one.md",
                b"notes/one.md".as_slice(),
                "one",
                "shared",
            ),
            (
                2,
                "notes/two.md",
                b"notes/two.md".as_slice(),
                "two",
                "shared",
            ),
            (
                3,
                "notes/cover.md",
                b"notes/cover.md".as_slice(),
                "cover",
                "cover-id",
            ),
        ] {
            conn.execute(
                "INSERT INTO notes (note_id,path,path_native,path_key,stem_key,title,stable_id,stable_id_key,content_hash,size,mtime_ns,scan_ns,generation) VALUES (?1,?2,?3,?2,?4,'',?5,?5,X'00',0,0,0,0)",
                params![id, path, native, stem, stable],
            ).unwrap();
        }
        conn.execute(
            "INSERT INTO aliases (note_id,alias,alias_key) VALUES (1,'first','alias')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attachments (att_id,path,path_native,path_key,stem_key,kind,size,mtime_ns,scan_ns,generation) VALUES (5,'images/cover.png',X'696d61676573','images/cover.png','cover','image',1,0,0,0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO links (src,ord,kind,dst_raw,target_key,by_id,span_start,span_end,res_state) VALUES (1,0,'link','two','two',0,0,0,'unresolved')",
            [],
        ).unwrap();

        let mut resolver = Resolver::new(&conn);
        resolver.refresh_cache().unwrap();
        assert_eq!(
            resolver
                .resolve("cover-id", "id:cover-id", false, None)
                .note
                .unwrap()
                .note_id,
            3
        );
        let duplicate_id = resolver.resolve("shared", "id:shared", false, None);
        assert_eq!(duplicate_id.note.unwrap().note_id, 1);
        assert_eq!(duplicate_id.state, ResState::Resolved);
        assert!(!duplicate_id.is_ambiguous);
        assert_eq!(
            resolver
                .resolve("notes/one.md", "notes/one.md", false, None)
                .note
                .unwrap()
                .note_id,
            1
        );
        assert_eq!(
            resolver
                .resolve("images/cover.png", "images/cover.png", true, None)
                .attachment
                .unwrap()
                .att_id,
            5
        );
        assert_eq!(
            resolver
                .resolve("alias", "first", false, None)
                .note
                .unwrap()
                .note_id,
            1
        );
        let ambiguous = resolver.resolve("cover", "cover", true, None);
        assert!(!ambiguous.is_ambiguous);
        assert_eq!(ambiguous.attachment.unwrap().att_id, 5);

        resolver.re_resolve_all().unwrap();
        let state: String = resolver
            .conn
            .query_row(
                "SELECT res_state FROM links WHERE src=1 AND ord=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "resolved");
        resolver.invalidate_for_note(1).unwrap();
        resolver.invalidate_for_attachment(5).unwrap();
        assert!(
            resolver
                .resolve("alias", "first", false, None)
                .is_unresolved
        );
        resolver.refresh_cache().unwrap();
        let resolution = Resolution::ambiguous();
        resolver.update_link_resolution(1, 0, &resolution).unwrap();
        let state: String = resolver
            .conn
            .query_row(
                "SELECT res_state FROM links WHERE src=1 AND ord=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "ambiguous");
        resolver
            .update_link_resolution(1, 0, &Resolution::unresolved())
            .unwrap();
        resolver
            .update_link_resolution(
                1,
                0,
                &Resolution {
                    note: None,
                    attachment: None,
                    is_ambiguous: false,
                    is_unresolved: false,
                    state: ResState::Resolved,
                },
            )
            .unwrap();
    }

    #[test]
    fn test_attachment_extension() {
        assert!(is_attachment_extension("image.png"));
        assert!(is_attachment_extension("doc.pdf"));
        assert!(!is_attachment_extension("note.md"));
    }

    #[test]
    fn test_extract_stem() {
        assert_eq!(extract_stem("note.md"), "note");
        assert_eq!(extract_stem("my-note.md"), "my-note");
        assert_eq!(extract_stem("path/to/note.md"), "note");
    }
}
