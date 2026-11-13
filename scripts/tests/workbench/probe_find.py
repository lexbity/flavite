"""Temporary probe: does the find-bar input accept typed text natively?"""
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

    common.key("slash")
    wait(lambda s: any(t.get("find") for t in common.tabs(s)), "Find prompt open")
    # Click the find input region (same geometry the search row uses).
    common.point(window, harness, 212, 172)
    common.run("xdotool", "click", "1")
    time.sleep(0.3)
    common.type_text("green")
    deadline = time.monotonic() + 6
    ok = False
    while time.monotonic() < deadline:
        value = common.snapshot(harness)
        query = next((t.get("find", {}).get("query") for t in common.tabs(value) if t.get("find")), None)
        print(f"PROBE find query={query!r} mode={(value.get('context_mode') or {}).get('state')}")
        if query == "green":
            ok = True
            break
        time.sleep(0.2)
    assert ok, "find input did not accept typed text"
    common.finish(output, [{"id": "probe-find", "outcome": "pass"}], backend)


if __name__ == "__main__":
    main()
