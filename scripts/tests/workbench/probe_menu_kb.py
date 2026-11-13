"""Temporary probe: keyboard Select-menu navigation → Add caret on row below."""
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
    common.key("ctrl+Home")
    common.key("ctrl+space")
    wait(lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND", "command mode")

    common.key("v")
    time.sleep(0.5)
    value = common.snapshot(harness)
    print(f"PROBE after v: menu={value.get('context_mode', {}).get('menu')} msg={value.get('context_mode', {}).get('message')}")
    for i in range(10):
        common.key("Down")
        time.sleep(0.1)
        value = common.snapshot(harness)
        print(f"PROBE down {i}: menu={value.get('context_mode', {}).get('menu')} msg={value.get('context_mode', {}).get('message')}")
    common.key("Return")
    time.sleep(0.5)
    value = common.snapshot(harness)
    print(f"PROBE enter: menu={value.get('context_mode', {}).get('menu')} msg={value.get('context_mode', {}).get('message')} sel={selections(value)}")
    common.finish(output, [{"id": "probe-menu-kb", "outcome": "pass"}], backend)


def selections(s):
    editor = next(t for t in common.tabs(s) if t.get("selection_count") is not None)
    return editor["selection_count"]


if __name__ == "__main__":
    main()
