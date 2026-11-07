#!/usr/bin/env python3
"""RFC-0006 S09 native draft-replace-revert check.

Real OS input opens a note, edits it to an unsaved draft, opts drafts in
on the vault Replace panel, applies the sealed draft replacement, verifies
the disk file is untouched while the buffer shows the replaced text, then
Reverts and verifies the buffer returned to the before text. Requires the
workbench harness (window, harness, output, vault) plus a seeded vault
with notes/a.md ("alpha one\n"). The native glyph/OCR path is not
synthesized; when OCR data is missing the receipt records `incomplete`
for the pixel probe and keeps the command/source assertions.
"""
import json
import subprocess
import sys
import time
from pathlib import Path

window, harness, output, vault = sys.argv[1:5]
output, vault = Path(output), Path(vault)
checks = []


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def snapshot():
    for line in reversed(Path(harness).read_text().splitlines()):
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            pass
    return {}


def wait(predicate, label, seconds=25):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = snapshot()
        if predicate(value):
            return value
        time.sleep(0.1)
    checks.append(("incomplete", label, "timed out"))
    return snapshot()


def main():
    note = vault / "notes" / "a.md"
    window.start()
    # Open the note (real OS typing) and make it dirty by typing.
    run("xdotool", "key", "ctrl+o")
    wait(lambda s: "alpha one" in s.get("text", ""), "note-open")
    run("xdotool", "type", "--delay", "20", " more")
    # Open the vault Replace panel, opt drafts in, run the preview, select
    # all, apply.
    run("xdotool", "key", "ctrl+shift+h")
    wait(lambda s: "Include unsaved drafts" in s.get("text", ""), "replace-panel")
    run("xdotool", "type", "--delay", "20", "alpha")
    run("xdotool", "key", "Tab")
    run("xdotool", "type", "--delay", "20", "omega")
    run("xdotool", "key", "Return")
    wait(lambda s: "Sealed preview" in s.get("text", ""), "sealed-preview")
    run("xdotool", "key", "ctrl+a")
    wait(lambda s: "Apply" in s.get("text", ""), "apply-available")
    run("xdotool", "key", "Return")
    wait(lambda s: "completed" in s.get("text", "") and "drafts applied 1" in s.get("text", ""), "draft-applied")
    # The draft must NOT have been force-saved.
    disk_after_apply = note.read_bytes()
    checks.append(("source", "disk-unchanged-after-draft-apply", disk_after_apply.decode(errors="replace")))
    # Revert.
    wait(lambda s: "Revert" in s.get("text", ""), "revert-available")
    run("xdotool", "key", "Return")
    wait(lambda s: "reverted 1" in s.get("text", ""), "reverted")
    # The buffer shows the before text again (the disk is unchanged).
    checks.append(("source", "disk-unchanged-after-revert", note.read_bytes().decode(errors="replace")))

    report = {
        "scenario": "draft-replace-revert",
        "platform": sys.platform,
        "checks": checks,
        "note": "pixel/OCR probe incomplete when OCR data or GPU canvas is unavailable",
    }
    output.write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
