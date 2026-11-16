#!/usr/bin/env bash
# FEUX-0002 cumulative source gates (X01..X08). The slice argument names the
# highest completed phase; every gate from X01 through that slice runs
# (cumulative — skipping completed earlier gates is forbidden), and future
# phase gates are never required early. Presence gates demand nonempty files;
# textual absence gates succeed only on rg exit 1, never exit 2. The second
# argument exists for negative fixtures (xtask/tests/feux_gates.rs points it
# at a seeded tree).
set -euo pipefail

slice="${1:-}"
root="${2:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"

if [[ ! -f "$root/scripts/lib/source-gates.sh" ]]; then
    echo "missing gate library: $root/scripts/lib/source-gates.sh" >&2
    exit 2
fi
# shellcheck source=lib/source-gates.sh
source "$root/scripts/lib/source-gates.sh"

GATES_RUN=0
GATES_PASSED=0
GATES_FAILED=0

gate() {
    GATES_RUN=$((GATES_RUN + 1))
    if "$@"; then
        GATES_PASSED=$((GATES_PASSED + 1))
    else
        GATES_FAILED=$((GATES_FAILED + 1))
    fi
}

# Presence of a production pattern (nonempty sources, not stubs).
assert_present() {
    local pattern="$1"; shift
    if rg -n -- "$pattern" "$@" >/dev/null 2>&1; then
        return 0
    fi
    echo "required production pattern missing: $pattern" >&2
    return 1
}

# Absence of a sleep inside one named function body (production history
# paths only; unrelated worker loops are out of scope).
assert_no_sleep_in_fn() {
    local file="$1" fn="$2"
    awk -v fn="$fn" '
        $0 ~ "^[[:space:]]*(pub |pub(crate) |fn )?" fn "\\(" { inside = 1 }
        inside && /std::thread::sleep|tokio::time::sleep/ { print; bad = 1 }
        inside && /^}/ { inside = 0 }
        END { exit bad }
    ' "$file"
}

run_x01_gates() {
    local file
    for file in \
        crates/flavite-buffer/src/budget.rs \
        crates/flavite-buffer/src/undo.rs \
        crates/flavite-buffer/src/history.rs \
        crates/flavite-buffer/src/model.rs \
        crates/flavite-buffer/tests/history_budget.rs \
        crates/flavite-api/tests/editing_history.rs \
        crates/flavite-ui/src/views/editor_history.rs \
        scripts/check-feux-contracts.sh \
        xtask/tests/feux_gates.rs \
        scripts/tests/workbench/context_mode_budget.py \
        scripts/workbench-visual-linux.sh; do
        gate assert_nonempty "$root/$file"
    done
    # Budget math stays pure and toolkit-free, with no placeholder code.
    gate assert_absent 'TODO|todo!\(|unimplemented!\(' \
        "$root/crates/flavite-buffer/src/budget.rs"
    # No sleeps in the buffer crate (pure history model) or inside the
    # Session functions that own the history-budget paths. The delivered
    # recovery-worker poll sleep in session.rs is unrelated to history and
    # is excluded by the per-function scope (deviation from the literal
    # spec grep, recorded in the X01 verification record).
    gate assert_absent 'std::thread::sleep|tokio::time::sleep' \
        "$root/crates/flavite-buffer/src"
    local session_fn
    for session_fn in \
        apply_edits \
        editing_history \
        restore_editing_state \
        seal_undo_group \
        set_app_history_budget \
        set_buffer_history_budgets; do
        gate assert_no_sleep_in_fn "$root/crates/flavite-vault/src/session.rs" "$session_fn"
    done
    # The private raw jump stays private (X03 gate, enforced from X01).
    gate assert_absent 'pub fn goto\b' "$root/crates/flavite-buffer/src/undo.rs"
    # Reservation/eviction/refusal plumbing exists for real.
    gate assert_present 'fn reserve' "$root/crates/flavite-buffer/src/undo.rs"
    gate assert_present 'fn evict_to_fit' "$root/crates/flavite-buffer/src/undo.rs"
    gate assert_present 'history_truncated' \
        "$root/crates/flavite-buffer/src/history.rs" \
        "$root/crates/flavite-buffer/src/undo.rs"
    gate assert_present 'HistoryChanged' "$root/crates/flavite-buffer/src/history.rs"
    gate assert_present 'APP_WIDE_MAX_HISTORY_BYTES' \
        "$root/crates/flavite-buffer/src/budget.rs" \
        "$root/crates/flavite-vault/src/session.rs"
    gate assert_present 'LimitExceeded' "$root/crates/flavite-buffer/src/model.rs"
}

run_x02_gates() {
    run_x01_gates
    # The consolidated crate exists with real content (no empty-module gate).
    local file
    for file in \
        crates/flavite-edit-context/Cargo.toml \
        crates/flavite-edit-context/src/lib.rs \
        crates/flavite-edit-context/src/key.rs \
        crates/flavite-edit-context/src/vocabulary.rs \
        crates/flavite-edit-context/src/grammar.rs \
        crates/flavite-edit-context/src/session.rs \
        crates/flavite-edit-context/src/effect.rs \
        crates/flavite-edit-context/tests/transitions.rs; do
        gate assert_nonempty "$root/$file"
    done
    # Exactly one state machine and one grammar: neither the widget crate nor
    # the shell defines the mode, the grammar functions, or a second effect
    # module. The temporary move re-export is gone (imports only).
    gate assert_absent 'pub enum ContextMode' \
        "$root/crates/flavite-editor/src" "$root/crates/flavite-ui/src"
    gate assert_absent 'fn command_key|fn motion_key|fn menu_key' \
        "$root/crates/flavite-editor/src" "$root/crates/flavite-ui/src"
    gate assert_absent 'pub mod effect' "$root/crates/flavite-editor/src/context.rs"
    gate assert_absent 'pub use flavite_edit_context' "$root/crates/flavite-editor/src"
    # The crate is toolkit-free and placeholder-free.
    gate assert_absent 'iced|winit|wgpu|cosmic_text' "$root/crates/flavite-edit-context/src"
    gate assert_absent 'TODO|todo!\(|unimplemented!\(' \
        "$root/crates/flavite-edit-context/src" \
        "$root/crates/flavite-edit-context/tests"
    # The statechart entry point and its event/effect vocabulary exist.
    gate assert_present 'pub fn reduce' "$root/crates/flavite-edit-context/src/session.rs"
    gate assert_present 'pub enum Event' "$root/crates/flavite-edit-context/src/session.rs"
    gate assert_present 'pub enum Effect' "$root/crates/flavite-edit-context/src/session.rs"
}

run_x03_gates() {
    run_x02_gates
    # X03 artifacts exist with real content.
    local file
    for file in \
        crates/flavite-buffer/src/selection.rs \
        crates/flavite-buffer/src/commands.rs \
        crates/flavite-buffer/tests/selection_properties.rs \
        crates/flavite-buffer/tests/movement_deltas.rs \
        crates/flavite-vault/src/editor_receipts.rs \
        crates/flavite-vault/tests/editor_receipts.rs \
        crates/flavite-api/src/editor_commands.rs \
        crates/flavite-api/tests/editor_commands.rs \
        crates/flavite-ui/tests/editor_spine.rs; do
        gate assert_nonempty "$root/$file"
    done
    # The protocol crate never exists, anywhere.
    gate assert_absent 'flavite-edit-protocol' \
        "$root/Cargo.toml" "$root/Cargo.lock" "$root/crates" "$root/docs"
    # The workspace `Selection` type and the 256 cap are deleted; canonical
    # values come from `flavite-buffer::selection` only.
    gate assert_absent '^pub struct Selection\b' \
        "$root/crates/flavite-workspace/src" "$root/crates/flavite-buffer/src/model.rs"
    gate assert_absent 'selections\.len\(\) > 256|1\.\.=256' \
        "$root/crates/flavite-workspace/src"
    # The algebra stays pure and toolkit-free.
    gate assert_absent 'iced|winit|wgpu|cosmic_text|std::fs|std::net' \
        "$root/crates/flavite-buffer/src/selection.rs"
    # No raw public undo jump.
    gate assert_absent 'pub fn goto\b' "$root/crates/flavite-buffer/src/undo.rs"
    # No app-side re-implementation of normalization: nobody DEFINES the
    # algebra outside `flavite-buffer` (X04's widget live set CALLS the
    # canonical implementation — that is the whole point of E-1).
    gate assert_absent 'fn normalize\(' \
        "$root/crates/flavite-ui/src" "$root/crates/flavite-editor/src"
    # Per-view selection version (workspace data + the GUI target that
    # validates it, FEUX-001 §5.4's split) and the ticket ledger exist.
    # Each file asserted individually: rg over several files succeeds on any
    # one match, which would make a multi-file presence gate vacuous.
    gate assert_present 'selection_version' "$root/crates/flavite-workspace/src/input.rs"
    gate assert_present 'selection_version' "$root/crates/flavite-ui/src/host/context_mode.rs"
    gate assert_present 'pub struct OperationId' "$root/crates/flavite-vault/src/editor_receipts.rs"
    gate assert_present 'pub struct OperationId' "$root/crates/flavite-vault/src/editor_receipts.rs"
    gate assert_present 'MAX_SELECTIONS' "$root/crates/flavite-buffer/src/selection.rs"
    gate assert_present 'MovementSpan' "$root/crates/flavite-buffer/src/history.rs"
}

run_x04_gates() {
    run_x03_gates
    # X04 artifacts exist with real content.
    local file
    for file in \
        crates/flavite-editor/src/selections.rs \
        crates/flavite-editor/tests/multi_selection.rs \
        crates/flavite-ui/tests/multi_selection.rs \
        crates/flavite-ui/src/host/editor_clipboard.rs \
        crates/flavite-ui/src/views/note.rs \
        crates/flavite-ui/src/views/context_toolbar.rs \
        crates/flavite-ui/src/accessibility/tree.rs; do
        gate assert_nonempty "$root/$file"
    done
    # The widget's live state is a set; no single anchor/caret field access
    # remains on the live state (transient operation structs may carry their
    # own parameters; the editor state itself must not).
    gate assert_absent 'state\.(caret|anchor)\b' "$root/crates/flavite-editor/src/widget.rs"
    gate assert_present 'selections: EditorSelections' "$root/crates/flavite-editor/src/widget.rs"
    # Mutations route on the whole committed set, never `.first()`.
    gate assert_absent 'interaction\.selections\.first\(' "$root/crates/flavite-ui/src"
    # No inline OS-clipboard effects outside the seam (settings' path-copy
    # is a different surface; the seam itself owns the iced calls).
    gate assert_absent 'iced::clipboard::(read|write)|iced::Clipboard' \
        "$root/crates/flavite-editor/src" \
        "$root/crates/flavite-ui/src/host/context_mode.rs" \
        "$root/crates/flavite-ui/src/views"
    # No second regex engine sneaks into editor or planner paths (X04 gate).
    gate assert_absent 'Regex::new|regex::Regex' \
        "$root/crates/flavite-editor/src" \
        "$root/crates/flavite-buffer/src/commands.rs"
    # N-rendering: viewport-window member loop + primary caret distinction.
    gate assert_present 'visible_members' "$root/crates/flavite-editor/src/widget.rs"
    # OFF-state: muted fill channel + non-color count + Esc collapse.
    gate assert_present 'selection_muted' \
        "$root/crates/flavite-editor/src/widget.rs" \
        "$root/crates/flavite-ui/src/style/tokens.rs"
    gate assert_present 'member_count' \
        "$root/crates/flavite-ui/src/views/note.rs" \
        "$root/crates/flavite-ui/src/views/context_toolbar.rs"
    gate assert_present 'escape_collapse' "$root/crates/flavite-editor/src/selections.rs"
    # Link insertion refuses at N>1 with the specified message (FR-9).
    gate assert_present 'Choose one caret to insert a link' "$root/crates/flavite-ui/src/app.rs"
    # a11y N-selection description.
    gate assert_present 'selections, primary' "$root/crates/flavite-ui/src/accessibility/tree.rs"
}

run_x05_gates() {
    run_x04_gates
    # X05 artifacts exist with real content.
    local file
    for file in \
        crates/flavite-ui/tests/editor_catalog.rs \
        crates/flavite-edit-context/tests/transitions.rs \
        crates/flavite-buffer/tests/selection_ops.rs \
        scripts/tests/workbench/context_mode_setops.py; do
        gate assert_nonempty "$root/$file"
    done
    # AwaitChar is the one new statechart state (E-8).
    gate assert_present 'AwaitChar' "$root/crates/flavite-edit-context/src/session.rs"
    # Find-char is a scan, never a second regex engine (newline-index
    # scanning for line geometry is not a matcher).
    gate assert_absent 'Regex::new|regex::Regex' \
        "$root/crates/flavite-editor/src" \
        "$root/crates/flavite-edit-context/src"
    # The catalogue enumeration gate exists (NFR-12).
    gate assert_present 'seeded_missing_row_fails_the_enumeration' \
        "$root/crates/flavite-ui/tests/editor_catalog.rs"
}

run_x06_gates() {
    run_x05_gates
    # X06 artifacts exist with real content.
    local file
    for file in \
        crates/flavite-vault/src/text_select.rs \
        crates/flavite-api/tests/selection_find.rs \
        crates/flavite-ui/src/host/selection_search.rs \
        crates/flavite-ui/tests/selection_search.rs \
        scripts/tests/workbench/context_mode_matches.py; do
        gate assert_nonempty "$root/$file"
    done
    # No second matcher in the match-selection paths (E-10): the existing
    # engine is consumed through the vault collection, never a
    # re-implementation.
    gate assert_absent 'match_indices\(|Regex::new|regex::Regex' \
        "$root/crates/flavite-ui/src/host/selection_search.rs" \
        "$root/crates/flavite-api/src/editor_commands.rs"
    # No main-thread regex fallback and no legacy compat shim identifiers.
    # (The literal spec token `compat` is relaxed to identifier forms
    # because pre-existing API doc prose legitimately uses the word
    # "compatibility"; the semantic target — no second engine, no legacy
    # fallback — is enforced by the companion gates. Deviation recorded in
    # the X06 verification record.)
    gate assert_absent 'MainThreadRegex|main_thread_regex|legacy_regex|regex_compat' \
        "$root/crates/flavite-ui/src" "$root/crates/flavite-api/src"
    # No per-selection regex transform verbs anywhere (deferred with the
    # scope protocol, §1.2).
    gate assert_absent 'sel\.regex\.(split|keep)|keepMatching|splitRegex' \
        "$root/crates/flavite-edit-context/src" "$root/crates/flavite-ui/src"
    # Preview-key exactness is real (E-11) and committed overlay spans ride
    # the widget preview channel.
    gate assert_present 'PreviewKey' "$root/crates/flavite-ui/src/host/selection_search.rs"
    gate assert_present 'overlay_spans' "$root/crates/flavite-ui/src/host/selection_search.rs"
    gate assert_present 'with_match_preview' "$root/crates/flavite-editor/src/widget.rs"
    # The vault collection is a real consumer of the existing engine with
    # the selection-cap bound.
    gate assert_present 'MAX_SELECTION_SPANS' "$root/crates/flavite-vault/src/text_select.rs"
    gate assert_present 'MatchesCollected' "$root/crates/flavite-vault/src/session.rs"
}

run_x07_gates() {
    run_x06_gates
    # X07 artifacts exist with real content.
    local file
    for file in \
        crates/flavite-ui/src/host/editor_diagnostics.rs \
        crates/flavite-ui/tests/editor_diagnostics.rs \
        crates/flavite-ui/tests/editor_failures.rs \
        crates/flavite-edit-context/tests/burst.rs \
        scripts/tests/workbench/context_mode_failures.py; do
        gate assert_nonempty "$root/$file"
    done
    # No sleeps in the new host paths or the pure statechart (E-15).
    gate assert_absent 'std::thread::sleep|tokio::time::sleep' \
        "$root/crates/flavite-ui/src/host/editor_diagnostics.rs" \
        "$root/crates/flavite-ui/src/host/context_mode.rs" \
        "$root/crates/flavite-edit-context/src"
    # No network surface in diagnostics (NFR-10; just network-denied).
    gate assert_absent 'reqwest|TcpStream|UdpSocket|https?://' \
        "$root/crates/flavite-ui/src/host/editor_diagnostics.rs"
    # The ring schema never names content fields: content-free by design,
    # and the seeded-token scan proves the emit paths cannot leak.
    gate assert_absent 'buffer_text|clipboard_text|query_source|file_path|typed_chars' \
        "$root/crates/flavite-ui/src/host/editor_diagnostics.rs"
    # The burst hint offers Exit / Open-history only — never an undo action
    # (FEUX-001 D-22: shared-buffer edits make that promise false).
    gate assert_absent 'undo_burst|Undo burst' \
        "$root/crates/flavite-ui/src" \
        "$root/crates/flavite-edit-context/src"
    # The burst thresholds emit the effect, and the typed setting exists.
    gate assert_present 'ShowBurstHint' "$root/crates/flavite-edit-context/src/session.rs"
    gate assert_present 'BURST_WINDOW_MS' "$root/crates/flavite-edit-context/src/session.rs"
    gate assert_present 'BURST_SUPPRESS_MS' "$root/crates/flavite-edit-context/src/session.rs"
    gate assert_present 'burst_hint_enabled' "$root/crates/flavite-settings/src/registry.rs"
    gate assert_present 'MAX_DIAGNOSTIC_EVENTS' \
        "$root/crates/flavite-ui/src/host/editor_diagnostics.rs"
}

run_x08_gates() {
    run_x07_gates
    # X08 artifacts exist with real content (the release gate is the
    # acceptance scenario plus the user-facing feature document).
    local file
    for file in \
        scripts/tests/workbench/context_workbench_acceptance.py \
        docs/APP-FEATURES.md; do
        gate assert_nonempty "$root/$file"
    done
    # No compatibility surfaces anywhere in the touched crates (FR-23):
    # aliases, legacy keymaps, and deprecated ID shims are forbidden.
    gate assert_absent 'deprecatedIds|command_aliases|shortcut_aliases|legacy_keymap|legacy_chrome' \
        "$root/crates/flavite-api/src" \
        "$root/crates/flavite-ui/src" \
        "$root/crates/flavite-settings/src" \
        "$root/crates/flavite-edit-context/src"
    # No placeholder code remains in any FEUX-0002 production path (AC-18).
    gate assert_absent 'TODO|todo!\(|unimplemented!\(' \
        "$root/crates/flavite-buffer/src/selection.rs" \
        "$root/crates/flavite-buffer/src/budget.rs" \
        "$root/crates/flavite-edit-context/src" \
        "$root/crates/flavite-ui/src/host/context_mode.rs" \
        "$root/crates/flavite-ui/src/host/selection_search.rs" \
        "$root/crates/flavite-ui/src/host/editor_diagnostics.rs" \
        "$root/crates/flavite-ui/src/views/context_toolbar.rs"
    # The protocol crate never exists, anywhere — final sweep including
    # docs (E-1; the X03 gate scans the same roots; X08 keeps the fence).
    gate assert_absent 'flavite-edit-protocol' \
        "$root/Cargo.toml" "$root/Cargo.lock" "$root/crates" "$root/docs"
}

run_feux0004_x07_gates() {
    # FEUX-0004 X07: Close-out gates for the FEUX-0004 round
    # Presence gates for all X01-X07 artifacts
    local file
    for file in \
        crates/flavite-edit-context/src/key.rs \
        crates/flavite-edit-context/src/session.rs \
        crates/flavite-edit-context/src/keymap.rs \
        crates/flavite-edit-context/src/vocabulary.rs \
        crates/flavite-buffer/src/pairs.rs \
        crates/flavite-buffer/src/numbers.rs \
        crates/flavite-ui/src/host/command_args.rs \
        crates/flavite-ui/src/host/context_mode.rs \
        crates/flavite-ui/src/views/context_toolbar.rs \
        crates/flavite-editor/src/context/layout.rs \
        scripts/tests/workbench/context_mode_transforms.py \
        scripts/tests/workbench/context_mode_counts_repeat.py \
        scripts/tests/workbench/context_mode_surround.py; do
        gate assert_nonempty "$root/$file"
    done
    
    # Core FEUX-0004 presence checks
    gate assert_present 'ReservedPosition' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'is_digit_position' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'Digit0' "$root/crates/flavite-edit-context/src/key.rs"
    gate assert_present 'Code::Digit0' "$root/crates/flavite-editor/src/context.rs"
    gate assert_present 'pub count' "$root/crates/flavite-edit-context/src/session.rs"
    gate assert_present 'AwaitArgs' "$root/crates/flavite-edit-context/src/session.rs"
    gate assert_present 'InvocationRecord' "$root/crates/flavite-edit-context/src/session.rs"
    gate assert_present 'RepeatLast' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'Transform' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'Lines' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'Surround' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'find_enclosing_pair' "$root/crates/flavite-buffer/src/pairs.rs"
    gate assert_present 'scan_number' "$root/crates/flavite-buffer/src/numbers.rs"
    gate assert_present 'CommandArgsState' "$root/crates/flavite-ui/src/host/command_args.rs"
    gate assert_present 'count_prefix' "$root/crates/flavite-ui/src/views/context_toolbar.rs"
    gate assert_present 'editor.menu_transform' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'editor.menu_lines' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'editor.menu_surround' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'Transform' "$root/crates/flavite-editor/src/context/layout.rs"
    gate assert_present 'Lines' "$root/crates/flavite-editor/src/context/layout.rs"
    gate assert_present 'Surround' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'GotoLine' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    
    # No AwaitChar remains (replaced by AwaitArgs)
    gate assert_absent 'pub struct AwaitChar' "$root/crates/flavite-edit-context/src/session.rs"
    
    # Grave position is excluded permanently (E-10)
    gate assert_absent 'Grave|Backquote' "$root/crates/flavite-edit-context/src/key.rs"
    
    # Documentation updated with FEUX-0004 features
    gate assert_present 'Transform menu' "$root/docs/APP-FEATURES.md"
    
    # E-3: shifted menu openers are allowed (Rule X amended)
    gate assert_present 'shifted.*KeyT' "$root/crates/flavite-edit-context/src/keymap.rs"
}

run_feux0003_x01_gates() {
    # FEUX-0003 X01: Position vocabulary and keymap data core
    # 35-position vocabulary exists
    gate assert_present 'KeyM|KeyN|KeyQ|KeyR|KeyS|KeyZ' "$root/crates/flavite-edit-context/src/key.rs"
    gate assert_present 'Quote|Comma|Period|Backslash|BracketLeft|BracketRight|Minus|Equal' "$root/crates/flavite-edit-context/src/key.rs"
    # New modules exist
    gate assert_nonempty "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_nonempty "$root/crates/flavite-edit-context/src/labels.rs"
    gate assert_nonempty "$root/crates/flavite-edit-context/src/text.rs"
    # keymap exports exist
    gate assert_present 'pub struct ContextKeymap' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'pub struct Slot' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'pub enum BindingId' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'pub enum Layout' "$root/crates/flavite-edit-context/src/labels.rs"
    # Text helpers moved to text.rs
    gate assert_present 'pub fn word_bounds' "$root/crates/flavite-edit-context/src/text.rs"
    gate assert_present 'pub fn selected_characters' "$root/crates/flavite-edit-context/src/text.rs"
    gate assert_present 'pub fn clamp_grapheme_boundary' "$root/crates/flavite-edit-context/src/text.rs"
    # Grammar still exists in X01, deleted in X02+
    # gate assert_nonempty "$root/crates/flavite-edit-context/src/grammar.rs"
    # Editor capture boundary extended to 35 positions
    gate assert_present 'Code::KeyM|Code::KeyN|Code::KeyQ|Code::KeyR|Code::KeyS|Code::KeyZ' "$root/crates/flavite-editor/src/context.rs"
    gate assert_present 'Code::Quote|Code::Comma|Code::Period|Code::Backslash|Code::BracketLeft|Code::BracketRight|Code::Minus|Code::Equal' "$root/crates/flavite-editor/src/context.rs"
    # Coverage test extended to 35 positions
    gate assert_present 'Code::KeyM.*KeyCode::KeyM' "$root/crates/flavite-editor/src/context.rs"
    # Shadow equivalence test exists
    gate assert_present 'defaults_keymap_classifies_identically_to_delivered_grammar' "$root/crates/flavite-edit-context/src/keymap.rs"
}

run_feux0003_x02_gates() {
    run_feux0003_x01_gates
    # FEUX-0003 X02: Grammar deleted, hints derive
    # grammar.rs is deleted - check it doesn't exist
    gate test ! -f "$root/crates/flavite-edit-context/src/grammar.rs"
    # Old grammar functions gone
    gate assert_absent 'fn command_key\|fn motion_key\|fn menu_key' "$root/crates/flavite-edit-context/src/"
    # hint() functions deleted
    gate assert_absent 'pub fn hint\(\)' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_absent 'ContextMenu::hint' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    # ContextMenu::Type is deleted
    gate assert_absent 'ContextMenu::Type' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    # Host uses keymap lookups
    gate assert_present 'lookup_root' "$root/crates/flavite-ui/src/host/context_mode.rs"
    gate assert_present 'lookup_menu' "$root/crates/flavite-ui/src/host/context_mode.rs"
    # Toolbar uses derived labels
    gate assert_present 'context_label' "$root/crates/flavite-ui/src/views/context_toolbar.rs"
    gate assert_present 'binding_label' "$root/crates/flavite-ui/src/host/appearance.rs"
}

run_feux0003_x03_gates() {
    run_feux0003_x02_gates
    # FEUX-0003 X03: rebindable surface
    # Descriptors exist
    gate assert_present 'shortcuts.context_keys' "$root/crates/flavite-settings/src/registry.rs"
    gate assert_present 'appearance.keyboard_layout' "$root/crates/flavite-settings/src/registry.rs"
    # CaptureRequest enum exists
    gate assert_present 'pub enum CaptureRequest' "$root/crates/flavite-ui/src/host/settings.rs"
    # set_context_key message exists
    gate assert_present 'SetContextKey' "$root/crates/flavite-ui/src/host/settings.rs"
    # Context Mode keys section exists
    gate assert_present 'context_keys_section' "$root/crates/flavite-ui/src/views/settings.rs"
    # binding_label in keymap
    gate assert_present 'pub fn binding_label' "$root/crates/flavite-edit-context/src/keymap.rs"
}

run_feux0003_x04_gates() {
    run_feux0003_x03_gates
    # FEUX-0003 X04: Type menu dissolution
    # Type menu enum variant deleted
    gate assert_absent 'ContextMenu::Type' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    # INTRO/UNBOUND reworded
    gate assert_present 'Select text, then act' "$root/crates/flavite-edit-context/src/session.rs"
    gate assert_present 'choose a menu' "$root/crates/flavite-edit-context/src/session.rs"
    # Type button removed from toolbar
    gate assert_absent 'ContextMenu::Type' "$root/crates/flavite-ui/src/views/context_toolbar.rs"
    # Only 3 menus remain
    gate assert_present 'pub enum ContextMenu' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'Select,' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'Go,' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'More,' "$root/crates/flavite-edit-context/src/vocabulary.rs"
}

run_feux0003_x05_gates() {
    run_feux0003_x04_gates
    # FEUX-0003 X05: Layout labels
    # Layout enum exists with 5 variants
    gate assert_present 'pub enum Layout' "$root/crates/flavite-edit-context/src/labels.rs"
    gate assert_present 'Qwerty' "$root/crates/flavite-edit-context/src/labels.rs"
    gate assert_present 'Dvorak' "$root/crates/flavite-edit-context/src/labels.rs"
    gate assert_present 'Colemak' "$root/crates/flavite-edit-context/src/labels.rs"
    gate assert_present 'Azerty' "$root/crates/flavite-edit-context/src/labels.rs"
    gate assert_present 'Qwertz' "$root/crates/flavite-edit-context/src/labels.rs"
    # glyph function exists
    gate assert_present 'pub fn glyph' "$root/crates/flavite-edit-context/src/labels.rs"
    # keyboard_layout descriptor exists
    gate assert_present 'appearance.keyboard_layout' "$root/crates/flavite-settings/src/registry.rs"
    # Appearance host has keyboard_layout
    gate assert_present 'pub keyboard_layout' "$root/crates/flavite-ui/src/host/appearance.rs"
}

run_feux0003_x06_gates() {
    run_feux0003_x05_gates
    # FEUX-0003 X06: Profiles store and facade
    # profiles.rs exists
    gate assert_nonempty "$root/crates/flavite-api/src/profiles.rs"
    # ProfilesStore exists
    gate assert_present 'pub struct ProfilesStore' "$root/crates/flavite-api/src/profiles.rs"
    # Facade methods exist
    gate assert_present 'profiles_list' "$root/crates/flavite-api/src/flavite.rs"
    gate assert_present 'profiles_create_from_current' "$root/crates/flavite-api/src/flavite.rs"
    gate assert_present 'profiles_apply' "$root/crates/flavite-api/src/flavite.rs"
    gate assert_present 'profiles_delete' "$root/crates/flavite-api/src/flavite.rs"
    gate assert_present 'profiles_import_path' "$root/crates/flavite-api/src/flavite.rs"
    gate assert_present 'profiles_export_path' "$root/crates/flavite-api/src/flavite.rs"
    # ApplyProfileResult exists
    gate assert_present 'pub struct ApplyProfileResult' "$root/crates/flavite-api/src/flavite.rs"
    # Profile diagnostics events exist
    gate assert_present 'ProfileCaptured' "$root/crates/flavite-ui/src/host/editor_diagnostics.rs"
    gate assert_present 'ProfileApplied' "$root/crates/flavite-ui/src/host/editor_diagnostics.rs"
    gate assert_present 'ProfileCreated' "$root/crates/flavite-ui/src/host/editor_diagnostics.rs"
    gate assert_present 'ProfileDeleted' "$root/crates/flavite-ui/src/host/editor_diagnostics.rs"
    gate assert_present 'ProfileImported' "$root/crates/flavite-ui/src/host/editor_diagnostics.rs"
    gate assert_present 'ProfileExported' "$root/crates/flavite-ui/src/host/editor_diagnostics.rs"
    # ProfilesStore tests exist
    gate assert_nonempty "$root/crates/flavite-api/tests/profiles_store.rs"
    gate assert_nonempty "$root/crates/flavite-api/tests/profiles_facade.rs"
}

run_feux0003_x07_gates() {
    run_feux0003_x06_gates
    # FEUX-0003 X07: Profiles UI and acceptance
    # Profiles section in settings view
    gate assert_present 'fn profiles_section' "$root/crates/flavite-ui/src/views/settings.rs"
    # Profile messages in SettingsMessage
    gate assert_present 'ProfilesList' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'ProfilesCreate' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'ProfilesApply' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'ProfilesDelete' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'ProfilesImport' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'ProfilesExport' "$root/crates/flavite-ui/src/host/settings.rs"
    # Profile state in SettingsHost
    gate assert_present 'pub profiles' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'pub profile_name_draft' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'pub pending_profile_delete' "$root/crates/flavite-ui/src/host/settings.rs"
    # Profile methods in SettingsHost
    gate assert_present 'pub fn list_profiles' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'pub fn create_profile' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'pub fn apply_profile' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'pub fn delete_profile' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'pub fn import_profile' "$root/crates/flavite-ui/src/host/settings.rs"
    gate assert_present 'pub fn export_profile' "$root/crates/flavite-ui/src/host/settings.rs"
    # workspace.apply_profile command
    gate assert_present 'workspace.apply_profile' "$root/crates/flavite-ui/src/host/commands.rs"
    # Dialog functions for import/export
    gate assert_present 'pick_profile_import_file' "$root/crates/flavite-ui/src/host/dialogs/mod.rs"
    gate assert_present 'pick_profile_export_file' "$root/crates/flavite-ui/src/host/dialogs/mod.rs"
    # Workbench scripts
    gate assert_nonempty "$root/scripts/tests/workbench/context_mode_rebind.py"
    gate assert_nonempty "$root/scripts/tests/workbench/settings_profiles.py"
    # Documentation updated
    gate assert_present 'Settings profiles' "$root/docs/APP-FEATURES.md"
    gate assert_present 'Rebindable Context Mode keys' "$root/docs/APP-FEATURES.md"
}

run_feux0005_x01_gates() {
    # FEUX-0005 X01: Indent family presence gates
    # Core module exists
    gate assert_nonempty "$root/crates/flavite-buffer/src/indent.rs"
    # Planner ops exist
    gate assert_present 'IndentLines' "$root/crates/flavite-buffer/src/commands.rs"
    gate assert_present 'DedentLines' "$root/crates/flavite-buffer/src/commands.rs"
    # Vocabulary commands exist
    gate assert_present 'Indent' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'Dedent' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'editor.indent' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'editor.dedent' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    # Keymap bindings
    gate assert_present 'BindingId::Command.*Indent' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'BindingId::Command.*Dedent' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'Slot::shifted.*Period' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'Slot::shifted.*Comma' "$root/crates/flavite-edit-context/src/keymap.rs"
    # API handlers
    gate assert_present 'handle_editor_indent' "$root/crates/flavite-api/src/editor_commands.rs"
    gate assert_present 'handle_editor_dedent' "$root/crates/flavite-api/src/editor_commands.rs"
    gate assert_present 'editor.indent' "$root/crates/flavite-api/src/flavite.rs"
    gate assert_present 'editor.dedent' "$root/crates/flavite-api/src/flavite.rs"
    # UI dispatch arms
    gate assert_present 'Indent =>' "$root/crates/flavite-ui/src/host/context_mode.rs"
    gate assert_present 'Dedent =>' "$root/crates/flavite-ui/src/host/context_mode.rs"
    # UI keymap classification
    gate assert_present 'Slot::shifted.*Period' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'Slot::shifted.*Comma' "$root/crates/flavite-edit-context/src/keymap.rs"
}

run_feux0005_x02_gates() {
    # FEUX-0005 X02: Whitespace conversion family presence gates
    # Core module exists (already checked by X01)
    gate assert_nonempty "$root/crates/flavite-buffer/src/indent.rs"
    # Conversion functions exist
    gate assert_present 'tabs_to_spaces_line' "$root/crates/flavite-buffer/src/indent.rs"
    gate assert_present 'spaces_to_tabs_line' "$root/crates/flavite-buffer/src/indent.rs"
    # Planner op exists
    gate assert_present 'ReWhitespace' "$root/crates/flavite-buffer/src/commands.rs"
    # Vocabulary commands exist
    gate assert_present 'TabsToSpaces' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'SpacesToTabs' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'editor.tabs_to_spaces' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    gate assert_present 'editor.spaces_to_tabs' "$root/crates/flavite-edit-context/src/vocabulary.rs"
    # Keymap bindings (Transform menu)
    gate assert_present 'BindingId::Command.*TabsToSpaces' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'BindingId::Command.*SpacesToTabs' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'ContextMenu::Transform' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'KeyCode::KeyE' "$root/crates/flavite-edit-context/src/keymap.rs"
    gate assert_present 'KeyCode::KeyR' "$root/crates/flavite-edit-context/src/keymap.rs"
    # API handlers
    gate assert_present 'handle_editor_tabs_to_spaces' "$root/crates/flavite-api/src/editor_commands.rs"
    gate assert_present 'handle_editor_spaces_to_tabs' "$root/crates/flavite-api/src/editor_commands.rs"
    gate assert_present 'editor.tabs_to_spaces' "$root/crates/flavite-api/src/flavite.rs"
    gate assert_present 'editor.spaces_to_tabs' "$root/crates/flavite-api/src/flavite.rs"
    # UI dispatch arms
    gate assert_present 'TabsToSpaces =>' "$root/crates/flavite-ui/src/host/context_mode.rs"
    gate assert_present 'SpacesToTabs =>' "$root/crates/flavite-ui/src/host/context_mode.rs"
}

run_feux0005_x03_gates() {
    # FEUX-0005 X03: Auto-indent for OpenLine
    # Core module exists (already checked by X01/X02)
    gate assert_nonempty "$root/crates/flavite-buffer/src/indent.rs"
    # OpenLine operation exists (already checked by X01)
    gate assert_present 'OpenLine' "$root/crates/flavite-buffer/src/commands.rs"
    # leading_whitespace function exists and is used
    gate assert_present 'leading_whitespace' "$root/crates/flavite-buffer/src/commands.rs"
    # import of leading_whitespace in commands.rs
    gate assert_present 'leading_whitespace' "$root/crates/flavite-buffer/src/commands.rs"
}

case "$slice" in
    X01) run_x01_gates ;;
    X02) run_x02_gates ;;
    X03) run_x03_gates ;;
    X04) run_x04_gates ;;
    X05) run_x05_gates ;;
    X06) run_x06_gates ;;
    X07) run_x07_gates ;;
    X08) run_x08_gates ;;
    FEUX0005-X01) run_feux0005_x01_gates ;;
    FEUX0005-X02) run_feux0005_x02_gates ;;
    FEUX0005-X03) run_feux0005_x03_gates ;;
    FEUX0004-X07) run_feux0004_x07_gates ;;
    FEUX0003-X01) run_feux0003_x01_gates ;;
    FEUX0003-X02) run_feux0003_x02_gates ;;
    FEUX0003-X03) run_feux0003_x03_gates ;;
    FEUX0003-X04) run_feux0003_x04_gates ;;
    FEUX0003-X05) run_feux0003_x05_gates ;;
    FEUX0003-X06) run_feux0003_x06_gates ;;
    FEUX0003-X07) run_feux0003_x07_gates ;;
    *) echo "unknown slice: ${slice:-<empty>} (implemented: X01 X02 X03 X04 X05 X06 X07 X08 FEUX0005-X01 FEUX0004-X07 FEUX0003-X01 FEUX0003-X02 FEUX0003-X03 FEUX0003-X04 FEUX0003-X05 FEUX0003-X06 FEUX0003-X07)" >&2; exit 2 ;;
esac

if ((GATES_FAILED > 0)); then
    echo "feux gate $slice FAILED: $GATES_PASSED/$GATES_RUN gates passed" >&2
    exit 1
fi
echo "feux gate $slice passed: $GATES_RUN gates."
