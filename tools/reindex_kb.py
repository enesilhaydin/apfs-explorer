#!/usr/bin/env python3
"""Extract Apple's APFS Reference PDF to page-anchored markdown.

The context-mode FTS5 KB is ephemeral (per-PID tmpdir, cleaned on exit), so
this extractor is the durable on-disk piece of the re-index pipeline. The
agent (or anyone) then ctx_index'es the output file as source
``apfs-spec:pdf-verbatim``.

Page headers are emitted as ``## PAGE <printed-page-no> (pdf-index N)`` so
verbatim lookups by section / page number round-trip exactly. All 181
pages of the Apple "Apple File System Reference" PDF are captured (zero
truncation).

Usage:
    python3 tools/reindex_kb.py               # default in/out paths
    python3 tools/reindex_kb.py PDF OUT.md    # explicit paths

Then in the agent session:
    ctx_index(path="/tmp/apfs-spec-pages.md",
              source="apfs-spec:pdf-verbatim")

Run ``scripts/reindex-kb.md`` for the full recipe (this + all fetch URLs).
Requires: ``pip3 install --user pymupdf``.
"""
from __future__ import annotations

import os
import re
import sys


def extract(pdf_path: str, out_path: str) -> tuple[int, int]:
    import fitz  # pymupdf

    doc = fitz.open(pdf_path)
    total = len(doc)
    written_pages = 0
    with open(out_path, "w") as f:
        f.write("# Apple File System Reference (page-anchored)\n\n")
        f.write(f"Source: `{pdf_path}` — {total} pages, pymupdf.\n\n")
        for i in range(total):
            text = doc[i].get_text("text").strip()
            if not text:
                continue
            # Try to read the printed page number from the page footer; fall
            # back to the 1-based pdf index.
            m = re.search(r"\b(\d{1,3})\b\s*$", text)
            printed = m.group(1) if m else str(i + 1)
            f.write(f"\n## PAGE {printed} (pdf-index {i + 1})\n\n")
            f.write(text + "\n")
            written_pages += 1
    return total, written_pages


def main(argv: list[str]) -> int:
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    pdf_default = os.path.join(repo, "docs", "Apple-File-System-Reference.pdf")
    out_default = "/tmp/apfs-spec-pages.md"
    pdf = argv[1] if len(argv) > 1 else pdf_default
    out = argv[2] if len(argv) > 2 else out_default
    if not os.path.isfile(pdf):
        print(f"PDF not found: {pdf}", file=sys.stderr)
        print(
            "Place Apple's APFS Reference PDF at docs/Apple-File-System-Reference.pdf "
            "or pass the path as the first argument.",
            file=sys.stderr,
        )
        return 1
    try:
        total, written = extract(pdf, out)
    except ModuleNotFoundError:
        print("pymupdf missing — run: pip3 install --user pymupdf", file=sys.stderr)
        return 2
    sz = os.path.getsize(out)
    print(
        f"OK extracted {written}/{total} pages → {out} "
        f"({sz} bytes, {sz // 1024} KB). "
        f"Next: ctx_index(path={out!r}, source='apfs-spec:pdf-verbatim')."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
