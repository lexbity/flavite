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
    *)
        echo "unknown workspace contract profile: ${profile:-<empty>}" >&2
        exit 2
        ;;
esac
echo "workspace contract profile $profile passed"
