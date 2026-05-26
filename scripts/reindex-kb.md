# KB re-index recipe — single source of truth

The context-mode FTS5 knowledge base is **ephemeral** (per-PID tmpdir
SQLite, cleaned on process exit). This file is the canonical, complete
recipe to rebuild it from scratch in one session — Apple's full APFS
reference, every authoritative writer source we depend on, and our own
locked spec notes. Truth lives in git + upstream URLs; the KB is just a
hot search cache.

Run after a fresh session, a compact, or any time the KB feels empty.

## Step 0 — prereqs (once)

    pip3 install --user pymupdf

Confirm Apple's PDF is at `docs/Apple-File-System-Reference.pdf`
(committed via Git LFS).

## Step 1 — extract the Apple APFS Reference (all 181 pages)

    python3 tools/reindex_kb.py
    # writes /tmp/apfs-spec-pages.md (page-anchored, "## PAGE N" headers)

Then, in the agent:

    ctx_index(path="/tmp/apfs-spec-pages.md",
              source="apfs-spec:pdf-verbatim")

Expected: ~181 page sections (the entire Apple document — every offset,
every struct, every prose paragraph).

## Step 2 — fetch + index the external authoritative sources

Each line is one `ctx_fetch_and_index(url, source)` call. URLs are stable
raw-content endpoints; the source labels match what `ctx_search` queries
use elsewhere in the project.

| url | source |
|-----|--------|
| https://raw.githubusercontent.com/libyal/libfsapfs/main/documentation/Apple%20File%20System%20(APFS).asciidoc | apfs-spec:libfsapfs |
| https://raw.githubusercontent.com/linux-apfs/linux-apfs-rw/master/apfs.h | apfs-spec:linux-apfs-rw |
| https://raw.githubusercontent.com/linux-apfs/linux-apfs-rw/master/apfs_raw.h | apfs-spec:linux-apfs-rw |
| https://raw.githubusercontent.com/linux-apfs/linux-apfs-rw/master/super.c | apfs-spec:linux-apfs-rw |
| https://raw.githubusercontent.com/linux-apfs/linux-apfs-rw/master/spaceman.c | apfs-spec:linux-apfs-rw |
| https://raw.githubusercontent.com/linux-apfs/linux-apfs-rw/master/snapshot.c | apfs-spec:linux-apfs-rw |
| https://raw.githubusercontent.com/linux-apfs/linux-apfs-rw/master/btree.c | apfs-spec:linux-apfs-rw |
| https://raw.githubusercontent.com/linux-apfs/linux-apfs-rw/master/transaction.c | apfs-spec:linux-apfs-rw |
| https://raw.githubusercontent.com/linux-apfs/linux-apfs-rw/master/object.c | apfs-spec:linux-apfs-rw |
| https://raw.githubusercontent.com/lzfse/lzfse/master/src/lzvn_decode_base.c | apfs-spec:lzvn-ref |
| https://raw.githubusercontent.com/sgan81/apfs-fuse/master/README.md | apfs-fuse:repo |
| https://winfsp.dev/doc/WinFsp-API-winfsp.h/ | winfsp:apiref |

On-demand (large files; only when a milestone needs them):

| url | source |
|-----|--------|
| https://raw.githubusercontent.com/sgan81/apfs-fuse/master/ApfsLib/ApfsFile.cpp | apfs-fuse:repo |
| https://raw.githubusercontent.com/sgan81/apfs-fuse/master/ApfsLib/Decmpfs.cpp | apfs-fuse:repo |
| https://raw.githubusercontent.com/sgan81/apfs-fuse/master/ApfsLib/Crypto.cpp | apfs-fuse:repo |

## Step 3 — index our committed locked spec notes + project state

These are durable in git; they get indexed so `ctx_search` finds them
alongside the upstream sources.

    ctx_index(path="docs/superpowers/specs/2026-05-19-m7a-write-spec-notes.md",
              source="apfs-spec:m7a-confirmed")
    ctx_index(path="docs/superpowers/specs/2026-05-19-m7a-write-spec-notes-round2.md",
              source="apfs-spec:m7a-confirmed")
    ctx_index(path="docs/ROADMAP.md", source="project:roadmap-state")

If milestone-specific `apfs-spec:m{1,1b,1c,2,3,3b,3c,3d}-confirmed` labels
are referenced in code but absent from the KB, they can be reconstituted
from the corresponding plan files under `docs/superpowers/plans/` and the
code comments — those plans are themselves durable spec.

## Step 4 — sanity check

    ctx_stats             # source labels + section counts
    ctx_search(queries=["nx_superblock magic offset",
                        "btree_node_phys btn_table_space",
                        "spaceman_phys chunk_info",
                        "snap_metadata_val extentref_tree_oid"])

Each query should return hits from `apfs-spec:pdf-verbatim` AND
`apfs-spec:linux-apfs-rw` (cross-source corroboration intact).

## Notes

- KB lifetime: per-PID SQLite in the OS tmpdir, deleted on process exit;
  `claude --continue` restores session-captured tool events but does NOT
  restore manual `ctx_index`/`ctx_fetch_and_index` content. Re-run this
  recipe (Steps 1-3) at the start of any new/fresh session.
- Truth lives in git + the upstream raw URLs. If a URL moves, update the
  table here in one place; the agent re-reads this file.
- Apple's PDF is checked in at `docs/Apple-File-System-Reference.pdf`
  (LFS). Replacement is rare; pin the document hash in this file when
  Apple revises the spec.
