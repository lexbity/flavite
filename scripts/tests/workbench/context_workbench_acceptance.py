"""FEUX-0002 X08: integrated native acceptance — one continuous OS-input
workflow across the selection-first surface.

Flow: open the source note → enter Context Mode → build three carets via
the Select-menu row navigation (Add caret on row below, keyboard-only) →
type at all three members (one undo group) → seal with a motion → copy →
palette keep-primary + Go-menu to document start → paste (second undo
node) → undo twice (byte-exact original) → chip restore of the recorded
state (recorded selection set comes back) → mode exit with a muted standing
count → Esc collapse in ordinary Edit.

Embedded seeded negatives (each fails the validator when the product is
wrong):
- dropped key: typing at all three members must land everywhere — any
  dropped/replayed event breaks the byte-exact text check;
- duplicate activation: each Select-menu row dispatch installs exactly one
  caret — the count must match the number of dispatches;
- hidden Exit bounds: Exit must remain painted and clickable above the
  history chooser.
- The match-selection query row is exercised as a *surface* (palette
  opens it, paint verified); query entry into the row is recorded as
  environment-limited (synthetic pointer/keyboard focus does not transfer
  into newly mounted chrome), and the query semantics are covered by the
  headless App/Session suites.
"""
import time

import common


def search_state(s):
    return next(
        (t.get("selection_search") for t in common.tabs(s) if t.get("selection_search") is not None),
        None,
    )


def s_snapshot(harness):
    return common.snapshot(harness)


def mode(s):
    return (s.get("context_mode") or {}).get("state")


def run_select_row(harness, down):
    """Open the Select menu and run the row at `down` arrow-downs (the
    first row is index 0). Keyboard-only menu navigation."""
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        value = common.snapshot(harness)
        if (value.get("context_mode") or {}).get("menu") != "Select":
            opened = False
            for _ in range(6):
                common.key("v")
                time.sleep(0.4)
                value = common.snapshot(harness)
                if (value.get("context_mode") or {}).get("menu") == "Select":
                    opened = True
                    break
            if not opened:
                time.sleep(0.2)
                continue
        for _ in range(down):
            common.key("Down")
            time.sleep(0.15)
        common.key("Return")
        # Let the dispatch land; the harness selection-count line lags, so
        # callers assert through text effects instead of count polling.
        end = time.monotonic() + 5
        while time.monotonic() < end:
            value = common.snapshot(harness)
            if (value.get("context_mode") or {}).get("menu") is None:
                return
            time.sleep(0.1)
    raise AssertionError("menu row dispatch did not complete")


def palette_run(harness, command_id):
    common.key("ctrl+shift+p")
    common.wait(harness, lambda s: s.get("palette_open"), "palette opened")
    common.key("ctrl+a")
    common.type_text(command_id)
    common.key("Return")
    common.wait(harness, lambda s: not s.get("palette_open"), "palette dispatched")


def press_until(harness, key, predicate, label, tries=4):
    """Press `key` until `predicate` holds on a snapshot (retrying under
    synthetic-input stale captures)."""
    for _ in range(tries):
        common.key(key)
        try:
            end = time.monotonic() + 6
            while time.monotonic() < end:
                if predicate(common.snapshot(harness)):
                    return
                time.sleep(0.1)
        except (KeyError, StopIteration, IndexError, TypeError):
            pass
        time.sleep(0.3)
    raise AssertionError(label)


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)

    # --- open the source note and write the fixture text ---
    common.key("ctrl+p")
    common.type_text("alpha")
    common.key("Return")
    value = wait(
        lambda s: any(t["title"] == "alpha" and t["load"] == "ready" and t["editor_bounds"] for t in common.tabs(s)),
        "source editor ready",
    )
    editor = next(t for t in common.tabs(value) if t["title"] == "alpha")
    common.click_rect(window, harness, editor["editor_bounds"])
    common.run("xdotool", "windowsize", "--sync", window, "1600", "800")
    wait(lambda s: s["window_size"][0] >= 1580, "window widened")

    # The runner pre-seeds alpha.md with "# Alpha\n\nalpha body\n".
    base = "# Alpha\n\nalpha body\n"
    for word in ("alpha", "beta", "gamma"):
        common.key("ctrl+End")
        common.type_text(word + " x")
        common.key("Return")
    source = base + "alpha x\nbeta x\ngamma x\n"
    wait(lambda s: s.get("focused_text", "") == source, "fixture text written")
    original = source

    # --- enter Context Mode at the document start ---
    common.key("ctrl+Home")
    common.key("ctrl+space")
    wait(lambda s: mode(s) == "COMMAND", "command state")

    # --- build three carets via Select-menu rows (Add caret on row below,
    # index 7); the effect is asserted through the typed text below ---
    run_select_row(harness, 7)
    run_select_row(harness, 7)
    time.sleep(0.6)

    # --- type at all three members (one atomic undo group) ---
    # The transitions `valid` gate requires the captured target to match the
    # committed selection exactly; under synthetic input the first `i` can
    # capture mid-commit, so retry.
    entered = False
    for _ in range(4):
        common.key("i")
        try:
            end = time.monotonic() + 4
            while time.monotonic() < end:
                if mode(common.snapshot(harness)) == "TYPING":
                    entered = True
                    break
                time.sleep(0.1)
        except StopIteration:
            pass
        if entered:
            break
        time.sleep(0.4)
    assert entered, "typing group did not open"
    common.type_text("!")
    # The three carets sit at the start of the first three lines (built by
    # Add caret on row below from the document start).
    edited = "!# Alpha\n!\n!alpha body\n" + "alpha x\nbeta x\ngamma x\n"
    wait(lambda s: s.get("focused_text") == edited, "typing edited all three members")
    # Negative (dropped key): every member got the character; a dropped or
    # replayed key would break the byte-exact equality above.

    # --- seal the group with Escape; one undo reverts the whole batch ---
    common.key("Escape")
    wait(lambda s: mode(s) == "COMMAND", "Escape sealed the typing group")
    common.key("u")
    wait(lambda s: s.get("focused_text") == original, "one undo reverts the whole batch")
    common.key("shift+u")
    wait(lambda s: s.get("focused_text") == edited, "redo restores the batch")

    # --- surface: the toolbar renders the Copy control; execute via the
    # physical `y` binding (the same canonical executor the toolbar button
    # routes to). Control clicks after prior commands are unreliable under
    # this environment's synthetic pointer timing (X04 record); the toolbar
    # copy/paste surface is the presence + key-execution pair.
    cut = {}
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        current = common.snapshot(harness)
        cut = (current.get("context_mode") or {}).get("bounds") or {}
        if "editor.copy" in cut and "editor.paste" in cut:
            break
        time.sleep(0.04)
    assert "editor.copy" in cut and "editor.paste" in cut, "toolbar copy/paste controls not rendered"
    time.sleep(0.5)
    # The expected clipboard is computed from the live committed members:
    # each caret contributes its containing line including its break, and
    # the fragments join with LF — so a wrong copy or a wrong broadcast
    # breaks the byte-exact paste check below.
    value = common.snapshot(harness)
    copy_members = next(
        (t["selections"] for t in common.tabs(value)
         if t.get("active") and t.get("selection_count") is not None)
    )
    edited_now = value["focused_text"]

    def containing_line(at):
        start = edited_now.rfind("\n", 0, at) + 1
        end = edited_now.find("\n", at)
        end = len(edited_now) if end == -1 else end + 1
        return edited_now[start:end]

    clipboard = "\n".join(containing_line(head) for (_, head) in copy_members)
    for _ in range(4):
        common.key("y")  # copy: fragments join with LF
        time.sleep(0.4)
    time.sleep(0.4)

    # --- palette keep-primary, then Go menu to document start ---
    palette_run(harness, "editor.keep_primary")
    time.sleep(0.6)
    # `g` may be refused if captured mid-commit; retry until the Go menu
    # actually opens.
    opened = False
    for _ in range(4):
        common.key("g")
        try:
            end = time.monotonic() + 4
            while time.monotonic() < end:
                if (common.snapshot(harness).get("context_mode") or {}).get("menu") == "Go":
                    opened = True
                    break
                time.sleep(0.1)
        except StopIteration:
            pass
        if opened:
            break
        time.sleep(0.3)
    assert opened, "Go menu did not open"
    common.key("t")
    wait(
        lambda s: (s.get("context_mode") or {}).get("menu") is None,
        "document-start motion on the primary",
    )

    # --- paste broadcast (second undo node) ---
    common.key("p")
    pasted = clipboard + edited
    press_until(
        harness,
        "p",
        lambda s: s.get("focused_text") == pasted,
        "paste broadcast inserted once",
    )

    # --- match selection (X06): real query entry into the owned row, the
    # preview overlay, and the atomic Apply. The committed count is derived
    # from the deterministic fixture text, so the negative is real: a stale
    # or partial install breaks the exact equality.
    palette_run(harness, "editor.select_matches")
    wait(
        lambda s: (search_state(s) or {}).get("kind") == "select",
        "selection-search row open",
    )
    deadline = time.monotonic() + 15
    last = ""
    while time.monotonic() < deadline:
        image = output / "paint-searchrow.png"
        common.run("import", "-window", window, str(image))
        last = common.run("tesseract", str(image), "stdout", "-l", "afr", "--psm", "6")
        # "Alt+Enter" OCRs unreliably (the + mangles); the placeholder's
        # tail survives OCR and proves the row paints.
        if "inserts a newline" in last.replace("\n", " "):
            break
        time.sleep(0.05)
    assert "inserts a newline" in last.replace("\n", " "), f"search row not painted; OCR={last!r}"
    common.capture(window, output, "02-searchrow.png", checks)

    # The row owns keyboard input: the query is typed into the row itself
    # (executed, not recorded as environment-limited).
    assert common.click_selection_row_input(window, harness), (
        "the query input's mounted bounds are known"
    )
    time.sleep(0.2)
    common.type_text("alpha")
    wait(
        lambda s: (search_state(s) or {}).get("query") == "alpha",
        "query entry landed in the owned row",
    )
    wait(
        lambda s: (search_state(s) or {}).get("pending") is False
        and (search_state(s) or {}).get("overlay_matches", 0) > 0,
        "collection drained into an overlay",
    )
    # The row's default is case-insensitive: "# Alpha" matches too.
    expected_matches = pasted.lower().count("alpha")
    wait(
        lambda s: (search_state(s) or {}).get("total_matches") == expected_matches,
        f"the exact observed total is {expected_matches}",
    )
    common.key("Return")
    wait(
        lambda s: common.selections(s) == expected_matches,
        "Apply installs exactly the observed matches",
    )
    # Negative (double apply): a second Enter with the consumed overlay must
    # be a zero-effect refusal — the committed count cannot change.
    common.key("Return")
    time.sleep(0.6)
    assert common.selections(s_snapshot(harness)) == expected_matches, (
        "a double apply must not install twice"
    )
    common.key("Escape")
    wait(lambda s: (search_state(s) or {}).get("kind") != "select", "select row closed")

    # --- split (the Kakoune S complement): gaps between the same matches,
    # through the same owned row ---
    palette_run(harness, "editor.split_selection")
    wait(
        lambda s: (search_state(s) or {}).get("kind") == "split",
        "split row open",
    )
    assert common.click_selection_row_input(window, harness), (
        "the split query input's mounted bounds are known"
    )
    time.sleep(0.2)
    common.type_text("alpha")
    wait(
        lambda s: (search_state(s) or {}).get("query") == "alpha"
        and (search_state(s) or {}).get("pending") is False,
        "split query entered and collected",
    )
    lowered = pasted.lower()
    leading = lowered.startswith("alpha")
    trailing = lowered.endswith("alpha")
    expected_gaps = expected_matches + 1 - int(leading) - int(trailing)
    common.key("Return")
    wait(
        lambda s: common.selections(s) == expected_gaps,
        f"split installs exactly the {expected_gaps} gaps",
    )
    common.key("Escape")
    wait(lambda s: (search_state(s) or {}).get("kind") != "split", "split row closed")

    # --- undo twice: paste node, then the typing node (each press is
    # retried while the target text is not yet reached — the byte-exact
    # equality stays the deliverable) ---
    press_until(harness, "u", lambda s: s.get("focused_text") == edited, "undo reverts the paste")
    press_until(harness, "u", lambda s: s.get("focused_text") == original, "undo reverts the typing batch")

    # --- chip restore with the recorded selection ---
    palette_run(harness, "editor.history")
    wait(
        lambda s: (s.get("context_mode") or {}).get("history", {}).get("open"),
        "history chooser open",
    )
    value = common.snapshot(harness)
    history = value["context_mode"]["history"]
    candidates = [
        entry
        for entry in history["page"]["entries"]
        if entry.get("selection_count") == 3 and entry.get("restorable")
    ]
    assert candidates, f"no recorded three-selection state in the chooser: {history['page']['entries']}"
    chip_node = max(entry["target"]["node"] for entry in candidates)
    bounds = history.get("bounds") or {}
    assert str(chip_node) in bounds, f"chip bound missing for node {chip_node}: {bounds}"
    # The negative: the restore must not write anything to disk. Record the
    # disk immediately before the click and require byte-identical content
    # after (whatever the autosave state is).
    disk_before = (vault / "alpha.md").read_text()
    common.click_rect(window, harness, bounds[str(chip_node)])
    wait(
        lambda s: s.get("focused_text") == edited and common.selections(s) == 3,
        "chip restore brings back text and the recorded selection",
    )
    assert (vault / "alpha.md").read_text() == disk_before, (
        "restore leaves the disk baseline alone"
    )

    # --- Exit stays above the chooser (hidden-Exit negative) ---
    palette_run(harness, "editor.history")
    wait(
        lambda s: (s.get("context_mode") or {}).get("history", {}).get("open"),
        "history chooser re-opened",
    )
    value = common.snapshot(harness)
    assert "editor.context_exit" in value["context_mode"]["bounds"], "Exit hidden by the chooser"
    common.click_rect(window, harness, value["context_mode"]["bounds"]["editor.context_exit"])
    wait(lambda s: s.get("context_mode") is None, "Exit works through the chooser")

    # --- OFF-state: muted count paints (the "selections" caption only
    # renders when N > 1 survive exit) ---
    deadline = time.monotonic() + 20
    last = ""
    while time.monotonic() < deadline:
        image = output / "paint-offstate.png"
        common.run("import", "-window", window, str(image))
        # The count lives in the note footer; OCR the bottom strip only —
        # the whole-window pass mangles the small footer text.
        common.run(
            "python3", "-c",
            "import sys; from PIL import Image, ImageOps; "
            f"img = Image.open({str(image)!r}); "
            "strip = img.crop((0, img.height - 70, img.width // 2, img.height - 20))"
            ".convert('L'); "
            "ImageOps.invert(strip).resize((strip.width * 3, strip.height * 3))"
            f".save({str(output / 'paint-offstate-footer.png')!r})",
        )
        last = common.run(
            "tesseract", str(output / "paint-offstate-footer.png"),
            "stdout", "-l", "afr", "--psm", "6",
        )
        if "selections" in last.replace("\n", " "):
            break
        time.sleep(0.05)
    assert "selections" in last.replace("\n", " "), f"OFF-state count not painted; OCR={last!r}"
    common.capture(window, output, "01-offstate-count.png", checks)

    # --- Esc collapses the additional selections first, then the range ---
    # (collapses are selection-only; text stays byte-identical)
    common.key("Escape")
    time.sleep(0.3)
    common.key("Escape")
    time.sleep(0.3)
    assert common.snapshot(harness)["focused_text"] == edited, "collapse is selection-only"

    checks.append({"id": "context-workbench-acceptance", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
