# APFS filename normalization for hashing — NotebookLM questions (English)

> 2026-05-23. **Universal-language requirement:** this writer ships to real
> users across every language and script — Latin (Turkish/German/French ASCII-
> extended), Cyrillic, Greek, Arabic (RTL), Hebrew (RTL), Chinese / Japanese /
> Korean (CJK), Devanagari, Thai, Vietnamese (combining tones), Korean Hangul
> (precomposed vs jamo), emoji with ZWJ sequences and skin-tone modifiers, etc.
> Real bug surfaced by `decisive_unicode_filenames_on_scratch`: Turkish/emoji
> filenames make `fsck_apfs` report the volume corrupt with the deferred repair
> "Update directory record objects? NO". ASCII names are fine. We need the
> CANONICAL rule the kernel uses so EVERY language round-trips cleanly. Lock
> answers to KB `apfs-spec:name-normalization`.

## Context

- The volume is the default macOS APFS image (`hdiutil`/`newfs_apfs`, case-sensitive).
- DREC key layout we emit: `jkey(8) + name_len_and_hash(u32) + name_utf8 + NUL`.
  - `name_len_and_hash` = `(hash22 << 10) | (utf8_len + 1)`.
  - `hash22 = CRC32C(UTF-32-LE(name.chars())) & 0x003F_FFFF`.
- DREC val layout: `file_id(8) + date_added(8) + dt_type(u16)` (no name in val).
- ASCII-name writes pass fsck CLEAN. Turkish "şehir.txt" / emoji "📊" do not.

## Questions

1. **Normalization rule for the name hash.** For an APFS volume with
   `APFS_INCOMPAT_NORMALIZATION_INSENSITIVE` set (the default for modern macOS),
   does the kernel apply Unicode normalization to the name BEFORE computing the
   CRC32C hash? If yes, NFD or NFC? Does the rule change when
   `APFS_INCOMPAT_CASE_INSENSITIVE` is also set vs only normalization?

2. **Exact pre-hash transform pipeline.** What is the canonical sequence:
   normalize(NFD?) → lowercase(if case-insensitive)? → UTF-32-LE → CRC32C low
   22 bits? Or normalize → lowercase → re-normalize? Order matters when
   case-folding produces decomposable code points.

3. **Stored bytes in the DREC name field.** Does the kernel store the name in
   the DREC key/val bytes as ORIGINAL UTF-8 (user-provided form, possibly NFC)
   or as the NORMALIZED form used for hashing? `fsck_apfs`'s "Update directory
   record objects" suggests the stored representation must match a canonical
   form — which one?

4. **`name_len` semantics.** Is the low 10 bits of `name_len_and_hash` the
   length of the STORED bytes (incl NUL) or the NORMALIZED bytes (incl NUL)?
   For "şehir.txt" the NFC form is 11 bytes incl NUL; the NFD form has the
   same character count but more bytes if any precomposed character decomposes.
   Which length does the kernel write?

5. **Volume incompat flag detection.** How does a writer determine whether the
   volume requires normalization for the hash? Reading `apfs_volume_superblock_t
   .apfs_incompatible_features` (offset 0x38), masking
   `APFS_INCOMPAT_NORMALIZATION_INSENSITIVE` (which bit?) — confirm the bit
   value and whether the default macOS-formatted volume has it set.

6. **`apfs_name_hash` 22-bit truncation + masking.** Confirm: the low 22 bits
   of the CRC32C are taken (no final-xor in our impl matches confirmed). Any
   variation when normalization is active (e.g. hash input padded, terminator
   included)?

## Cross-language edge cases the writer must handle correctly

The canonical rule must round-trip ALL of the following filename categories
exactly as the kernel does (writer-stored name + writer-computed hash must
match what a kernel-formatted file with the same name would produce):

7. **CJK precomposed characters** (e.g. Chinese 中文, Japanese 日本語, Korean
   한글): are CJK code points decomposed under NFD or kept atomic? Korean
   Hangul syllables specifically decompose to L+V+T jamo under NFD — does the
   kernel apply this? Does it matter that NFD-Hangul is much longer than NFC?

8. **RTL (right-to-left) scripts** — Arabic, Hebrew, Persian. Bidi control
   characters (U+200E LRM, U+200F RLM, U+202B RLE, etc.) embedded in a name:
   does the kernel preserve them in the stored bytes, and are they included in
   the hash input as-is, or stripped/normalized?

9. **Combining marks** — Vietnamese tone marks, Devanagari conjuncts, Hebrew
   niqqud, Arabic harakat. Many of these have both precomposed and
   decomposed forms (e.g. Vietnamese "ế" = U+1EBF NFC, or U+0065 + U+0302 +
   U+0301 NFD). Which form does the kernel HASH? Which does it STORE?

10. **Emoji ZWJ sequences and modifiers** — e.g. "👨‍👩‍👧" (family ZWJ
    sequence U+1F468 U+200D U+1F469 U+200D U+1F467), skin-tone modifiers,
    flags (regional indicator pairs). Are these treated as opaque UTF-32
    sequences, or are the U+200D ZWJ / U+FE0F VS16 normalized away?

11. **Non-BMP code points** — code points > U+FFFF (most emoji, historic
    scripts). Confirm the UTF-32-LE encoding for the hash is one 4-byte LE
    word per code point (not UTF-16 surrogate pairs).

12. **Case-folding of non-Latin scripts** on case-insensitive volumes —
    Turkish "İ → i" (special), German "ß → ss" (one-to-many), Greek "Σ → σ
    vs ς" (positional). Does APFS use full Unicode case-folding (e.g.
    `CaseFolding.txt`), simple `to_lowercase`, or something language-aware?
    Does the answer change between case-folded ASCII and full case-folded
    Unicode (e.g. matching Apple's `unicode_compare` vs glibc `tolower_l`)?

13. **Maximum name length under normalization**. NFD can expand a name (more
    bytes than NFC). If the user-supplied name is at the byte limit in NFC,
    can the kernel still store it (rejecting at NFD-byte > limit)? What is
    the exact byte limit — APFS spec says NAME_MAX = 255 + NUL.

## Hypotheses to confirm/correct

- Q1/Q7: I suspect NFD is required when NORMALIZATION_INSENSITIVE is set
  (default macOS), and applies uniformly across scripts including CJK Hangul
  decomposition.
- Q3: The STORED name is the ORIGINAL UTF-8 (preserves user input); only the
  HASH uses the normalized form. So a writer never alters what the user typed.
- Q5: APFS_INCOMPAT_NORMALIZATION_INSENSITIVE is bit 0x4 (per
  apfs-spec:linux-apfs-rw constants), and modern macOS sets it by default.
- Q12: APFS uses Apple's `unicode_compare`-style case-fold (the same NFD-fold
  used by HFS+); Rust's `str::to_lowercase` is close but NOT a guaranteed match
  for special locales (Turkish dotted-I etc.). A safe writer uses an Apple-
  compatible fold table (the `icu_casemap`/Unicode CaseFolding.txt status=C+F).

---

## ANSWERS (LOCKED — NotebookLM + Apple PDF, 2026-05-23) — KB `apfs-spec:name-normalization`

> md.6: hypotheses; real macOS disk is the arbiter. Empirically verified by
> `decisive_unicode_filenames_on_scratch`: with NFD unconditionally applied,
> "şehir.txt", "Günlük.md", "rapor 📊.txt" and a unicode `günlük/` directory all
> fsck CLEAN + Python decode + mount byte-exact readback.

- **Q1–2 Pipeline:** NFD → case-fold (if `APFS_INCOMPAT_CASE_INSENSITIVE`) →
  UTF-32-LE → CRC32C (Castagnoli, poly `0x82F63B78`, init `0xFFFFFFFF`, no
  final XOR) → low 22 bits. Order matters; standard Unicode CaseFolding
  (Status C+F) handles re-normalization safely. PDF p.135-136.
- **Q3–4 Stored bytes / name_len:** The DREC key stores the ORIGINAL UTF-8
  exactly as given (typically NFC from macOS UI). `name_len` low 10 bits =
  length of the STORED ORIGINAL UTF-8 incl. NUL. NFD form is used only for
  the transient hash input.
- **Q5 Incompat flags:**
  `APFS_INCOMPAT_CASE_INSENSITIVE = 0x1`,
  `APFS_INCOMPAT_NORMALIZATION_INSENSITIVE = 0x8` (NOT 0x4 — that's
  `DEF_CRYPTO_TWEAKED`). Modern macOS sets both by default.
- **Q6 NUL exclusion:** NUL terminator is **NOT** included in the CRC32C
  input. CRC32C is `crc & 0x003F_FFFF`. (`name_len` low 10 bits DOES include
  NUL.)
- **Q7 CJK / Hangul:** Hangul syllables decompose into L+V+T jamo under NFD
  (hash input expands); the stored name keeps the compact NFC Hangul.
- **Q8 RTL / Bidi:** LRM/RLM/ZWJ are preserved on disk and pass through NFD
  unchanged into the hash input.
- **Q9 Combining marks:** Stored verbatim (precomposed); hashed as fully
  decomposed NFD (e.g. Vietnamese ế NFC → NFD `e + ̂ + ́`).
- **Q10 Emoji & ZWJ sequences:** Modifiers and ZWJ (`U+200D`) do not decompose
  under NFD; pass through as standard UTF-32 scalars.
- **Q11 Non-BMP:** UTF-32-LE = exactly one 4-byte LE word per scalar
  (NOT UTF-16 surrogate pairs). Emoji 👨 (`U+1F468`) → `68 F4 01 00`.
- **Q12 Case-folding:** Use full Unicode CaseFolding (Common + Full). Rust's
  `str::to_lowercase` is generally good but NOT a guaranteed match for the
  special locales (Turkish `İ`, German `ß`, Greek final-sigma). For a
  filesystem writer, an `icu_casemap`-style fold is the safest. **Status in
  this codebase:** case-INsensitive volumes are not the common default
  (scratch incompat=0x1 IS case-insensitive but our Unicode test mainly
  exercises name preservation; full Unicode case-fold is documented as a
  follow-up if mismatches surface for case-insensitive Unicode lookups).
- **Q13 NAME_MAX:** 255 bytes stored UTF-8 (+1 NUL = 256 in DREC key). NFD
  expansion is transient and not constrained.

### Implementation note (empirical)

The doc said NFD applies "when `NORMALIZATION_INSENSITIVE` is set." Our
scratch volume (hdiutil/newfs_apfs default) reports incompat=0x1 (only
`CASE_INSENSITIVE`, NO `NORMALIZATION_INSENSITIVE`). Yet without NFD, fsck
flags Unicode-named DRECs ("Update directory record objects"). Applying NFD
**unconditionally** makes fsck CLEAN. Conclusion: the flag may gate
collision semantics, but the hash itself uses NFD universally on modern
macOS. apfs_name_hash now applies NFD unconditionally (ASCII = no-op).
