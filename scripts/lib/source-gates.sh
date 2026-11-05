#!/usr/bin/env bash
# RFC-0006 §11 source gates, defined exactly once and sourced (never
# executed) by every workbench checker — scripts/check-workbench-contracts.sh
# today; the existing workspace checker imports them when migrated with
# equivalent negative fixtures.
#
# Contract (§11): a textual absence gate succeeds only on rg exit 1, never
# exit 2 — a missing search target is a failure, not a pass — and presence
# gates demand nonempty files (error-on-missing-source semantics).

assert_absent() {
    local pattern="$1"; shift
    local status=0
    rg -n -- "$pattern" "$@" || status=$?
    case "$status" in
        1) return 0 ;;
        0) echo "forbidden production pattern: $pattern" >&2; return 1 ;;
        *) echo "search failed ($status): $pattern" >&2; return "$status" ;;
    esac
}

assert_nonempty() { test -s "$1" || { echo "missing/empty $1" >&2; return 1; }; }

# Dependency-table gate: no dependency table in the manifest may declare a
# key matching the extended regex. Scans [dependencies], [dev-dependencies],
# [build-dependencies], their [dependencies.<name>] subtable forms and their
# target-specific forms ([target.'cfg(...)'.(dev-|build-)?dependencies]).
# The package's own `name = "..."` under [package] is not a dependency-table
# key, so a crate is never falsely rejected for its own name (§11 S00 note).
assert_no_dep_named() {
    local manifest="$1"
    local forbidden="$2"
    if [[ ! -f "$manifest" ]]; then
        echo "dependency gate: manifest not found: $manifest" >&2
        return 1
    fi
    local out
    local status=0
    out=$(awk -v forbidden="$forbidden" '
        /^[[:space:]]*#/ { next }
        /^[[:space:]]*\[+/ {
            sec = $0
            sub(/^[[:space:]]*\[+/, "", sec)
            sub(/\]+[[:space:]]*(#.*)?$/, "", sec)
            gsub(/[[:space:]]/, "", sec)
            # The [dependencies.<name>] subtable form declares the dependency
            # in the table header rather than in a key line.
            header_name = ""
            if (sec ~ /^(dev-|build-)?dependencies($|\.)/) {
                header_name = sec
                sub(/^(dev-|build-)?dependencies\.?/, "", header_name)
            } else if (sec ~ /\.(dev-|build-)?dependencies($|\.)/) {
                header_name = sec
                sub(/^.*\.(dev-|build-)?dependencies\.?/, "", header_name)
            }
            in_dep = (header_name != "") || \
                     (sec ~ /^(dev-|build-)?dependencies$/) || \
                     (sec ~ /\.(dev-|build-)?dependencies$/)
            if (header_name ~ /\./) { sub(/\..*$/, "", header_name) }
            if (header_name != "" && header_name ~ forbidden) {
                printf "forbidden dependency declared in %s: %s (matches /%s/)\n", FILENAME, header_name, forbidden
                fail = 1
            }
            next
        }
        in_dep && /^[[:space:]]*"?[A-Za-z0-9_.-]+"?[[:space:]]*=/ {
            key = $0
            sub(/^[[:space:]]*"?/, "", key)
            sub(/"?[[:space:]]*=.*$/, "", key)
            name = key
            sub(/\..*$/, "", name)
            if (name ~ forbidden) {
                printf "forbidden dependency declared in %s: %s (matches /%s/)\n", FILENAME, name, forbidden
                fail = 1
            }
        }
        END { exit fail ? 1 : 0 }
    ' "$manifest") || status=$?
    if [[ -n "$out" ]]; then
        printf '%s\n' "$out" >&2
    fi
    return "$status"
}
