//! Complete Boolean AST evaluation over one captured snapshot
//! (RFC-0006 S06, §6.3, D-07).
//!
//! The evaluator normalizes the query to negation-normal form (NNF) by De
//! Morgan rules without distributing AND over OR, then evaluates the
//! complete supported AST: each text leaf produces a note-ID/score set from
//! the pinned captured searcher; structural leaves produce ID sets from the
//! captured SQL snapshot. Membership uses bitsets indexed by compact
//! snapshot-local ordinals (≤50k), never a giant SQL `IN` and never a
//! top-hit approximation. Each retained ID carries a 64-bit
//! contributing-leaf mask; AND and OR union masks only from satisfied child
//! branches; negative leaves contribute mask zero. At the root each set
//! bit's leaf score is summed once, so a repeated Text across two satisfied
//! OR branches counts once by definition.

use crate::query::Query;
use crate::search::CapturedSearcher;
use rusqlite::Connection;
use std::collections::HashMap;
use thiserror::Error;

/// Maximum authorized note universe for one snapshot (D-08 supported design
/// corpus bound for the bitset membership model).
pub const MAX_UNIVERSE_NOTES: usize = 50_000;

/// NNF node bound after normalization (§6.3: at most 512 nodes after a
/// ≤256-node input; no CNF expansion).
pub const MAX_NNF_NODES: usize = 512;

/// Typed evaluation failure. Every resource limit is a typed error, never a
/// truncated "complete" set (D-07/D-08).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EvalError {
    #[error("evaluation limit exceeded: {kind} observed {observed} of {limit}")]
    LimitExceeded {
        kind: &'static str,
        observed: usize,
        limit: usize,
    },
    #[error("attachment predicates are not members of note search")]
    AttachmentNotSearchable,
    #[error("non-finite relevance score")]
    NonFiniteScore,
    #[error("SQL evaluation error: {0}")]
    Sql(String),
    #[error("search evaluation error: {0}")]
    Search(String),
}

/// The authorized note universe of one snapshot: every indexed note, in
/// canonical native-path order, with compact ordinals.
#[derive(Debug, Clone)]
pub struct Universe {
    /// Note IDs in canonical order (`path_native` asc, `note_id` asc).
    pub ids: Vec<i64>,
    /// Native path bytes per ordinal.
    pub paths: Vec<Vec<u8>>,
    ordinals: HashMap<i64, u32>,
}

impl Universe {
    /// Read the universe from one captured SQL read transaction.
    ///
    /// # Errors
    /// Returns a typed limit error above [`MAX_UNIVERSE_NOTES`] and SQL
    /// errors verbatim.
    pub fn from_tx(tx: &Connection) -> Result<Self, EvalError> {
        let mut stmt = tx
            .prepare("SELECT note_id, path_native FROM notes ORDER BY path_native, note_id")
            .map_err(|error| EvalError::Sql(error.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|error| EvalError::Sql(error.to_string()))?;
        let mut ids = Vec::new();
        let mut paths = Vec::new();
        for row in rows {
            let (id, path) = row.map_err(|error| EvalError::Sql(error.to_string()))?;
            ids.push(id);
            paths.push(path);
            if ids.len() > MAX_UNIVERSE_NOTES {
                return Err(EvalError::LimitExceeded {
                    kind: "snapshot_universe",
                    observed: ids.len(),
                    limit: MAX_UNIVERSE_NOTES,
                });
            }
        }
        let ordinals = ids
            .iter()
            .enumerate()
            .map(|(ordinal, id)| (*id, u32::try_from(ordinal).unwrap_or(u32::MAX)))
            .collect();
        Ok(Self {
            ids,
            paths,
            ordinals,
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    #[must_use]
    pub fn ordinal(&self, note_id: i64) -> Option<u32> {
        self.ordinals.get(&note_id).copied()
    }
}

/// Bitset indexed by snapshot-local ordinal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitset {
    words: Vec<u64>,
}

impl Bitset {
    #[must_use]
    pub fn empty(universe_len: usize) -> Self {
        Self {
            words: vec![0; universe_len.div_ceil(64)],
        }
    }

    #[must_use]
    pub fn full(universe_len: usize) -> Self {
        let mut words = vec![u64::MAX; universe_len.div_ceil(64)];
        let remainder = universe_len % 64;
        if remainder != 0
            && let Some(last) = words.last_mut()
        {
            *last = (1 << remainder) - 1;
        }
        Self { words }
    }

    pub fn set(&mut self, ordinal: u32) {
        let ordinal = usize::try_from(ordinal).unwrap_or(usize::MAX);
        if let Some(word) = self.words.get_mut(ordinal / 64) {
            *word |= 1 << (ordinal % 64);
        }
    }

    #[must_use]
    pub fn get(&self, ordinal: u32) -> bool {
        let ordinal = usize::try_from(ordinal).unwrap_or(usize::MAX);
        self.words
            .get(ordinal / 64)
            .is_some_and(|word| word >> (ordinal % 64) & 1 == 1)
    }

    #[must_use]
    pub fn intersects(&self, other: &Self) -> bool {
        self.words
            .iter()
            .zip(other.words.iter())
            .any(|(a, b)| a & b != 0)
    }

    /// Set ordinals in ascending order.
    pub fn iter_ordinals(&self) -> impl Iterator<Item = u32> + '_ {
        self.words
            .iter()
            .enumerate()
            .flat_map(move |(index, word)| {
                let base = u32::try_from(index * 64).unwrap_or(u32::MAX);
                (0..64)
                    .filter(move |bit| word >> bit & 1 == 1)
                    .map(move |bit| base + u32::try_from(bit).unwrap_or(0))
            })
    }

    fn intersect(&self, other: &Self) -> Self {
        Self {
            words: self
                .words
                .iter()
                .zip(other.words.iter())
                .map(|(a, b)| a & b)
                .collect(),
        }
    }

    fn union(&self, other: &Self) -> Self {
        Self {
            words: self
                .words
                .iter()
                .zip(other.words.iter())
                .map(|(a, b)| a | b)
                .collect(),
        }
    }

    fn complement(&self, universe_len: usize) -> Self {
        Bitset::full(universe_len).intersect(&Self {
            words: self.words.iter().map(|word| !word).collect(),
        })
    }
}

/// Negation-normal form over deduplicated leaves. `Not` only wraps leaves;
/// double negation normalizes to a positive leaf (§6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nnf {
    /// Matches the whole authorized universe (`AND` of zero branches).
    All,
    /// Matches nothing (`OR` of zero branches).
    None,
    Leaf(usize),
    /// A negative leaf: contributes membership complement, mask zero.
    Not(usize),
    And(Box<Nnf>, Box<Nnf>),
    Or(Box<Nnf>, Box<Nnf>),
}

impl Nnf {
    #[must_use]
    pub fn node_count(&self) -> usize {
        match self {
            Self::All | Self::None | Self::Leaf(_) | Self::Not(_) => 1,
            Self::And(a, b) | Self::Or(a, b) => 1 + a.node_count() + b.node_count(),
        }
    }
}

/// Normalize `query` to NNF, deduplicating identical leaves so they share
/// one evaluation and (for text) one contributing-mask bit.
///
/// # Errors
/// Returns [`EvalError::LimitExceeded`] when the NNF exceeds
/// [`MAX_NNF_NODES`], and [`EvalError::AttachmentNotSearchable`] for
/// attachment predicates (attachments are not members of note search).
pub fn to_nnf(query: &Query) -> Result<(Nnf, Vec<Query>), EvalError> {
    let mut leaves: Vec<Query> = Vec::new();
    let mut nodes = 0usize;
    let nnf = normalize(query, false, &mut leaves, &mut nodes)?;
    if leaves.len() > crate::query::MAX_QUERY_TEXT_LEAVES {
        return Err(EvalError::LimitExceeded {
            kind: "text_leaves",
            observed: leaves.len(),
            limit: crate::query::MAX_QUERY_TEXT_LEAVES,
        });
    }
    Ok((nnf, leaves))
}

fn normalize(
    query: &Query,
    negate: bool,
    leaves: &mut Vec<Query>,
    nodes: &mut usize,
) -> Result<Nnf, EvalError> {
    *nodes += 1;
    if *nodes > MAX_NNF_NODES {
        return Err(EvalError::LimitExceeded {
            kind: "nnf_nodes",
            observed: *nodes,
            limit: MAX_NNF_NODES,
        });
    }
    match query {
        Query::And(parts) => {
            // Positive: AND-fold; negated: De Morgan to OR-fold.
            let mut acc: Option<Nnf> = None;
            for part in parts {
                let child = normalize(part, negate, leaves, nodes)?;
                acc = Some(match acc {
                    None => child,
                    Some(previous) => {
                        if negate {
                            Nnf::Or(Box::new(previous), Box::new(child))
                        } else {
                            Nnf::And(Box::new(previous), Box::new(child))
                        }
                    }
                });
            }
            Ok(acc.unwrap_or(if negate { Nnf::None } else { Nnf::All }))
        }
        Query::Or(parts) => {
            let mut acc: Option<Nnf> = None;
            for part in parts {
                let child = normalize(part, negate, leaves, nodes)?;
                acc = Some(match acc {
                    None => child,
                    Some(previous) => {
                        if negate {
                            Nnf::And(Box::new(previous), Box::new(child))
                        } else {
                            Nnf::Or(Box::new(previous), Box::new(child))
                        }
                    }
                });
            }
            Ok(acc.unwrap_or(if negate { Nnf::All } else { Nnf::None }))
        }
        Query::Not(inner) => normalize(inner, !negate, leaves, nodes),
        Query::Attachment { .. } => Err(EvalError::AttachmentNotSearchable),
        leaf => Ok(leaf_node(negate, leaf_index(leaf, leaves))),
    }
}

fn leaf_node(negate: bool, index: usize) -> Nnf {
    if negate {
        Nnf::Not(index)
    } else {
        Nnf::Leaf(index)
    }
}

fn leaf_index(query: &Query, leaves: &mut Vec<Query>) -> usize {
    if let Some(index) = leaves.iter().position(|leaf| leaf == query) {
        return index;
    }
    leaves.push(query.clone());
    leaves.len() - 1
}

/// One evaluated row: note identity, canonical native path and the summed
/// score of distinct positive text leaves. Rows from a tree with no
/// positive text leaf carry `None` (structural-only, D11); zero-score rows
/// under a mixed tree keep `Some(0.0)` so ordering stays total (§6.3).
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredNote {
    pub note_id: i64,
    pub path_native: Vec<u8>,
    pub score: Option<f32>,
}

/// The captured snapshot an evaluation runs against: the authorized
/// universe, structural sets from the captured SQL transaction, and text
/// sets from the ordered writer's captured searcher.
pub struct Snapshot<'a> {
    pub universe: &'a Universe,
    pub searcher: &'a CapturedSearcher,
    pub tx: &'a Connection,
}

/// Evaluate the complete supported AST against one snapshot (§6.3).
///
/// # Errors
/// Returns typed evaluation errors; every limit is a typed error, never a
/// truncated complete set.
pub fn evaluate(query: &Query, snapshot: &Snapshot<'_>) -> Result<Vec<ScoredNote>, EvalError> {
    let (nnf, leaves) = to_nnf(query)?;
    let universe_len = snapshot.universe.len();
    // One 64-bit contributing-mask bit per distinct text leaf; identical
    // leaves share results by deduplication (§6.3).
    let mut leaf_mask_bits: HashMap<usize, u64> = HashMap::new();
    let mut next_bit = 0u64;
    for (index, leaf) in leaves.iter().enumerate() {
        if matches!(leaf, Query::Text(_)) {
            leaf_mask_bits.insert(index, 1 << next_bit);
            next_bit += 1;
            if next_bit >= 65 {
                return Err(EvalError::LimitExceeded {
                    kind: "text_leaves",
                    observed: usize::try_from(next_bit).unwrap_or(usize::MAX),
                    limit: 64,
                });
            }
        }
    }
    // Evaluate every leaf once; identical leaves share the result.
    let mut leaf_sets: Vec<Option<(Bitset, HashMap<u32, f32>)>> = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        leaf_sets.push(eval_leaf(leaf, snapshot)?);
    }
    // Mask-bit -> leaf score table for the root sum.
    let bit_scores: Vec<(u64, &HashMap<u32, f32>)> = leaves
        .iter()
        .enumerate()
        .filter_map(|(index, leaf)| {
            if let Query::Text(_) = leaf {
                let bit = leaf_mask_bits.get(&index)?;
                let scores = leaf_sets.get(index)?.as_ref()?;
                Some((*bit, &scores.1))
            } else {
                None
            }
        })
        .collect();
    let (root_set, root_masks) = fold_masks(&nnf, &leaf_sets, &leaf_mask_bits, universe_len);
    // Root scores: sum each set bit's leaf score once per ID. A repeated
    // Text across two satisfied OR branches shares one bit, so it counts
    // once by definition (§6.3). A tree with no positive text leaf carries
    // no relevance at all (structural-only, D11).
    let has_positive_text = !bit_scores.is_empty();
    let mut rows = Vec::new();
    for ordinal in root_set.iter_ordinals() {
        let mask = root_masks.get(&ordinal).copied().unwrap_or(0);
        let mut score = 0.0f32;
        for (bit, scores) in &bit_scores {
            if mask & bit != 0 {
                score += scores.get(&ordinal).copied().unwrap_or(0.0);
            }
        }
        if !score.is_finite() {
            return Err(EvalError::NonFiniteScore);
        }
        let score = has_positive_text.then_some(score);
        let index = usize::try_from(ordinal).unwrap_or(usize::MAX);
        let path_native = snapshot
            .universe
            .paths
            .get(index)
            .cloned()
            .unwrap_or_default();
        let note_id = snapshot
            .universe
            .ids
            .get(index)
            .copied()
            .unwrap_or_default();
        rows.push(ScoredNote {
            note_id,
            path_native,
            score,
        });
    }
    // Complete ordering: score descending, then canonical native PathKey
    // bytes ascending, then note ID (D-07). Negative-only/structural
    // queries all score zero, collapsing to native path order.
    rows.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path_native.cmp(&b.path_native))
            .then_with(|| a.note_id.cmp(&b.note_id))
    });
    Ok(rows)
}

fn fold_masks(
    nnf: &Nnf,
    leaf_sets: &[Option<(Bitset, HashMap<u32, f32>)>],
    leaf_mask_bits: &HashMap<usize, u64>,
    universe_len: usize,
) -> (Bitset, HashMap<u32, u64>) {
    match nnf {
        Nnf::All => (Bitset::full(universe_len), HashMap::new()),
        Nnf::None => (Bitset::empty(universe_len), HashMap::new()),
        Nnf::Leaf(index) => {
            let Some(Some((set, _))) = leaf_sets.get(*index) else {
                return (Bitset::empty(universe_len), HashMap::new());
            };
            let masks: HashMap<u32, u64> = if let Some(bit) = leaf_mask_bits.get(index) {
                set.iter_ordinals().map(|ord| (ord, *bit)).collect()
            } else {
                HashMap::new()
            };
            (set.clone(), masks)
        }
        Nnf::Not(index) => {
            let set = leaf_sets
                .get(*index)
                .and_then(|entry| entry.as_ref().map(|(set, _)| set.clone()))
                .unwrap_or_else(|| Bitset::empty(universe_len));
            // Negative leaves contribute mask zero (§6.3).
            (set.complement(universe_len), HashMap::new())
        }
        Nnf::And(a, b) => {
            let (set_a, masks_a) = fold_masks(a, leaf_sets, leaf_mask_bits, universe_len);
            let (set_b, masks_b) = fold_masks(b, leaf_sets, leaf_mask_bits, universe_len);
            let set = set_a.intersect(&set_b);
            // Union contributing masks from both satisfied branches for
            // retained IDs (§6.3).
            let mut masks = HashMap::new();
            for ordinal in set.iter_ordinals() {
                let mask = masks_a.get(&ordinal).copied().unwrap_or(0)
                    | masks_b.get(&ordinal).copied().unwrap_or(0);
                if mask != 0 {
                    masks.insert(ordinal, mask);
                }
            }
            (set, masks)
        }
        Nnf::Or(a, b) => {
            let (set_a, masks_a) = fold_masks(a, leaf_sets, leaf_mask_bits, universe_len);
            let (set_b, masks_b) = fold_masks(b, leaf_sets, leaf_mask_bits, universe_len);
            let set = set_a.union(&set_b);
            // Union masks of satisfied branches: an ID retained from branch
            // A carries A's mask; from both, the union (§6.3).
            let mut masks = HashMap::new();
            for ordinal in set.iter_ordinals() {
                let mask = masks_a.get(&ordinal).copied().unwrap_or(0)
                    | masks_b.get(&ordinal).copied().unwrap_or(0);
                if mask != 0 {
                    masks.insert(ordinal, mask);
                }
            }
            (set, masks)
        }
    }
}

/// Evaluate one deduplicated leaf: text leaves from the captured searcher,
/// structural leaves from the captured SQL transaction.
#[allow(clippy::type_complexity)]
fn eval_leaf(
    leaf: &Query,
    snapshot: &Snapshot<'_>,
) -> Result<Option<(Bitset, HashMap<u32, f32>)>, EvalError> {
    match leaf {
        Query::Text(text_leaf) => {
            let limit = snapshot
                .searcher
                .searcher
                .num_docs()
                .max(u64::try_from(snapshot.universe.len()).unwrap_or(u64::MAX));
            // TopDocs requires a positive limit even for an empty index.
            let limit = usize::try_from(limit).unwrap_or(usize::MAX).max(1);
            let hits = snapshot
                .searcher
                .leaf_hits(text_leaf, limit)
                .map_err(|error| EvalError::Search(error.to_string()))?;
            let mut set = Bitset::empty(snapshot.universe.len());
            let mut scores: HashMap<u32, f32> = HashMap::new();
            for hit in hits {
                if !hit.score.is_finite() {
                    return Err(EvalError::NonFiniteScore);
                }
                if let Some(ordinal) = snapshot.universe.ordinal(hit.note_id) {
                    set.set(ordinal);
                    // The captured searcher's score for this leaf; identical
                    // leaves share one evaluation.
                    scores.insert(ordinal, hit.score);
                }
            }
            Ok(Some((set, scores)))
        }
        Query::Attachment { .. } => Err(EvalError::AttachmentNotSearchable),
        structural => {
            let (where_clause, params) = structural
                .to_sql()
                .map_err(|error| EvalError::Sql(error.to_string()))?;
            let sql = format!("SELECT note_id FROM notes WHERE {where_clause}");
            let mut stmt = snapshot
                .tx
                .prepare(&sql)
                .map_err(|error| EvalError::Sql(error.to_string()))?;
            let param_refs: Vec<&dyn rusqlite::ToSql> =
                params.iter().map(std::convert::AsRef::as_ref).collect();
            let rows = stmt
                .query_map(param_refs.as_slice(), |row| row.get::<_, i64>(0))
                .map_err(|error| EvalError::Sql(error.to_string()))?;
            let mut set = Bitset::empty(snapshot.universe.len());
            for row in rows {
                let note_id = row.map_err(|error| EvalError::Sql(error.to_string()))?;
                if let Some(ordinal) = snapshot.universe.ordinal(note_id) {
                    set.set(ordinal);
                }
            }
            Ok(Some((set, HashMap::new())))
        }
    }
}
