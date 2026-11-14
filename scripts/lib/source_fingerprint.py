#!/usr/bin/env python3
"""Fingerprint build inputs for native acceptance without reading vault data."""
import hashlib
from pathlib import Path
import subprocess


def fingerprint():
    digest = hashlib.sha256()
    paths = set(subprocess.check_output(["git", "ls-files", "-co", "--exclude-standard", "-z"]).split(b"\0"))
    for raw in sorted(path for path in paths if path):
        path = Path(raw.decode())
        if not path.is_file() or not (
            str(path).startswith(("crates/", "vendor/", "scripts/", ".github/"))
            or str(path) in ("Cargo.lock", "Cargo.toml", "justfile")
        ):
            continue
        data = path.read_bytes()
        digest.update(raw + b"\0" + str(len(data)).encode() + b"\0" + data)
    return digest.hexdigest()


if __name__ == "__main__":
    print(fingerprint())
