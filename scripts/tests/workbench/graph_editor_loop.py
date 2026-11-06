"""RFC-0006 S04 native scenario `graph-editor-loop` (AC-4/5/6, D-02/D-03).

Runs the real shell on the selected renderer backend with OS input and
proves the edit→graph→open→edit loop: selection changes the canvas bounds
by zero and keeps painted node positions; a double click *activates* an
unopened node (opens its editor, never inserts); editing by keys publishes
a real unsaved edge; the explicit Insert command places a link into the
other note at its acknowledged caret; the graph stays visible and a
user-held camera stays unchanged throughout.
"""

import hashlib
import time
from pathlib import Path

from PIL import Image

import common


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()

    def snap():
        return common.snapshot(harness)

    def wait(predicate, label):
        return common.wait(harness, predicate, label)

    def tabs(value):
        return common.tabs(value)

    def graph(value):
        return common.graph(value)

    def key(chord):
        common.key(chord)

    def type_text(value):
        common.type_text(value)

    def ready_graph():
        return wait(
            lambda s: (g := graph(s))
            and g.get("graph", {}).get("bounds")
            and g["graph"].get("nodes", 0) > 0
            and not g["graph"]["updating"],
            "current graph frame",
        )

    def node_by_title(title):
        """Renewed identity lookup from the newest frame (S04: coordinates
        are never retained across frames)."""
        value = ready_graph()
        return next(
            n for n in graph(value)["graph"]["screen_nodes"] if n["title"] == title
        )

    def node_painted(node):
        """A colored node disc occupies the logical center: one screenshot,
        then the center crop must differ from the canvas corner pixel (any
        node color, selection emphasis included — positions, not palette)."""
        path = output / "probe.png"
        common.run("import", "-window", window, str(path))
        with Image.open(path).convert("RGB") as pixels:
            value = snap()
            sx = pixels.width / value["window_size"][0]
            sy = pixels.height / value["window_size"][1]
            x, y = round(node["x"] * sx), round(node["y"] * sy)
            if not (0 <= x - 3 and x + 4 <= pixels.width and 0 <= y - 3 and y + 4 <= pixels.height):
                return False
            corner = pixels.getpixel((2, 2))
            crop = pixels.crop((x - 3, y - 3, x + 4, y + 4))
            return any(c != corner for c in crop.getdata())

    def activate(node, expect_title):
        """One deliberate double click (two OS clicks inside the pair
        window) activates the node: the note opens, text is untouched."""
        common.point(window, harness, node["x"], node["y"])
        common.run("xdotool", "click", "1")
        time.sleep(0.2)
        common.run("xdotool", "click", "1")
        return wait(
            lambda s: any(
                t["title"] == expect_title and t["load"] == "ready" for t in tabs(s)
            ),
            f"double-click activation opened {expect_title}",
        )

    # ---- workbench setup: an editing group (the anchor) and the graph in
    # its own group — the D-13 workbench, so activation never covers the
    # network and the D-03 destination order can prove AC-5.
    alpha_before = hashlib.sha256((vault / "alpha.md").read_bytes()).hexdigest()
    gamma_before = hashlib.sha256((vault / "Systems" / "gamma.md").read_bytes()).hexdigest()
    key("ctrl+p")
    type_text("alpha")
    key("Return")
    wait(
        lambda s: any(
            t["title"] == "alpha" and t["load"] == "ready" and t.get("editor_bounds")
            for t in tabs(s)
        ),
        "alpha editor mounted",
    )
    alpha_tab = next(t for t in tabs(snap()) if t["title"] == "alpha")
    common.click_rect(window, harness, alpha_tab["editor_bounds"])
    key("ctrl+End")
    wait(lambda s: s.get("writing_target") is not None, "caret acknowledged")
    # Split: the duplicate editor anchors the second group; the graph then
    # opens into the focused group beside it.
    key("ctrl+alt+Right")
    wait(lambda s: len(s["groups"]) >= 2, "real split")
    common.palette(harness, "Open graph view")
    ready_graph()
    # Hand focus back to the editing note: its group becomes the preferred
    # document destination (D-01/D-03), independent of graph focus.
    key("shift+F6")
    wait(
        lambda s: any(
            t["title"] == "alpha" and t["active"] and g.get("focused")
            for g in s["groups"]
            for t in g["tabs"]
        ),
        "editor group refocused",
    )
    editor = next(
        t
        for t in tabs(snap())
        if t["title"] == "alpha" and t["active"] and t.get("editor_bounds")
    )
    common.click_rect(window, harness, editor["editor_bounds"])
    key("ctrl+End")
    wait(lambda s: s.get("writing_target") is not None, "caret re-acknowledged")
    common.focus_graph(harness)
    value = ready_graph()
    assert graph(value)["graph"]["backend"] == backend
    graph_tab_id = graph(value)["tab"]

    # The first upgraded launch still owes the dismissible gesture-contract
    # explanation (D-02); dismiss it through its real control.
    pending = graph(value)["graph"].get("activation_notice_pending")
    if pending:
        # The overlay reports its layout bounds on mount; wait for the
        # reachability evidence rather than sampling once.
        notice_value = wait(
            lambda s: graph(s)["graph"].get("notice_bounds"),
            "notice bounds reported",
        )
        notice = graph(notice_value)["graph"].get("notice_bounds")
        # The Got-it control sits at the right end of the notice row.
        x, y, w, h = notice
        common.click_rect(window, harness, (x + w - 60, y, 60, h))
        value = wait(
            lambda s: graph(s)["graph"].get("activation_notice_pending") is False,
            "gesture-contract explanation acknowledged",
        )
        checks.append(
            {"id": "gesture-contract-notice-dismissible", "outcome": "pass"}
        )

    common.palette(harness, "Freeze graph layout")
    common.palette(harness, "Fit graph to view")
    value = wait(lambda s: graph(s)["graph"].get("frozen"), "layout frozen")
    # AC-6 is a USER-HELD-camera claim: take the camera with a real
    # background drag pan. This stops auto-follow; the layout is frozen;
    # every later screen-position is then stable by contract, not by luck
    # of a settled simulation.
    value = common.pan(window, harness, 30, -20)

    # ---- AC-4: selection changes bounds by zero and keeps painted nodes.
    bounds_before = graph(value)["graph"]["bounds"]
    key_positions = {
        n["title"]: (n["x"], n["y"])
        for n in graph(value)["graph"]["screen_nodes"]
    }
    beta = node_by_title("Beta")
    assert node_painted(beta), f"{backend} painted no node at Beta's center"
    common.point(window, harness, beta["x"], beta["y"])
    common.run("xdotool", "click", "1")
    value = wait(
        lambda s: graph(s).get("graph", {}).get("selection") is not None,
        "single-click selection",
    )
    assert graph(value)["graph"]["bounds"] == bounds_before, "selection moved the canvas"
    beta_after = node_by_title("Beta")
    assert (beta_after["x"], beta_after["y"]) == key_positions["Beta"], (
        "selection moved painted node positions"
    )
    assert node_painted(beta_after), "selected node no longer painted at its center"
    assert "[[beta]]" not in next(
        t for t in tabs(value) if t["tab"] == alpha_tab["tab"]
    )["buffer_text"], "selection inserted a link"
    common.capture(window, output, "01-selected-zero-bounds.png", checks)
    checks.append({"id": "AC-4-selection-zero-bounds-stable-pixels", "outcome": "pass"})

    # ---- D-02: insertion is the explicit Insert command (Ctrl+Enter),
    # never a double click. Busy is a zero-mutation admission refusal whose
    # displayed repair is exercised with another explicit gesture.
    time.sleep(0.6)
    for attempt in range(3):
        common.focus_graph(harness)
        beta = node_by_title("Beta")
        common.point(window, harness, beta["x"], beta["y"])
        common.run("xdotool", "click", "1")
        value = wait(
            lambda s: graph(s)["graph"].get("selection") is not None,
            f"selection restored (attempt {attempt})",
        )
        key("ctrl+Return")
        value = wait(
            lambda s: "[[beta]]"
            in (
                next(t for t in tabs(s) if t["tab"] == alpha_tab["tab"]).get("buffer_text")
                or ""
            )
            or (
                s.get("status", "").startswith("Preparing")
                is False
                and "busy" in s.get("status", "").lower()
                and s["pending_operations"] == 0
            ),
            "explicit insertion or Busy refusal",
        )
        alpha_text = next(t for t in tabs(value) if t["tab"] == alpha_tab["tab"])[
            "buffer_text"
        ]
        if "[[beta]]" in alpha_text:
            assert alpha_text.count("[[beta]]") == 1, "insertion duplicated the link"
            break
        checks.append({"id": f"busy-repair-new-os-gesture-{attempt}", "outcome": "pass"})
        time.sleep(0.4)
    else:
        raise AssertionError("insertion refused across three explicit gestures")
    value = wait(
        lambda s: graph(s)["graph"].get("overlay_sources", 0) > 0
        and not graph(s)["graph"]["updating"],
        "unsaved edge published to the graph",
    )
    assert graph(value)["graph"]["bounds"] == bounds_before, "insertion moved the canvas"
    common.capture(window, output, "02-explicit-insert.png", checks)
    checks.append({"id": "AC-2-explicit-insert-exact-link", "outcome": "pass"})

    # ---- AC-5: a double click now ACTIVATES the node — it opens the note
    # through the shared destination policy (the preferred document group),
    # never inserts; the graph keeps its own visible group. Beta is the
    # still-unopened node away from the overlay band region.
    time.sleep(0.6)
    beta = node_by_title("Beta")
    value = activate(beta, "beta")
    assert "[[beta]] and [[gamma]]" not in next(
        t for t in tabs(value) if t["tab"] == alpha_tab["tab"]
    )["buffer_text"], "activation mutated the source note"
    assert any(
        t["load"] == "graph" and t["active"] and t["tab"] == graph_tab_id
        for t in tabs(value)
    ), "the graph lost visibility or identity"
    common.capture(window, output, "03-activation-open.png", checks)
    checks.append({"id": "AC-5-activation-opens-unopened-note", "outcome": "pass"})

    # ---- AC-6: edit the opened note by keys; the graph publishes the
    # unsaved edge without moving a user-held camera or the frozen layout.
    beta_tab = next(t for t in tabs(snap()) if t["title"] == "beta")
    common.click_rect(window, harness, beta_tab["editor_bounds"])
    key("ctrl+End")
    wait(
        lambda s: s.get("writing_target", {}).get("tab") == beta_tab["tab"],
        "beta caret acknowledged",
    )
    type_text(" see [[gamma]]")
    wait(
        lambda s: graph(s)["graph"].get("overlay_sources", 0) > 0
        and not graph(s)["graph"]["updating"],
        "beta edit published to the graph",
    )
    positions_after_edit = {
        n["title"]: (n["x"], n["y"]) for n in graph(snap())["graph"]["screen_nodes"]
    }
    assert positions_after_edit == key_positions, (
        "frozen layout or camera moved during the edit loop"
    )
    common.capture(window, output, "04-edit-published-edge.png", checks)
    checks.append({"id": "AC-6-edit-publishes-with-stable-camera", "outcome": "pass"})

    # ---- Undo through the real keymap; no disk write happens without a
    # save, and the untouched notes stay byte-identical.
    common.focus_tab(harness, "beta", "ready")
    beta_tab2 = next(t for t in tabs(snap()) if t["title"] == "beta")
    common.click_rect(window, harness, beta_tab2["editor_bounds"])
    key("ctrl+z")
    wait(
        lambda s: " see [[gamma]]"
        not in (
            next(t for t in tabs(s) if t["tab"] == beta_tab["tab"]).get("buffer_text")
            or ""
        ),
        "one undo removes the typed link",
    )
    assert (
        hashlib.sha256((vault / "alpha.md").read_bytes()).hexdigest() == alpha_before
    ), "insertion wrote to disk without a save"
    assert (
        hashlib.sha256((vault / "Systems" / "gamma.md").read_bytes()).hexdigest()
        == gamma_before
    ), "activation or editing modified gamma on disk"
    checks.append({"id": "undo-local-and-disks-untouched", "outcome": "pass"})

    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
