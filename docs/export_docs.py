#!/usr/bin/env python3
"""Convert Markdown documentation into Hugo-compatible content.

Reads all ``.md`` files from a source directory (default ``./docs``) and writes
them to ``content/docs/`` with Hugo front matter, organized into category
folders based on filename keywords:

    readme | intro | overview            -> getting-started/
    install | setup | configure          -> getting-started/
    usage | guide | tutorial             -> guides/
    api | reference | docs               -> reference/
    everything else                      -> miscellaneous/
"""

from __future__ import annotations

import argparse
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

# Categories in priority order; the first matching rule wins.
# (category folder, category index, filename keywords)
CATEGORIES = [
    ("getting-started", 0, ("readme", "intro", "overview")),
    ("getting-started", 0, ("install", "setup", "configure")),
    ("guides", 1, ("usage", "guide", "tutorial")),
    ("reference", 2, ("api", "reference", "docs")),
    ("miscellaneous", 3, ()),
]

FRONT_MATTER_DELIM = re.compile(r"\A---\s*\n.*?\n---\s*\n", re.DOTALL)
HEADING_RE = re.compile(r"\A#{1,6}\s+(.+?)\s*#*\s*$")


def categorize(filename: str) -> tuple[str, int]:
    """Return (folder, category_index) for a source filename."""
    stem = Path(filename).stem.lower()
    for folder, index, keywords in CATEGORIES:
        if any(keyword in stem for keyword in keywords):
            return folder, index
    return "miscellaneous", 3


def extract_title(content: str, filename: str) -> tuple[str, str]:
    """Extract a title from the content (first heading), falling back to the
    filename. Returns (title, content_with_heading_stripped)."""
    for line in content.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        match = HEADING_RE.match(stripped)
        if match:
            return match.group(1), content
        break  # non-empty, non-heading first line: keep content untouched
    # No leading heading — derive from the filename.
    stem = Path(filename).stem
    title = stem.replace("-", " ").replace("_", " ").title()
    return title, content


def strip_front_matter(content: str) -> str:
    """Remove an existing YAML front matter block, if present."""
    return FRONT_MATTER_DELIM.sub("", content)


def lastmod(path: Path) -> str:
    """File modification time as a Hugo-friendly UTC timestamp."""
    mtime = datetime.fromtimestamp(path.stat().st_mtime, tz=timezone.utc)
    return mtime.strftime("%Y-%m-%dT%H:%M:%S+00:00")


def convert_file(src: Path, out_root: Path, project_name: str) -> Path:
    content = src.read_text(encoding="utf-8")
    content = strip_front_matter(content)

    title, content = extract_title(content, src.name)
    folder, category_index = categorize(src.name)
    weight = 10 + category_index * 5

    front_matter = (
        "---\n"
        f'title: "{project_name} - {title}"\n'
        f"weight: {weight}\n"
        "type: docs\n"
        "draft: false\n"
        f"lastmod: {lastmod(src)}\n"
        "---\n\n"
    )

    dest = out_root / folder / src.name
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(front_matter + content, encoding="utf-8")
    return dest


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--source",
        type=Path,
        default=Path("docs"),
        help="directory containing source .md files (default: ./docs)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("content/docs"),
        help="Hugo content output directory (default: ./content/docs)",
    )
    parser.add_argument(
        "--project-name",
        default=None,
        help="project name used in titles (default: name of the source's parent directory)",
    )
    args = parser.parse_args(argv)

    source = args.source.resolve()
    if not source.is_dir():
        parser.error(f"source directory not found: {source}")

    project_name = args.project_name or source.parent.name

    md_files = sorted(source.glob("*.md"))
    if not md_files:
        print(f"No .md files found in {source}", file=sys.stderr)
        return 1

    for src in md_files:
        dest = convert_file(src, args.output, project_name)
        print(f"{src} -> {dest}")

    print(f"\nConverted {len(md_files)} file(s) into {args.output}/")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
