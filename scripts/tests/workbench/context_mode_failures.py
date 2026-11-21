"""FEUX-0002 X07: diagnostics burst hint in a real session.

A rapid destructive accelerator burst (five deletes within the 1.5 s
window) shows the non-blocking hint; the harness flag and a painted OCR
check prove it; the hint's Exit action leaves Context Mode and clears the
banner. The embedded negative is intrinsic: if the threshold, window, or
banner render breaks, the `burst_hint` wait or the OCR paint check times
out and the validator fails.
"""
import time

import common


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)

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
    common.key("ctrl+End")

    # Four lines give the delete burst room to act without emptying the doc.
    for word in ("one", "two", "three", "four"):
        common.type_text(word)
        if word != "four":
            common.key("Return")
    wait(
        lambda s: s.get("focused_text", "").endswith("four"),
        "four-line source text",
    )
    common.key("ctrl+Home")
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "command state",
    )

    # Rapid burst: five delete accelerators inside the 1.5 s window.
    for _ in range(5):
        common.key("d")
    wait(
        lambda s: (s.get("diagnostics") or {}).get("burst_hint") is True,
        "burst hint appears after five mutating activations",
    )

    # Paint: the hint text renders in the chrome (whole-window OCR).
    deadline = time.monotonic() + 20
    last = ""
    while time.monotonic() < deadline:
        image = output / "paint-burst.png"
        common.run("import", "-window", window, str(image))
        last = common.run("tesseract", str(image), "stdout", "-l", "afr", "--psm", "6")
        if "succession" in last.replace("\n", " "):
            break
        time.sleep(0.05)
    assert "succession" in last.replace("\n", " "), f"burst hint paint missing; OCR={last!r}"
    common.capture(window, output, "01-burst-hint.png", checks)

    # The hint's Exit action leaves Context Mode and clears the banner.
    bounds = {}
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        s = common.snapshot(harness)
        bounds = (s.get("context_mode") or {}).get("bounds") or {}
        if "burst-exit" in bounds:
            break
        time.sleep(0.04)
    assert "burst-exit" in bounds, "hint Exit control not rendered"
    common.click_rect(window, harness, bounds["burst-exit"])
    wait(lambda s: s.get("context_mode") is None, "mode exit via the hint")
    wait(
        lambda s: not (s.get("diagnostics") or {}).get("burst_hint"),
        "banner cleared after exit",
    )

    checks.append({"id": "context-mode-failures-burst-hint", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
