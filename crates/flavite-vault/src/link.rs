//! Semantic link insertion: plan store and intent deduplication (RFC-0003
//! §9, W05).
//!
//! `link.prepare` builds a bounded, expiring plan against an exact source
//! revision and canonical target identity; `link.insert` consumes it once.
//! Plan IDs are process-local capabilities: never persisted (FR-26) and
//! never trusted merely because they appear in client JSON. The store is
//! owned by the Session; commit linearizes under the session ingress.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Live semantic insertion plans per session (NFR-18: ≤256 / 10 s TTL).
pub const MAX_LIVE_LINK_PLANS: usize = 256;
/// Plan time-to-live; an expired confirmation leaves notes unchanged.
pub const LINK_PLAN_TTL: Duration = Duration::from_secs(10);
/// Terminal results retained per client namespace (NFR-18).
pub const LINK_RESULT_CACHE_LIMIT: usize = 4096;

/// A core-issued client namespace token (§9.1). Opaque, process-local,
/// never persisted; the host requests one per connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientToken(pub u128);

/// A gesture identity: one user action inside one client namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LinkIntent {
    /// The core-issued client namespace.
    pub client: ClientToken,
    /// Monotonic gesture sequence inside that namespace.
    pub sequence: u64,
}

/// Whether the user enabled stable-link insertion (D-06: an existing
/// unconflicted ID MAY be used only when enabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StableLinkPolicy {
    /// Always serialize the vault-relative path.
    Disabled,
    /// Use the stable-ID form when the target has an unconflicted ID;
    /// otherwise fall back to the path form.
    PreferStable,
}

/// One prepare request. The target is the vault-relative note path (with or
/// without `.md`); the core resolves it through the canonical resolver and
/// records the exact entity identity in the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkPrepareRequest {
    /// Gesture identity.
    pub intent: LinkIntent,
    /// Source buffer.
    pub buffer_id: u64,
    /// Exact source revision the caret is acknowledged against.
    pub revision: u64,
    /// Collapsed caret as a UTF-8 byte offset in normalized text.
    pub caret: usize,
    /// Vault-relative target path (components joined by `/`).
    pub target_path: String,
    /// Stable-link policy.
    pub stable_link_policy: StableLinkPolicy,
}

/// Source identity a plan is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSourceRef {
    /// Source buffer.
    pub buffer_id: u64,
    /// Exact source revision.
    pub revision: u64,
    /// Collapsed caret.
    pub caret: usize,
}

impl PreparedLink {
    /// Stamp the store-assigned plan ID.
    #[must_use]
    pub fn with_plan_id(mut self, plan_id: u128) -> Self {
        self.plan_id = plan_id;
        self
    }
}

/// The prepared plan exposed to hosts (RFC §9.1 result shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLink {
    /// Opaque process-local plan capability (assigned by the store).
    pub plan_id: u128,
    /// The serialized `[[...]]` text.
    pub serialized_link: String,
    /// Display title of the target (shown separately, not inserted).
    pub target_display: String,
    /// Vault-relative target components.
    pub target_path: Vec<String>,
    /// Time-to-live in milliseconds.
    pub expires_in_ms: u64,
    /// Canonical catalog epoch the target was validated against.
    pub validated_epoch: u64,
    /// Canonical catalog generation the target was validated against.
    pub validated_generation: u64,
    /// Source identity the plan is bound to.
    pub source: LinkSourceRef,
    /// Whether the host must obtain explicit confirmation before commit.
    pub requires_confirmation: bool,
}

/// A semantic-link commit outcome carried through the result cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The link was inserted; the buffer advanced to this revision.
    Applied {
        /// Source buffer.
        buffer_id: u64,
        /// New revision after the one guarded edit.
        new_revision: u64,
    },
}

/// A cached terminal gesture result: an outcome or a typed rejection.
pub type CachedOutcome = Result<LinkOutcome, LinkInsertionError>;

/// Typed semantic-link rejection (FR-09: every case is a zero-mutation
/// refusal with a relevant repair action).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkInsertionError {
    /// Another semantic insertion is outstanding on this client namespace.
    Busy,
    /// A different payload was presented under a consumed gesture sequence.
    IntentMismatch,
    /// The gesture sequence was evicted or already consumed.
    IntentExpired,
    /// No plan exists for this plan ID on this client namespace.
    PlanNotFound,
    /// The plan outlived its TTL.
    PlanExpired,
    /// The plan was already consumed.
    PlanConsumed,
    /// The source buffer moved past the prepared revision.
    StaleSource {
        /// Revision the plan was prepared against.
        expected: u64,
        /// Current revision.
        actual: u64,
    },
    /// The canonical catalog advanced past the validated epoch.
    StaleEpoch,
    /// The target no longer resolves to the planned entity.
    TargetRetargeted,
    /// The target does not resolve to a note.
    MissingTarget,
    /// The target resolves to more than one note.
    AmbiguousTarget,
    /// The target is the source note itself (D-05: self-links refused).
    SelfTarget,
    /// The caret cannot host a link (masked range, inside an existing link,
    /// embed-adjacent, or bracket adjacency).
    CaretNotLinkable,
    /// The caret is not a UTF-8 boundary or is past the text.
    CaretOutOfBounds,
    /// The source document does not parse.
    SourceUnparsable(String),
    /// The target cannot be serialized under the existing grammar.
    UnrepresentableTarget(String),
    /// The source buffer is conflicted or orphaned and needs a decision.
    BufferStateNeedsDecision {
        /// The typed buffer state name.
        state: &'static str,
    },
    /// The intent is structurally invalid (zero sequence).
    InvalidIntent,
    /// The guarded edit was rejected by the buffer despite the guards.
    EditRejected(String),
}

impl LinkInsertionError {
    /// Stable machine code carried to API consumers.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Busy => "link.busy",
            Self::IntentMismatch => "link.intent_mismatch",
            Self::IntentExpired => "link.intent_expired",
            Self::PlanNotFound => "link.plan_not_found",
            Self::PlanExpired => "link.plan_expired",
            Self::PlanConsumed => "link.plan_consumed",
            Self::StaleSource { .. } => "link.stale_source",
            Self::StaleEpoch => "link.stale_epoch",
            Self::TargetRetargeted => "link.target_retargeted",
            Self::MissingTarget => "link.missing_target",
            Self::AmbiguousTarget => "link.ambiguous_target",
            Self::SelfTarget => "link.self_target",
            Self::CaretNotLinkable => "link.caret_not_linkable",
            Self::CaretOutOfBounds => "link.caret_out_of_bounds",
            Self::SourceUnparsable(_) => "link.source_unparsable",
            Self::UnrepresentableTarget(_) => "link.unrepresentable_target",
            Self::BufferStateNeedsDecision { .. } => "link.buffer_state_needs_decision",
            Self::InvalidIntent => "link.invalid_intent",
            Self::EditRejected(_) => "link.edit_rejected",
        }
    }

    /// The user-facing repair action for this rejection.
    #[must_use]
    pub fn repair_action(&self) -> String {
        match self {
            Self::Busy => "wait for the outstanding insertion to finish and try again".into(),
            Self::IntentMismatch => "start a new insertion gesture".into(),
            Self::IntentExpired => {
                "start a new insertion gesture; this one cannot be replayed".into()
            }
            Self::PlanNotFound | Self::PlanConsumed => "prepare the link again".into(),
            Self::PlanExpired => "the confirmation expired; prepare the link again".into(),
            Self::StaleSource { .. } => "place the caret again and prepare the link again".into(),
            Self::StaleEpoch | Self::TargetRetargeted => {
                "the vault index changed; prepare the link again".into()
            }
            Self::MissingTarget => "choose an existing note to link to".into(),
            Self::AmbiguousTarget => "disambiguate the target with its full vault path".into(),
            Self::SelfTarget => "choose a different note to link to".into(),
            Self::CaretNotLinkable => {
                "move the caret to a plain-text position outside code, math, and existing links"
                    .into()
            }
            Self::CaretOutOfBounds => "place the caret inside the note text".into(),
            Self::SourceUnparsable(_) => {
                "resolve the document parse error before inserting links".into()
            }
            Self::UnrepresentableTarget(_) => {
                "choose a target representable by the existing link grammar".into()
            }
            Self::BufferStateNeedsDecision { .. } => {
                "resolve the conflict or recovery decision on this note first".into()
            }
            Self::InvalidIntent => "use a fresh positive gesture sequence".into(),
            Self::EditRejected(_) => "place the caret again and prepare the link again".into(),
        }
    }
}

/// One stored plan (process-local only).
#[derive(Debug, Clone)]
pub(crate) struct LinkPlan {
    /// Opaque plan identity.
    pub plan_id: u128,
    /// Gesture that owns this plan.
    pub intent: LinkIntent,
    /// Source binding.
    pub source: LinkSourceRef,
    /// Exact entity identity from the canonical resolver.
    pub target_entity_id: i64,
    /// Target portable path components.
    pub target_path: Vec<String>,

    /// The authorized edit: (start, end, replacement) in normalized bytes.
    pub edit: (usize, usize, String),
    /// Canonical catalog epoch the target was validated against.
    pub validated_epoch: u64,
    /// Canonical catalog generation at validation time.
    pub validated_generation: u64,
    /// Deadline after which the plan cannot commit.
    pub expires_at: Instant,
}

/// A completed prepare request's payload fingerprint (dedup key value).
pub(crate) type Fingerprint = u64;

/// Bounded plan store with gesture deduplication (§9.2).
#[derive(Debug, Default)]
pub struct LinkPlanStore {
    plans: HashMap<u128, LinkPlan>,
    /// The one outstanding gesture per client namespace.
    outstanding: HashMap<ClientToken, u64>,
    /// Terminal results per (client, sequence).
    results: HashMap<(ClientToken, u64), (Fingerprint, CachedOutcome)>,
    /// Monotonic highest-consumed sequence per client.
    high_water: HashMap<ClientToken, u64>,
    results_order: Vec<(ClientToken, u64)>,
    next_plan_id: u128,
    /// Explicit overflow counters; nothing is silently dropped.
    pub plans_overflowed: u64,
    pub results_overflowed: u64,
}

/// What admission decided for one prepare request.
#[derive(Debug)]
pub(crate) enum PrepareAdmission {
    /// Run the expensive prepare work; the plan is published on completion.
    Proceed,
}

/// What admission decided for one commit request.
#[derive(Debug)]
pub(crate) enum CommitAdmission {
    /// Run the guarded apply for this plan.
    Proceed(Box<LinkPlan>),
    /// The identical gesture already completed; replay its outcome.
    Cached(CachedOutcome),
}

fn fingerprint_prepare(request: &LinkPrepareRequest) -> Fingerprint {
    let mut state: u64 = 0xcbf2_9ce4_8422_2325;
    let payload = [
        request.buffer_id.to_le_bytes().as_slice(),
        request.revision.to_le_bytes().as_slice(),
        request.caret.to_le_bytes().as_slice(),
        request.target_path.as_bytes(),
        &[request.stable_link_policy.discriminant()],
        request.intent.sequence.to_le_bytes().as_slice(),
    ]
    .concat();
    for byte in payload.iter() {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x1000_0000_01b3);
    }
    state
}

/// The commit payload fingerprint (plan ID) used by the Session.
#[must_use]
pub fn fingerprint_insert_public(plan_id: u128) -> Fingerprint {
    fingerprint_insert(plan_id)
}

fn fingerprint_insert(plan_id: u128) -> Fingerprint {
    plan_id as u64 ^ (plan_id >> 64) as u64
}

impl StableLinkPolicy {
    const fn discriminant(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::PreferStable => 1,
        }
    }
}

impl LinkPlanStore {
    /// Allocate a fresh opaque plan ID.
    #[must_use]
    pub fn allocate_plan_id(&mut self) -> u128 {
        self.next_plan_id = self.next_plan_id.wrapping_add(1).max(1);
        self.next_plan_id
    }

    /// Admit one prepare request (fast checks on the session turn).
    pub(crate) fn admit_prepare(
        &mut self,
        request: &LinkPrepareRequest,
    ) -> Result<PrepareAdmission, LinkInsertionError> {
        let client = request.intent.client;
        let sequence = request.intent.sequence;
        if sequence == 0 {
            return Err(LinkInsertionError::InvalidIntent);
        }
        if self.outstanding.contains_key(&client) {
            // Busy does not consume the sequence (§9.2).
            return Err(LinkInsertionError::Busy);
        }
        let _fingerprint = fingerprint_prepare(request);
        if let Some(cached_sequence) = self.high_water.get(&client)
            && sequence <= *cached_sequence
        {
            // A consumed gesture cannot run again; a cached prepare result
            // replays only while its fingerprint matches.
            // A consumed gesture cannot run again; the terminal result is
            // replayable only through commit, never through a new prepare.
            let _ = self.results.get(&(client, sequence));
            return Err(LinkInsertionError::IntentExpired);
        }
        self.purge_expired();
        if self.plans.len() >= MAX_LIVE_LINK_PLANS {
            self.plans_overflowed = self.plans_overflowed.saturating_add(1);
            return Err(LinkInsertionError::Busy);
        }
        self.outstanding.insert(client, sequence);
        Ok(PrepareAdmission::Proceed)
    }

    /// Install a completed plan under its allocated ID.
    pub(crate) fn complete_prepare(&mut self, plan_id: u128, mut plan: LinkPlan) {
        plan.plan_id = plan_id;
        self.plans.insert(plan_id, plan);
    }

    /// Release the outstanding gesture after a failed prepare so the user
    /// can retry with a fresh sequence.
    pub fn fail_prepare(&mut self, client: ClientToken) {
        self.outstanding.remove(&client);
    }

    /// Admit one commit request (fast checks on the session turn).
    pub(crate) fn admit_commit(
        &mut self,
        intent: &LinkIntent,
        plan_id: u128,
    ) -> Result<CommitAdmission, LinkInsertionError> {
        let client = intent.client;
        let sequence = intent.sequence;
        let fingerprint = fingerprint_insert(plan_id);
        if let Some((cached_fingerprint, outcome)) = self.results.get(&(client, sequence)) {
            return if *cached_fingerprint == fingerprint {
                Ok(CommitAdmission::Cached(outcome.clone()))
            } else {
                Err(LinkInsertionError::IntentMismatch)
            };
        }
        if self
            .high_water
            .get(&client)
            .is_some_and(|high_water| sequence <= *high_water)
        {
            return Err(LinkInsertionError::IntentExpired);
        }
        let cloned = match self.plans.get(&plan_id) {
            Some(plan) if plan.intent == *intent => plan.clone(),
            Some(_) => return Err(LinkInsertionError::PlanNotFound),
            None => return Err(LinkInsertionError::PlanNotFound),
        };
        if cloned.expires_at <= Instant::now() {
            self.plans.remove(&plan_id);
            self.outstanding.remove(&client);
            return Err(LinkInsertionError::PlanExpired);
        }
        self.plans.remove(&plan_id);
        self.outstanding.remove(&client);
        Ok(CommitAdmission::Proceed(Box::new(cloned)))
    }

    /// Record the terminal outcome of one gesture, bounded and counted.
    pub fn record_outcome(
        &mut self,
        intent: &LinkIntent,
        fingerprint: Fingerprint,
        outcome: CachedOutcome,
    ) {
        let key = (intent.client, intent.sequence);
        if self.results.contains_key(&key) {
            return; // The first terminal outcome for a gesture is final.
        }
        self.results.insert(key, (fingerprint, outcome));
        self.results_order.push(key);
        let high_water = self.high_water.entry(intent.client).or_insert(0);
        *high_water = (*high_water).max(intent.sequence);
        while self.results_order.len() > LINK_RESULT_CACHE_LIMIT {
            let evicted = self.results_order.remove(0);
            self.results.remove(&evicted);
            self.results_overflowed = self.results_overflowed.saturating_add(1);
        }
    }

    /// Whether a gesture is currently outstanding on this namespace.
    #[must_use]
    pub fn is_outstanding(&self, client: ClientToken) -> bool {
        self.outstanding.contains_key(&client)
    }

    /// Cancel an outstanding gesture (host cancellation: no auto commit
    /// after cancellation, §9 worker table).
    pub fn cancel(&mut self, client: ClientToken) {
        if let Some(sequence) = self.outstanding.remove(&client) {
            self.plans.retain(|_, plan| {
                !(plan.intent.client == client && plan.intent.sequence == sequence)
            });
        }
    }

    /// Drop expired plans; their outstanding gestures are released.
    pub fn purge_expired(&mut self) {
        let now = Instant::now();
        let expired: Vec<u128> = self
            .plans
            .iter()
            .filter(|(_, plan)| plan.expires_at <= now)
            .map(|(plan_id, _)| *plan_id)
            .collect();
        for plan_id in expired {
            if let Some(plan) = self.plans.remove(&plan_id)
                && self.outstanding.get(&plan.intent.client) == Some(&plan.intent.sequence)
            {
                self.outstanding.remove(&plan.intent.client);
            }
        }
    }

    /// Number of live plans.
    #[must_use]
    pub fn live_plans(&self) -> usize {
        self.plans.len()
    }
}

/// The expensive prepare work, run on a bounded worker thread: acquire the
/// source snapshot, resolve the target through the canonical resolver,
/// serialize with the shared formatter, and prove the prospective parse.
pub(crate) struct PrepareContext<'a> {
    pub manager: &'a std::sync::Mutex<flavite_buffer::manager::BufferManager>,
    pub actor: &'a crate::actor::VaultActorHandle,
}

pub(crate) fn prepare_work(
    context: &PrepareContext<'_>,
    request: &LinkPrepareRequest,
) -> Result<(LinkPlan, PreparedLink), LinkInsertionError> {
    use flavite_buffer::manager::BufferManager;
    use flavite_syntax::link_format::{
        ProofRejection, prove_insertion, serialize_link, serialize_stable_link,
    };
    use flavite_syntax::text::fold_key;

    let buffer_id = request.buffer_id;
    let (text, source_path) = {
        let manager: std::sync::MutexGuard<'_, BufferManager> =
            context.manager.lock().expect("buffer manager lock");
        let buffer = manager
            .get_buffer_by_id(flavite_buffer::BufferId::from(buffer_id))
            .ok_or(LinkInsertionError::StaleSource {
                expected: request.revision,
                actual: 0,
            })?;
        let actual = buffer.revision().as_u64();
        if actual != request.revision {
            return Err(LinkInsertionError::StaleSource {
                expected: request.revision,
                actual,
            });
        }
        let path = manager
            .path_for_id(flavite_buffer::BufferId::from(buffer_id))
            .cloned()
            .ok_or(LinkInsertionError::StaleSource {
                expected: request.revision,
                actual,
            })?;
        (buffer.text().to_owned(), path)
    };
    if request.caret > text.len() || !text.is_char_boundary(request.caret) {
        return Err(LinkInsertionError::CaretOutOfBounds);
    }

    // Canonical resolution of the target path through the shared resolver.
    let raw_target = request
        .target_path
        .strip_suffix(".md")
        .unwrap_or(&request.target_path)
        .to_owned();
    let resolution = context
        .actor
        .resolve_target(fold_key(&raw_target), raw_target.clone(), false, None)
        .map_err(|_| LinkInsertionError::MissingTarget)?;
    let note = match resolution {
        resolution if resolution.is_ambiguous => return Err(LinkInsertionError::AmbiguousTarget),
        resolution => match resolution.note {
            Some(note) => note,
            None => return Err(LinkInsertionError::MissingTarget),
        },
    };

    // Portable vault-relative path + title for the resolved entity, matched
    // by exact native path bytes (display strings are not identity).
    let catalog = context
        .actor
        .note_catalog(50_000)
        .map_err(|_| LinkInsertionError::MissingTarget)?;
    let entry = catalog
        .items
        .iter()
        .find(|entry| entry.path_native == note.path_native)
        .ok_or(LinkInsertionError::MissingTarget)?;
    if std::path::Path::new(&entry.path) == source_path {
        return Err(LinkInsertionError::SelfTarget);
    }
    let components: Vec<String> = std::path::Path::new(&entry.path)
        .components()
        .filter_map(|component| component.as_os_str().to_str().map(str::to_owned))
        .collect();
    if components.is_empty() {
        return Err(LinkInsertionError::UnrepresentableTarget(
            entry.path.clone(),
        ));
    }

    // Serialization: stable-ID form only when enabled and unconflicted.
    let serialized = match request.stable_link_policy {
        StableLinkPolicy::Disabled => serialize_link(&components, None),
        StableLinkPolicy::PreferStable => {
            let target_key = entry.path.strip_suffix(".md").unwrap_or(&entry.path);
            let stable_id = context
                .actor
                .completion_candidates()
                .ok()
                .and_then(|candidates| {
                    candidates
                        .into_iter()
                        .find(|candidate| candidate.target == target_key)
                        .and_then(|candidate| candidate.stable_id)
                });
            match stable_id {
                Some(id) if !id.is_empty() => serialize_stable_link(&id, entry.title.as_str()),
                _ => serialize_link(&components, None),
            }
        }
    }
    .map_err(|error| LinkInsertionError::UnrepresentableTarget(error.to_string()))?;
    let proof =
        prove_insertion(&text, request.caret, &serialized).map_err(
            |rejection| match rejection {
                ProofRejection::CaretOutOfBounds => LinkInsertionError::CaretOutOfBounds,
                ProofRejection::CaretNotLinkable => LinkInsertionError::CaretNotLinkable,
                ProofRejection::SourceUnparsable(message) => {
                    LinkInsertionError::SourceUnparsable(message)
                }
            },
        )?;

    let version = context
        .actor
        .index_version()
        .map_err(|_| LinkInsertionError::StaleEpoch)?;
    let prepared = PreparedLink {
        plan_id: 0, // Assigned by the store on completion.
        serialized_link: serialized.text.clone(),
        target_display: entry.title.clone(),
        target_path: components,
        expires_in_ms: u64::try_from(LINK_PLAN_TTL.as_millis()).unwrap_or(10_000),
        validated_epoch: version.0,
        validated_generation: version.1,
        source: LinkSourceRef {
            buffer_id,
            revision: request.revision,
            caret: request.caret,
        },
        requires_confirmation: false,
    };
    let plan = LinkPlan {
        plan_id: 0,
        intent: request.intent,
        source: prepared.source.clone(),
        target_entity_id: note.note_id,
        target_path: prepared.target_path.clone(),
        edit: (proof.start, proof.end, proof.replacement),
        validated_epoch: version.0,
        validated_generation: version.1,
        expires_at: Instant::now() + LINK_PLAN_TTL,
    };
    Ok((plan, prepared))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(client: u128, sequence: u64) -> LinkIntent {
        LinkIntent {
            client: ClientToken(client),
            sequence,
        }
    }

    fn prepare(client: u128, sequence: u64) -> LinkPrepareRequest {
        LinkPrepareRequest {
            intent: intent(client, sequence),
            buffer_id: 7,
            revision: 23,
            caret: 118,
            target_path: "Systems/Emergence.md".into(),
            stable_link_policy: StableLinkPolicy::Disabled,
        }
    }

    #[test]
    fn one_outstanding_prepare_per_client_and_busy_does_not_consume() {
        let mut store = LinkPlanStore::default();
        assert!(matches!(
            store.admit_prepare(&prepare(1, 41)),
            Ok(PrepareAdmission::Proceed)
        ));
        assert!(
            matches!(
                store.admit_prepare(&prepare(1, 42)),
                Err(LinkInsertionError::Busy)
            ),
            "second concurrent prepare is Busy"
        );
        // A failure releases the namespace; the same sequence can then run.
        store.fail_prepare(ClientToken(1));
        assert!(matches!(
            store.admit_prepare(&prepare(1, 41)),
            Ok(PrepareAdmission::Proceed)
        ));
    }

    #[test]
    fn consumed_sequences_expire_and_zero_sequences_are_invalid() {
        let mut store = LinkPlanStore::default();
        assert!(matches!(
            store.admit_prepare(&prepare(1, 0)),
            Err(LinkInsertionError::InvalidIntent)
        ));
        store.record_outcome(
            &intent(1, 41),
            1,
            Ok(LinkOutcome::Applied {
                buffer_id: 7,
                new_revision: 24,
            }),
        );
        assert!(matches!(
            store.admit_prepare(&prepare(1, 41)),
            Err(LinkInsertionError::IntentExpired)
        ));
        assert!(matches!(
            store.admit_prepare(&prepare(1, 40)),
            Err(LinkInsertionError::IntentExpired)
        ));
    }

    #[test]
    fn commit_dedup_matches_fingerprints_and_eviction_counts() {
        let mut store = LinkPlanStore::default();
        store.record_outcome(
            &intent(1, 41),
            fingerprint_insert(9),
            Ok(LinkOutcome::Applied {
                buffer_id: 7,
                new_revision: 24,
            }),
        );
        // Same plan ID: cached replay.
        assert!(matches!(
            store.admit_commit(&intent(1, 41), 9),
            Ok(CommitAdmission::Cached(_))
        ));
        // Different plan ID under the same gesture: mismatch.
        assert!(matches!(
            store.admit_commit(&intent(1, 41), 10),
            Err(LinkInsertionError::IntentMismatch)
        ));
        // Eviction is counted, never silent.
        for sequence in 42..(42 + LINK_RESULT_CACHE_LIMIT as u64 + 2) {
            store.record_outcome(
                &intent(1, sequence),
                fingerprint_insert(u128::from(sequence)),
                Ok(LinkOutcome::Applied {
                    buffer_id: 7,
                    new_revision: 24,
                }),
            );
        }
        // 4099 terminal results against a 4096-entry cache: the three
        // oldest (including the probe result) were displaced, never lost.
        assert_eq!(store.results_overflowed, 3);
        assert!(matches!(
            store.admit_commit(&intent(1, 41), 9),
            Err(LinkInsertionError::IntentExpired)
        ));
    }

    #[test]
    fn cancellation_drops_the_gesture_without_consuming_the_sequence() {
        let mut store = LinkPlanStore::default();
        let _ = store.admit_prepare(&prepare(1, 41));
        store.cancel(ClientToken(1));
        assert!(!store.is_outstanding(ClientToken(1)));
        // The sequence was never consumed, so a fresh gesture can reuse it.
        assert!(matches!(
            store.admit_prepare(&prepare(1, 41)),
            Ok(PrepareAdmission::Proceed)
        ));
    }
}
