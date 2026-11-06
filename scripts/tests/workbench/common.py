"""Shared OS-input harness helpers for the RFC-0006 workbench visual
scenarios (`scripts/workbench-visual-linux.sh --scenario NAME`).

Extracted from the RFC-0003 runners: every scenario module receives the
same argv contract `(window, harness, output, vault, backend)` and drives
the *real* shell under Xvfb with real OS input, waiting for semantic
readiness and paint — never synthetic harness state (RFC-0006 §11).
"""

import json
import subprocess
import sys
import time
from pathlib import Path


def argv():
    """(window, harness, output, vault, backend) from the runner."""
    window, harness, output, vault, backend = sys.argv[1:]
    return window, Path(harness), Path(output), Path(vault), backend


def checks_store():
    return []


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def snapshot(harness):
    for line in reversed(Path(harness).read_text().splitlines()):
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            pass
    return {}


def wait(harness, predicate, label):
    """Poll the last harness line until `predicate` holds. Bounded."""
    end = time.monotonic() + 30
    while time.monotonic() < end:
        value = snapshot(harness)
        try:
            if predicate(value):
                return value
        except (KeyError, StopIteration, IndexError, TypeError):
            pass
        time.sleep(0.04)
    raise AssertionError(f"{label}: {snapshot(harness)}")


def key(chord):
    window, *_ = argv()
    run("xdotool", "windowfocus", "--sync", window)
    run("xdotool", "key", "--clearmodifiers", chord)


def type_text(value):
    window, *_ = argv()
    run("xdotool", "type", "--clearmodifiers", "--delay", "90", value)


def tabs(value):
    return [t for g in value.get("groups", []) for t in g["tabs"]]


def graph(value):
    return next((t for t in tabs(value) if t["load"] == "graph" and t["active"]), {})


def window_geometry(window):
    geometry = dict(
        row.split("=", 1)
        for row in run("xdotool", "getwindowgeometry", "--shell", window).splitlines()
    )
    return int(geometry["WIDTH"]), int(geometry["HEIGHT"])


def point(window, harness, x, y):
    """Move the pointer to logical-window coordinates (scaled to pixels)."""
    value = snapshot(harness)
    width, height = window_geometry(window)
    run(
        "xdotool",
        "mousemove",
        "--sync",
        "--window",
        window,
        str(round(x * width / value["window_size"][0])),
        str(round(y * height / value["window_size"][1])),
    )


def click_rect(window, harness, rect):
    x, y, w, h = rect
    point(window, harness, x + w / 2, y + h / 2)
    run("xdotool", "click", "1")


def pan(window, harness, dx, dy):
    """One real background drag that takes the camera (distinguishes pan
    from click past the gesture threshold). Returns the newest snapshot
    only after the screen projection is stable: the OS event queue may
    still be draining the drag when mouseup returns."""
    value = snapshot(harness)
    g = next(
        t
        for t in tabs(value)
        if t["load"] == "graph" and t.get("active")
    )["graph"]
    x, y, w, h = g["bounds"]
    occupied = [(n["x"], n["y"]) for n in g.get("screen_nodes", [])]
    # Start at the canvas lower-left, clear of every node by 30 px.
    sx, sy = x + 24.0, y + h - 24.0
    while any(abs(sx - nx) < 30 and abs(sy - ny) < 30 for nx, ny in occupied):
        sy -= 12.0
    point(window, harness, sx, sy)
    run("xdotool", "mousedown", "1")
    steps = max(3, abs(dx) // 12)
    for i in range(1, steps + 1):
        point(window, harness, sx + dx * i / steps, sy + dy * i / steps)
    run("xdotool", "mouseup", "1")
    wait(
        harness,
        lambda s: (
            v := next(
                (t for t in tabs(s) if t["load"] == "graph" and t.get("active")), None
            )
        )
        and v["graph"].get("user_camera_moved"),
        "camera taken by the user pan",
    )
    # Two consecutive snapshots with identical screen projection prove the
    # queued drag events are fully consumed.
    end = time.monotonic() + 10
    last = None
    while time.monotonic() < end:
        value = snapshot(harness)
        graph_tab = next(
            (t for t in tabs(value) if t["load"] == "graph" and t.get("active")), None
        )
        nodes = (
            tuple(sorted((n["title"], n["x"], n["y"]) for n in graph_tab["graph"]["screen_nodes"]))
            if graph_tab and graph_tab["graph"].get("screen_nodes")
            else None
        )
        if nodes is not None and nodes == last:
            return value
        last = nodes
        time.sleep(0.05)
    raise AssertionError("screen projection still moving after the pan")


def palette(harness, title):
    palette_run, *_ = argv()
    key("ctrl+shift+p")
    wait(harness, lambda s: s.get("palette_open"), "palette opened")
    key("ctrl+a")
    type_text(title)
    key("Return")
    wait(harness, lambda s: not s.get("palette_open"), "palette dispatched")


def focus_tab(harness, title, load):
    """Click a tab strip slot until that tab is the focused one."""
    window, _, _, _, _ = argv()
    value = wait(
        harness,
        lambda s: any(t["title"] == title and t["load"] == load for t in tabs(s)),
        f"{title} tab present",
    )
    group = next(
        g for g in value["groups"] if any(t["title"] == title for t in g["tabs"])
    )
    tab = next(t for t in group["tabs"] if t["title"] == title)
    x, y, w, _ = group["bounds"]
    point(
        window,
        harness,
        x + min(w - 12, tab["slot_index"] * group["tab_width"] + 35),
        y + 16,
    )
    run("xdotool", "click", "1")
    wait(harness, lambda s: s.get("focused_tab") == tab["tab"], f"{title} tab focus")
    return tab


def focus_graph(harness):
    """Focus the active graph tab."""
    window, harness_path, _, _, _ = argv()
    value = snapshot(harness_path)
    group = next(
        g
        for g in value["groups"]
        if any(t["load"] == "graph" and t["active"] for t in g["tabs"])
    )
    tab = next(t for t in group["tabs"] if t["load"] == "graph" and t["active"])
    x, y, w, _ = group["bounds"]
    point(
        window,
        harness,
        x + min(w - 12, tab["slot_index"] * group["tab_width"] + 35),
        y + 16,
    )
    run("xdotool", "click", "1")
    wait(harness, lambda s: s.get("focused_tab") == tab["tab"], "graph tab focus")


def capture(window, output, name, checks):
    """Screenshot that fails the scenario when the paint is blank."""
    path = output / name
    for _ in range(50):
        run("import", "-window", window, str(path))
        run("identify", str(path))
        colors = int(run("identify", "-format", "%k", str(path)))
        if colors >= 16:
            checks.append({"id": name, "outcome": "pass", "artifact": name})
            return path
        time.sleep(0.1)
    raise AssertionError(f"capture appears blank after waiting for paint: {name}")


def finish(output, checks, backend):
    (output / "graph-editor-loop-checks.json").write_text(
        json.dumps(
            {"backend": backend, "input_evidence": "rendered_os_input", "checks": checks},
            indent=2,
        )
        + "\n"
    )
