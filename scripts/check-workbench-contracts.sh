#!/usr/bin/env bash
# RFC-0006 §11 workbench source gates (RFC-0006-knowledge-workbench.md).
#
# Usage: check-workbench-contracts.sh <S00|S01|...> [repo-root]
#
# The slice argument names the highest completed slice; every gate from S00
# through that slice runs (cumulative — skipping completed earlier gates is
# forbidden) and future slice gates are never required early. Presence gates
# demand nonempty files: a missing source fails the gate
# (error-on-missing-source semantics), and a textual absence gate succeeds
# only on rg exit 1, never exit 2. The second argument exists for negative
# fixtures (xtask/tests/workbench_gates.rs points it at a seeded tree).
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

# Counted gate invocation: record the outcome and keep scanning so one run
# reports every failing gate, not only the first.
gate() {
    GATES_RUN=$((GATES_RUN + 1))
    if "$@"; then
        GATES_PASSED=$((GATES_PASSED + 1))
    else
        GATES_FAILED=$((GATES_FAILED + 1))
    fi
}

run_s00_gates() {
    # Completed codec/lifecycle files only — future matcher crates are not
    # required here (§11: "S00 gates require only its completed codec/lifecycle
    # files, not future matcher crates"). Absent files fail; no skip logic.
    local file
    for file in \
        crates/flavite-encoding/src/lib.rs \
        crates/flavite-encoding/src/format.rs \
        crates/flavite-encoding/src/decode.rs \
        crates/flavite-encoding/src/encode.rs \
        crates/flavite-encoding/src/map.rs \
        crates/flavite-encoding/src/stream.rs \
        crates/flavite-encoding/tests/roundtrip.rs \
        crates/flavite-encoding/tests/mapping_properties.rs \
        crates/flavite-syntax/tests/encoding_maps.rs \
        crates/flavite-buffer/tests/encoding_roundtrip.rs \
        crates/flavite-vault/tests/encoding_workflow.rs \
        crates/flavite-vault/tests/encoding_recovery.rs \
        crates/flavite-index/tests/encoding_ingest.rs \
        crates/flavite-api/tests/note_encoding.rs \
        crates/flavite-ui/tests/file_encoding.rs \
        crates/flavite-cli/tests/file_encoding.rs \
        crates/flavite-vault/src/encoding.rs \
        scripts/lib/source-gates.sh \
        xtask/tests/workbench_gates.rs \
        xtask/tests/workbench_ownership.rs; do
        gate assert_nonempty "$root/$file"
    done
    # The codec stays toolkit- and product-crate-free at the source level.
    gate assert_absent \
        '^use (flavite_|iced|cosmic_text|ropey|rusqlite|regex)|^extern crate (flavite_|iced)' \
        "$root/crates/flavite-encoding/src"
    gate assert_no_dep_named \
        "$root/crates/flavite-encoding/Cargo.toml" \
        'flavite-|iced|cosmic-text|ropey|rusqlite|regex|encoding_rs|chardet'
    # Strict decoding: no IO, no lossy UTF fallbacks inside the codec.
    gate assert_absent \
        'std::fs|std::path|std::process|from_utf8_lossy|from_utf16_lossy' \
        "$root/crates/flavite-encoding/src"
    # The buffer keeps its own strict UTF-8 validation; no lossy BOM flag.
    gate assert_absent 'pub has_bom: bool' "$root/crates/flavite-buffer/src/eol.rs"
    gate assert_absent 'Invalid UTF-8, create empty buffer' \
        "$root/crates/flavite-buffer/src/manager.rs"
    # The syntax layer must not rebuild encoded offsets by arithmetic.
    gate assert_absent \
        'source_offset = line\.source_start\.0 \+ \(text\.0 - line\.text_start\.0\)' \
        "$root/crates/flavite-syntax/src/text.rs"
    # The editor facade holds no direct encoding dependency; the dependency
    # tables are parsed so the package's own name cannot falsely trip it.
    gate assert_no_dep_named \
        "$root/crates/flavite-editor/Cargo.toml" \
        'flavite-encoding'
}

run_s01_gates() {
    # S01 presence: the isolated matcher helper plus its integration seams.
    local file
    for file in \
        crates/flavite-text-search/src/lib.rs \
        crates/flavite-text-search/src/matcher.rs \
        crates/flavite-text-search/src/template.rs \
        crates/flavite-text-search/tests/literal_properties.rs \
        crates/flavite-vault/src/text_edit.rs \
        crates/flavite-api/src/text_edit.rs \
        crates/flavite-ui/src/host/note_search.rs \
        crates/flavite-buffer/tests/prepared_edits.rs \
        crates/flavite-api/tests/note_replace.rs \
        crates/flavite-ui/tests/note_search.rs; do
        gate assert_nonempty "$root/$file"
    done
    # The reading host keeps no private matcher; note search never scans raw
    # text by hand.
    gate assert_absent 'fn find_matches|struct FindState' \
        "$root/crates/flavite-ui/src/host/reading.rs"
    gate assert_absent 'match_indices\(' \
        "$root/crates/flavite-ui/src/views/note.rs" \
        "$root/crates/flavite-ui/src/host/note_search.rs"
    # The helper stays toolkit-free and below the product crates.
    gate assert_no_dep_named \
        "$root/crates/flavite-text-search/Cargo.toml" \
        'iced|cosmic-text|rusqlite|flavite-(api|buffer|editor|vault|ui|index|fs|workspace)'
    gate assert_no_dep_named "$root/crates/flavite-editor/Cargo.toml" 'flavite-text-search'
    gate assert_no_dep_named "$root/crates/flavite-buffer/Cargo.toml" 'flavite-text-search'
    gate assert_no_dep_named "$root/crates/flavite-syntax/Cargo.toml" 'flavite-text-search'
    gate assert_no_dep_named "$root/crates/flavite-index/Cargo.toml" 'flavite-text-search'
    # Position/edit mapping stays in buffer+api; the helper exposes no types.
    gate assert_absent \
        'pub (struct|enum) (SourceEdit|SourcePatch|TextEdit|EditOp|Selection|Cursor)\b' \
        "$root/crates/flavite-text-search/src"
    gate assert_absent \
        'fn map_through_(edits|replacement)|fn map_selection_through_edits' \
        "$root/crates/flavite-text-search/src"
    gate assert_absent 'std::fs|std::thread::sleep|\.wait_for_operation\(' \
        "$root/crates/flavite-ui/src/host/note_search.rs"
}

run_s03_gates() {
    # S03 presence: the helper crate, protocol, worker seam and the test
    # surfaces that prove isolation (§6.2/D-14).
    local file
    for file in \
        crates/flavite-text-search/src/protocol.rs \
        crates/flavite-text-search-worker/Cargo.toml \
        crates/flavite-text-search-worker/src/main.rs \
        crates/flavite-text-search/tests/regex_properties.rs \
        crates/flavite-vault/src/text_worker.rs \
        crates/flavite-vault/tests/text_worker.rs \
        crates/flavite-ui/tests/note_regex.rs \
        crates/flavite-api/tests/note_regex.rs \
        fuzz/fuzz_targets/text_match.rs \
        fuzz/fuzz_targets/text_worker_protocol.rs; do
        gate assert_nonempty "$root/$file"
    done
    # The helper wraps only the matcher: no product crates, no toolkit.
    gate assert_no_dep_named \
        "$root/crates/flavite-text-search-worker/Cargo.toml" \
        'flavite-(vault|api|index|ui|buffer|editor|fs|workspace|syntax)|iced|cosmic-text|rusqlite'
    # The parent never reaches for a shell or an overridable worker path
    # (§6.2: sibling-only discovery, no note/preference/env override).
    gate assert_absent 'Command::new\("(sh|bash|python|python3)"\)|env::var\(".*WORKER.*PATH' \
        "$root/crates/flavite-vault/src/text_worker.rs"
    # The UI never compiles or splits patterns itself: mode/options ride
    # the typed API, matches come back revision-bound (TD-1/TD-2 discipline).
    gate assert_absent 'Regex::new|RegexBuilder|split_whitespace' \
        "$root/crates/flavite-ui/src/host/note_search.rs" \
        "$root/crates/flavite-ui/src/views/note.rs"
    # The helper binary declares no filesystem/network surface of its own.
    gate assert_absent 'std::fs|std::net|TcpStream|UdpSocket|File::open|fs::write' \
        "$root/crates/flavite-text-search-worker/src/main.rs"
}

run_s04_gates() {
    # S04 presence: the typed open-intent policy, the reworked gesture
    # arbiter, the workbench-loop evidence, and the native runner pieces.
    local file
    for file in \
        crates/flavite-ui/src/host/open_intents.rs \
        crates/flavite-ui/tests/open_intents.rs \
        crates/flavite-ui/tests/workbench_loop.rs \
        scripts/workbench-visual-linux.sh \
        scripts/tests/workbench/common.py \
        scripts/tests/workbench/graph_editor_loop.py; do
        gate assert_nonempty "$root/$file"
    done
    # D-02: activation is the pair outcome; explicit Insert is a distinct
    # command path — the old implicit-insert gesture outcome stays dead.
    gate assert_absent 'GestureOutcome::InsertLink' \
        "$root/crates/flavite-ui/src/graph/events.rs" \
        "$root/crates/flavite-ui/src/host/graph.rs"
    # D-03: one typed intent controls every navigation surface; the global
    # first-map-match open is deleted, and graph commands never reach for
    # an accidental global destination (the focused group).
    gate assert_absent 'fn request_open\(' "$root/crates/flavite-ui/src/app.rs"
    gate assert_absent 'app\.focused_group\(' "$root/crates/flavite-ui/src/host/graph.rs"
    # D-02: the retired implicit-insert gesture copy must not be re-adopted
    # by the graph chrome (banner/list hints), and deferred non-note
    # activation ("arrives with W09") is replaced by the explicit
    # inspector and Create-or-Locate intents.
    gate assert_absent \
        'double-click a note node to insert|double-click or Insert places' \
        "$root/crates/flavite-ui/src/views/graph.rs"
    gate assert_absent 'arrives with W09|arrives with the reading slice' \
        "$root/crates/flavite-ui/src/host/graph.rs" \
        "$root/crates/flavite-ui/src/views/graph.rs"
}

run_s05_gates() {
    # S05 rich catalog, shared editor candidates, and exact asynchronous
    # resolver verdicts. Completion data enters through the ticketed API;
    # hosts must not revive either synchronous index-backed read.
    local file
    for file in \
        crates/flavite-index/src/pool.rs \
        crates/flavite-index/src/types.rs \
        crates/flavite-api/src/queries.rs \
        crates/flavite-api/src/query_service.rs \
        crates/flavite-api/src/flavite.rs \
        crates/flavite-api/src/lib.rs \
        crates/flavite-vault/src/actor.rs \
        crates/flavite-vault/src/session.rs \
        crates/flavite-api/tests/catalog_tickets.rs \
        crates/flavite-editor/src/links.rs \
        crates/flavite-editor/src/widget.rs \
        crates/flavite-ui/src/app.rs \
        crates/flavite-ui/src/views/note.rs \
        crates/flavite-ui/tests/catalog_acquisition.rs \
        crates/flavite-api/src/device_preferences.rs; do
        gate assert_nonempty "$root/$file"
    done
    gate assert_absent '\.completion_candidates\(|\.note_catalog\(' \
        "$root/crates/flavite-ui/src"
    gate assert_absent 'pub fn with_candidates\([^)]*Vec' \
        "$root/crates/flavite-editor/src/widget.rs"
    gate assert_absent 'pub use crate::queries::NoteCatalogEntry as CatalogEntry' \
        "$root/crates/flavite-api/src/query_service.rs"
    gate assert_absent ' as (BufferStatus|IndexCursor|IndexNoteRef|CatalogEntry)' \
        "$root/crates/flavite-api/src/lib.rs" \
        "$root/crates/flavite-api/src/query_service.rs"
    gate assert_absent 'pub fn note_catalog\(' \
        "$root/crates/flavite-api/src/queries.rs"
    gate assert_absent 'pub fn completion_candidates|CompletionCandidates' \
        "$root/crates/flavite-index/src/pool.rs" \
        "$root/crates/flavite-index/src/types.rs" \
        "$root/crates/flavite-api/src/flavite.rs" \
        "$root/crates/flavite-api/src/lib.rs" \
        "$root/crates/flavite-vault/src/actor.rs" \
        "$root/crates/flavite-vault/src/session.rs"
    gate assert_absent 'legacy dotted alias|pre-date the naming cleanup|Both ids resolve' \
        "$root/crates/flavite-api/src/flavite.rs"
    gate assert_absent 'Message::Find(Open|Close|Query|Next)|^[[:space:]]*Find(Open|Close|Query|Next)\(' \
        "$root/crates/flavite-ui/src/app.rs"
}


run_s06_gates() {
    # S06 presence: the ordered writer owner, the complete evaluator, the
    # capture protocol, the materialized session service, the search host,
    # and the proof surfaces (RFC-0006 S06 table).
    local file
    for file in \
        crates/flavite-index/src/search_eval.rs \
        crates/flavite-index/src/search_session.rs \
        crates/flavite-index/src/search_writer.rs \
        crates/flavite-api/src/search_sessions.rs \
        crates/flavite-ui/src/host/search.rs \
        crates/flavite-index/tests/search_boolean.rs \
        crates/flavite-index/tests/search_writer.rs \
        crates/flavite-api/tests/search_sessions.rs \
        crates/flavite-ui/tests/search_paging.rs \
        crates/flavite-cli/tests/search_modes.rs; do
        gate assert_nonempty "$root/$file"
    done
    # S06 source isolation gates (exact required invocations, §12).
    gate assert_absent 'trimmed\.split_whitespace\(' \
        "$root/crates/flavite-api/src/query_service.rs" \
        "$root/crates/flavite-api/src/search_sessions.rs"
    gate assert_absent 'fn load_more|MAX_PAGE_LIMIT|page_limit.*SEARCH_PAGE' \
        "$root/crates/flavite-ui/src/views/search.rs" \
        "$root/crates/flavite-ui/src/host/search.rs"
    gate assert_absent 'Self::Text\(_query\)' "$root/crates/flavite-index/src/query.rs"
    gate assert_absent 'SearchState' "$root/crates/flavite-ui/src/views/search.rs"
    gate assert_absent 'fn search_notes_with_filter_limit' "$root/crates/flavite-index/src/pool.rs"
    # The old synchronous commit-before-search contract is deleted from
    # production search paths (D-07): no commit inside search_hits/searcher.
    gate assert_absent 'commit_before_search|if self\.is_dirty\(\) \{\s*$' \
        "$root/crates/flavite-index/src/search.rs"
    # The index keeps no synchronous IndexWriter: writes belong to the one
    # ordered writer thread, and the deferred-commit timer is gone.
    gate assert_absent 'writer: Mutex<IndexWriter>|fn commit_search_if_due' \
        "$root/crates/flavite-index/src/search.rs" \
        "$root/crates/flavite-index/src/pool.rs" \
        "$root/crates/flavite-vault/src/actor.rs"
    # The legacy And-only hybrid extraction and the giant-filter fallback
    # are deleted with search_notes_with_filter_limit.
    gate assert_absent 'MAX_TANTIVY_FILTER_SIZE' "$root/crates/flavite-index/src"
}

case "$slice" in
    S00) run_s00_gates ;;
    S01) run_s00_gates; run_s01_gates ;;
    S03) run_s00_gates; run_s01_gates; run_s03_gates ;;
    S04) run_s00_gates; run_s01_gates; run_s03_gates; run_s04_gates ;;
    S05) run_s00_gates; run_s01_gates; run_s03_gates; run_s04_gates; run_s05_gates ;;
    S06) run_s00_gates; run_s01_gates; run_s03_gates; run_s04_gates; run_s05_gates; run_s06_gates ;;
    *) echo "unknown slice: ${slice:-<empty>} (implemented: S00 S01 S03 S04 S05 S06)" >&2; exit 2 ;;
esac

if ((GATES_FAILED > 0)); then
    echo "workbench gate $slice FAILED: $GATES_PASSED/$GATES_RUN gates passed" >&2
    exit 1
fi
echo "workbench gate $slice passed: $GATES_RUN gates."
