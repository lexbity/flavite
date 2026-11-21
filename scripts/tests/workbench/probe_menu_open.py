"""Temporary probe: open selection row via the Select-menu row click, then
type — does the query accept text?"""
import time

import common


def main():
    window, harness, output, vault, backend = common.argv()
    wait = lambda predicate, label: common.wait(harness, predicate, label)

    common.key("ctrl+p")
    common.type_text("alpha")
    common.key("Return")
    wait(
        lambda s: any(t["title"] == "alpha" and t["load"] == "ready" and t["editor_bounds"] for t in common.tabs(s)),
        "source editor ready",
    )
    value = common.snapshot(harness)
    editor = next(t for t in common.tabs(value) if t["title"] == "alpha")
    common.click_rect(window, harness, editor["editor_bounds"])
    common.key("ctrl+space")
    wait(lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND", "command mode")

    # Open the Select menu and click the match-selection row.
    bounds = {}
    for _ in range(8):
        current = common.snapshot(harness)
        bounds = (current.get("context_mode") or {}).get("bounds") or {}
        if "Select matches…" in bounds:
            break
        common.key("v")
        time.sleep(0.1)
    assert "Select matches…" in bounds, "menu row missing"
    common.click_rect(window, harness, bounds["Select matches…"])
    time.sleep(0.6)
    common.type_text("TODO")
    deadline = time.monotonic() + 6
    ok = False
    while time.monotonic() < deadline:
        value = common.snapshot(harness)
        st = next((t.get("selection_search") for t in common.tabs(value) if t.get("selection_search")), None)
        print(f"PROBE kind={st and st.get('kind')} query={st and st.get('query')!r} mode={(value.get('context_mode') or {}).get('state')} overlay={st and st.get('overlay_matches')}")
        if st and st.get("query") == "TODO":
            ok = True
            break
        time.sleep(0.2)
    assert ok, "menu-row open did not accept typed query"
    common.finish(output, [{"id": "probe-menu-open", "outcome": "pass"}], backend)


if __name__ == "__main__":
    main()
