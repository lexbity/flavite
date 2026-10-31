#!/usr/bin/env bash
# Literal source gates for the workspace RFC. Missing inputs are failures.
set -euo pipefail

profile="${1:-}"
root="${2:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"

assert_absent() {
    local pattern="$1"
    shift
    local code=0
    rg -n -- "$pattern" "$@" || code=$?
    if [[ "$code" -eq 0 ]]; then
        echo "forbidden production pattern: $pattern" >&2
        return 1
    fi
    if [[ "$code" -ne 1 ]]; then
        echo "search failure, status $code, for pattern: $pattern" >&2
        return "$code"
    fi
}

assert_nonempty() {
    local file="$1"
    if [[ ! -s "$file" ]]; then
        echo "required workspace implementation is missing or empty: $file" >&2
        return 1
    fi
}

case "$profile" in
    W01)
        assert_nonempty "$root/crates/flavite-ui/src/accessibility/program.rs"
        assert_nonempty "$root/crates/flavite-ui/src/accessibility/tree.rs"
        assert_nonempty "$root/vendor/iced-accessibility/PATCHES.md"
        assert_nonempty "$root/vendor/iced-accessibility/iced_winit/src/lib.rs"
        assert_absent 'a11y_stub|fake_accessible_text|waived.*complete' \
            "$root/crates/flavite-ui/src/accessibility"
        ;;
    W02)
        # W01 gates remain in force additively; W02 adds the destination-shell
        # removal and the demo-target removal.
        bash "$0" W01 "$root"
        assert_nonempty "$root/crates/flavite-workspace/src/reducer.rs"
        assert_nonempty "$root/crates/flavite-workspace/src/geometry.rs"
        assert_nonempty "$root/crates/flavite-ui/src/workspace_shell.rs"
        assert_nonempty "$root/crates/flavite-ui/src/workspace/pane_grid.rs"
        assert_nonempty "$root/crates/flavite-ui/src/host/commands.rs"
        assert_nonempty "$root/scripts/workspace-visual-linux.sh"
        assert_absent 'enum Pane\b|Navigate\(Pane|Pane::NAV|Pane::Workspace|Pane::Editor|Pane::Read|Pane::Graph' \
            "$root/crates/flavite-ui/src"
        assert_absent 'demo\.md|LATTICE' \
            "$root/crates/flavite-ui/src/app.rs" \
            "$root/crates/flavite-ui/src/workspace_shell.rs"
        # The workspace model stays IO-free and toolkit-free.
        assert_absent 'iced|rusqlite|flavite-api|flavite-fs|flavite-ui|flavite-graph' \
            "$root/crates/flavite-workspace/Cargo.toml"
        ;;
    W03)
        # Completed layout and runtime gates remain additive.
        bash "$0" W02 "$root"
        for file in \
            crates/flavite-workspace/src/input.rs \
            crates/flavite-buffer/src/positions.rs \
            crates/flavite-editor/src/surface.rs \
            crates/flavite-editor/src/widget.rs \
            crates/flavite-api/src/flavite.rs \
            crates/flavite-ui/src/operation.rs \
            crates/flavite-ui/src/host/documents.rs \
            crates/flavite-ui/src/views/note.rs \
            crates/flavite-ui/tests/shared_views.rs; do
            assert_nonempty "$root/$file"
        done
        assert_absent 'active_buffer|\.session\(\)|execute_session\(' \
            "$root/crates/flavite-ui/src/app.rs" \
            "$root/crates/flavite-ui/src/host" \
            "$root/crates/flavite-ui/src/views" \
            "$root/crates/flavite-ui/src/operation.rs"
        assert_absent 'pub history: Option|pub navigation: Option|pub command_id:.*str' \
            "$root/crates/flavite-editor/src/surface.rs"
        assert_absent 'state == "|state != "' \
            "$root/crates/flavite-ui/src/views" \
            "$root/crates/flavite-editor/src/widget.rs"
        assert_absent 'pub fn session\(|pub fn execute_session\(' \
            "$root/crates/flavite-api/src/flavite.rs"
        ;;
    W04)
        # W03 gates remain additive; W04 adds the context-routing contract.
        bash "$0" W03 "$root"
        for file in \
            crates/flavite-workspace/src/context.rs \
            crates/flavite-api/src/query_service.rs \
            crates/flavite-ui/src/host/context.rs \
            crates/flavite-ui/src/views/context_header.rs \
            crates/flavite-ui/src/views/preview.rs \
            crates/flavite-ui/src/views/backlinks.rs \
            crates/flavite-workspace/tests/context_properties.rs \
            crates/flavite-ui/tests/context_routing.rs; do
            assert_nonempty "$root/$file"
        done
        assert_absent 'active_buffer|nearest_editor|last_pane' \
            "$root/crates/flavite-ui/src/host/context.rs" \
            "$root/crates/flavite-ui/src/views"
        assert_absent 'rusqlite|flavite_index|flavite_vault' \
            "$root/crates/flavite-ui/src/views"
        ;;
    W05)
        # W04 gates remain additive; W05 adds the semantic insertion contract.
        bash "$0" W04 "$root"
        for file in \
            crates/flavite-syntax/src/link_format.rs \
            crates/flavite-vault/src/link.rs \
            crates/flavite-api/src/links.rs \
            crates/flavite-ui/src/host/linking.rs \
            crates/flavite-api/tests/semantic_links.rs \
            crates/flavite-syntax/tests/link_insert_properties.rs \
            crates/flavite-ui/tests/link_target.rs \
            fuzz/fuzz_targets/link_insertion.rs; do
            assert_nonempty "$root/$file"
        done
        assert_absent 'link\.insert.*handle_edits|command_id: "link\.insert".*ops' \
            "$root/crates/flavite-api/src/flavite.rs" \
            "$root/crates/flavite-editor/src/surface.rs"
        assert_absent 'to_string_lossy\(\).*target|unwrap_or\(.*text\.len' \
            "$root/crates/flavite-ui/src/host/linking.rs" \
            "$root/crates/flavite-api/src/links.rs"
        ;;
    W06)
        # W05 gates remain additive; W06 adds the persistence contract.
        bash "$0" W05 "$root"
        for file in \
            crates/flavite-workspace/src/codec.rs \
            crates/flavite-workspace/src/migrate.rs \
            crates/flavite-workspace/src/restore.rs \
            crates/flavite-api/src/user_state.rs \
            crates/flavite-vault/src/user_state.rs \
            crates/flavite-ui/src/host/persistence.rs \
            crates/flavite-workspace/tests/workspace_codec.rs \
            crates/flavite-api/tests/workspace_storage.rs \
            crates/flavite-ui/tests/workspace_restore.rs \
            fuzz/fuzz_targets/workspace_restore.rs; do
            assert_nonempty "$root/$file"
        done
        # The UI persistence host and the workspace model stay IO-free:
        # storage goes through the injected Fs behind the facade.
        assert_absent 'std::fs|File::create|OpenOptions|fs::write|fs::rename' \
            "$root/crates/flavite-ui/src/host/persistence.rs" \
            "$root/crates/flavite-workspace/src"
        ;;
    *)
        echo "unknown workspace contract profile: ${profile:-<empty>}" >&2
        exit 2
        ;;
esac
echo "workspace contract profile $profile passed"
