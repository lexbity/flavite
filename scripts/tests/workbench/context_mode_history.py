"""Real history chip/branch activation; no state injection or edit bypass."""
import common
from context_mode import painted


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)
    history = lambda s: (s.get("context_mode") or {}).get("history", {})
    common.key("ctrl+p")
    common.type_text("alpha")
    common.key("Return")
    value = wait(lambda s: any(t["title"] == "alpha" and t["editor_bounds"] for t in common.tabs(s)), "note opened")
    editor = next(t for t in common.tabs(value) if t["title"] == "alpha")
    common.click_rect(window, harness, editor["editor_bounds"])
    original = value["focused_text"]
    common.key("ctrl+End")
    common.key("ctrl+space")
    wait(lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND", "command state")
    common.key("i")
    common.type_text(" first")
    common.key("Escape")
    first = original + " first"
    value = wait(lambda s: s.get("focused_text") == first and "0" in history(s).get("bounds", {}), "initial chip ready")
    current = history(value)["page"]["current"]
    first_state = current
    common.click_rect(window, harness, history(value)["bounds"]["0"])
    value = wait(lambda s: s.get("focused_text") == original and history(s).get("page", {}).get("current") == 0, "chip restores initial state")
    source = next(t for t in common.tabs(value) if t["title"] == "alpha")
    assert source["selections"][source.get("primary", 0)] == [0, 0]
    value = wait(lambda s: str(first_state) in history(s).get("bounds", {}), "redo chip ready")
    common.click_rect(window, harness, history(value)["bounds"][str(first_state)])
    value = wait(lambda s: s.get("focused_text") == first, "redo chip restores recorded state")
    source = next(t for t in common.tabs(value) if t["title"] == "alpha")
    assert source["selections"][source.get("primary", 0)] == [len(first.encode()), len(first.encode())]
    painted(window, harness, output, "COMMAND")
    common.capture(window, output, "01-history-chips.png", checks)
    common.key("i")
    common.type_text(" second")
    common.key("Escape")
    second = first + " second"
    value = wait(lambda s: s.get("focused_text") == second, "second history state")
    second_state = history(value)["page"]["current"]
    value = wait(lambda s: "0" in history(s).get("bounds", {}), "initial chip reappears")
    common.click_rect(window, harness, history(value)["bounds"]["0"])
    wait(lambda s: s.get("focused_text") == original, "restore before branching")
    common.key("ctrl+End")
    common.key("i")
    common.type_text(" alternative")
    common.key("Escape")
    wait(lambda s: s.get("focused_text") == original + " alternative", "new branch")

    def open_history():
        common.key("ctrl+shift+p")
        common.type_text("editor.history")
        common.key("Return")
        return wait(lambda s: history(s).get("open"), "owned editing history chooser")

    value = open_history()
    value = painted(window, harness, output, "Editing history", history=True)
    common.capture(window, output, "02-history-branches.png", checks)
    count = len(history(value)["page"]["entries"])
    value = wait(lambda s: str(second_state) in history(s).get("bounds", {}), "abandoned branch row ready")
    common.click_rect(window, harness, history(value)["bounds"][str(second_state)])
    value = wait(lambda s: s.get("focused_text") == second and not history(s).get("open"), "branch restored by one click")
    assert len(history(value)["page"]["entries"]) == count, "restore must move the head, not create a node"
    assert (vault / "alpha.md").read_text() == original, "restoring editing history must leave the disk baseline alone"
    common.key("u")
    wait(lambda s: s.get("focused_text") == first, "Undo on the restored branch")
    common.key("shift+u")
    wait(lambda s: s.get("focused_text") == second, "Redo follows the restored branch")
    common.key("i")
    wait(lambda s: (s.get("context_mode") or {}).get("state") == "TYPING", "typing before chooser")
    open_history()
    common.key("Escape")
    value = wait(lambda s: not history(s).get("open"), "Escape closes chooser first")
    assert value["context_mode"]["state"] == "TYPING"
    value = open_history()
    common.click_rect(window, harness, value["context_mode"]["bounds"]["editor.context_exit"])
    wait(lambda s: s.get("context_mode") is None, "Exit remains above the history chooser")
    checks.append({"id": "context-history-chips-branches-selection-and-exit", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
