//! Unicode NFD case-fold helpers for APFS case-insensitive volume lookups.
//!
//! Derived from Unicode UAX#15 (NFD decomposition) + APFS case-insensitive
//! spec (APFS_INCOMPAT_CASE_INSENSITIVE). Apple's kernel function
//! `utf8_normalizeOptCaseFoldAndCompare` applies NFD then Unicode lowercase
//! fold before any catalog comparison on case-insensitive volumes.
//!
//! This module provides the same semantic:
//!   1. NFD decompose (Unicode canonical decomposition)
//!   2. Per-scalar FULL Unicode case fold (table of expansions, else
//!      `char::to_lowercase`)
//!   3. Collect back to UTF-8
//!
//! Key semantics (FULL Unicode case folding, which may expand a scalar into
//! several - this is what macOS applies):
//! - Plain ASCII: "FOO" → "foo"  (identical to `to_ascii_lowercase`)
//! - Umlauts: "ÄPFEL" → "a\u{308}pfel"  (NFD decomposes Ä → A + combining)
//! - Turkish İ (U+0130): folds to "i\u{307}" (i + combining dot above),
//!   NOT plain "i". So fold("İSTANBUL") ≠ fold("istanbul") - they are
//!   genuinely different Unicode names that APFS keeps distinct even on
//!   case-insensitive volumes.
//! - German ß (U+00DF): folds to "ss", so fold("straße") == fold("STRASSE").
//!   Verified on a macOS-formatted case-insensitive volume, where a file
//!   created as "straße" also answers to "STRASSE".
//! - Compatibility ligatures (ﬁ, ﬀ, ...): fold to their component letters, so
//!   a file created as "ﬁle" answers to "file" - also verified on that volume.
//! - NFC vs NFD inputs: NFD decomposition unifies both forms so
//!   "ä" (U+00E4 NFC) == "a\u{308}" (NFD) after folding.
//!
//! Summary: `apfs_names_match` folds both sides the way macOS does, so a name
//! written by either side resolves on the other. Only the scalars whose full
//! fold differs from the simple lowercase mapping need a table; everything
//! else goes through `char::to_lowercase`.

use unicode_normalization::UnicodeNormalization;

/// Produce the APFS canonical case-fold of `s`: NFD decompose, then apply
/// Unicode lowercase fold to every scalar, then collect to UTF-8.
///
/// This is the string form used for comparison on case-insensitive APFS
/// volumes (APFS_INCOMPAT_CASE_INSENSITIVE). The STORED bytes on disk are
/// ALWAYS the original UTF-8; only the comparison key is folded.
pub fn apfs_case_fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfd() {
        match full_fold(c) {
            Some(f) => out.push_str(f),
            None => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// The full case fold of `c`, when it differs from the simple lowercase
/// mapping.
///
/// Unicode defines two case foldings. The simple form maps one scalar to one
/// scalar; the full form may expand, which is what macOS applies - confirmed on
/// a case-insensitive volume it formatted, where a file created as `stra\u{df}e`
/// also answers to `STRASSE`, and one created as `\u{fb01}le` answers to `file`.
/// `char::to_lowercase` implements the simple form, so those names never
/// compared equal for us: a file macOS wrote could not be found, and a name we
/// wrote hashed differently from the one macOS computes for it.
///
/// The table holds only the scalars where the two forms disagree - everything
/// else is left to `to_lowercase`. Sorted by scalar, searched by bisection.
fn full_fold(c: char) -> Option<&'static str> {
    FULL_FOLD_EXCEPTIONS
        .binary_search_by(|(k, _)| k.cmp(&c))
        .ok()
        .and_then(|i| FULL_FOLD_EXCEPTIONS.get(i))
        .map(|(_, f)| *f)
}

/// Scalars whose Unicode FULL case fold differs from their simple lowercase
/// mapping. Derived from the Unicode character database (full folding, form
/// C+F); regenerating it is a matter of listing the scalars where the two
/// mappings differ.
#[rustfmt::skip]
const FULL_FOLD_EXCEPTIONS: &[(char, &str)] = &[
    ('\u{b5}', "\u{3bc}"), // MICRO SIGN
    ('\u{df}', "ss"), // LATIN SMALL LETTER SHARP S
    ('\u{149}', "\u{2bc}n"), // LATIN SMALL LETTER N PRECEDED BY APOSTROPHE
    ('\u{17f}', "s"), // LATIN SMALL LETTER LONG S
    ('\u{1f0}', "j\u{30c}"), // LATIN SMALL LETTER J WITH CARON
    ('\u{345}', "\u{3b9}"), // COMBINING GREEK YPOGEGRAMMENI
    ('\u{390}', "\u{3b9}\u{308}\u{301}"), // GREEK SMALL LETTER IOTA WITH DIALYTIKA AND TONOS
    ('\u{3b0}', "\u{3c5}\u{308}\u{301}"), // GREEK SMALL LETTER UPSILON WITH DIALYTIKA AND TONOS
    ('\u{3c2}', "\u{3c3}"), // GREEK SMALL LETTER FINAL SIGMA
    ('\u{3d0}', "\u{3b2}"), // GREEK BETA SYMBOL
    ('\u{3d1}', "\u{3b8}"), // GREEK THETA SYMBOL
    ('\u{3d5}', "\u{3c6}"), // GREEK PHI SYMBOL
    ('\u{3d6}', "\u{3c0}"), // GREEK PI SYMBOL
    ('\u{3f0}', "\u{3ba}"), // GREEK KAPPA SYMBOL
    ('\u{3f1}', "\u{3c1}"), // GREEK RHO SYMBOL
    ('\u{3f5}', "\u{3b5}"), // GREEK LUNATE EPSILON SYMBOL
    ('\u{587}', "\u{565}\u{582}"), // ARMENIAN SMALL LIGATURE ECH YIWN
    ('\u{13a0}', "\u{13a0}"), // CHEROKEE LETTER A
    ('\u{13a1}', "\u{13a1}"), // CHEROKEE LETTER E
    ('\u{13a2}', "\u{13a2}"), // CHEROKEE LETTER I
    ('\u{13a3}', "\u{13a3}"), // CHEROKEE LETTER O
    ('\u{13a4}', "\u{13a4}"), // CHEROKEE LETTER U
    ('\u{13a5}', "\u{13a5}"), // CHEROKEE LETTER V
    ('\u{13a6}', "\u{13a6}"), // CHEROKEE LETTER GA
    ('\u{13a7}', "\u{13a7}"), // CHEROKEE LETTER KA
    ('\u{13a8}', "\u{13a8}"), // CHEROKEE LETTER GE
    ('\u{13a9}', "\u{13a9}"), // CHEROKEE LETTER GI
    ('\u{13aa}', "\u{13aa}"), // CHEROKEE LETTER GO
    ('\u{13ab}', "\u{13ab}"), // CHEROKEE LETTER GU
    ('\u{13ac}', "\u{13ac}"), // CHEROKEE LETTER GV
    ('\u{13ad}', "\u{13ad}"), // CHEROKEE LETTER HA
    ('\u{13ae}', "\u{13ae}"), // CHEROKEE LETTER HE
    ('\u{13af}', "\u{13af}"), // CHEROKEE LETTER HI
    ('\u{13b0}', "\u{13b0}"), // CHEROKEE LETTER HO
    ('\u{13b1}', "\u{13b1}"), // CHEROKEE LETTER HU
    ('\u{13b2}', "\u{13b2}"), // CHEROKEE LETTER HV
    ('\u{13b3}', "\u{13b3}"), // CHEROKEE LETTER LA
    ('\u{13b4}', "\u{13b4}"), // CHEROKEE LETTER LE
    ('\u{13b5}', "\u{13b5}"), // CHEROKEE LETTER LI
    ('\u{13b6}', "\u{13b6}"), // CHEROKEE LETTER LO
    ('\u{13b7}', "\u{13b7}"), // CHEROKEE LETTER LU
    ('\u{13b8}', "\u{13b8}"), // CHEROKEE LETTER LV
    ('\u{13b9}', "\u{13b9}"), // CHEROKEE LETTER MA
    ('\u{13ba}', "\u{13ba}"), // CHEROKEE LETTER ME
    ('\u{13bb}', "\u{13bb}"), // CHEROKEE LETTER MI
    ('\u{13bc}', "\u{13bc}"), // CHEROKEE LETTER MO
    ('\u{13bd}', "\u{13bd}"), // CHEROKEE LETTER MU
    ('\u{13be}', "\u{13be}"), // CHEROKEE LETTER NA
    ('\u{13bf}', "\u{13bf}"), // CHEROKEE LETTER HNA
    ('\u{13c0}', "\u{13c0}"), // CHEROKEE LETTER NAH
    ('\u{13c1}', "\u{13c1}"), // CHEROKEE LETTER NE
    ('\u{13c2}', "\u{13c2}"), // CHEROKEE LETTER NI
    ('\u{13c3}', "\u{13c3}"), // CHEROKEE LETTER NO
    ('\u{13c4}', "\u{13c4}"), // CHEROKEE LETTER NU
    ('\u{13c5}', "\u{13c5}"), // CHEROKEE LETTER NV
    ('\u{13c6}', "\u{13c6}"), // CHEROKEE LETTER QUA
    ('\u{13c7}', "\u{13c7}"), // CHEROKEE LETTER QUE
    ('\u{13c8}', "\u{13c8}"), // CHEROKEE LETTER QUI
    ('\u{13c9}', "\u{13c9}"), // CHEROKEE LETTER QUO
    ('\u{13ca}', "\u{13ca}"), // CHEROKEE LETTER QUU
    ('\u{13cb}', "\u{13cb}"), // CHEROKEE LETTER QUV
    ('\u{13cc}', "\u{13cc}"), // CHEROKEE LETTER SA
    ('\u{13cd}', "\u{13cd}"), // CHEROKEE LETTER S
    ('\u{13ce}', "\u{13ce}"), // CHEROKEE LETTER SE
    ('\u{13cf}', "\u{13cf}"), // CHEROKEE LETTER SI
    ('\u{13d0}', "\u{13d0}"), // CHEROKEE LETTER SO
    ('\u{13d1}', "\u{13d1}"), // CHEROKEE LETTER SU
    ('\u{13d2}', "\u{13d2}"), // CHEROKEE LETTER SV
    ('\u{13d3}', "\u{13d3}"), // CHEROKEE LETTER DA
    ('\u{13d4}', "\u{13d4}"), // CHEROKEE LETTER TA
    ('\u{13d5}', "\u{13d5}"), // CHEROKEE LETTER DE
    ('\u{13d6}', "\u{13d6}"), // CHEROKEE LETTER TE
    ('\u{13d7}', "\u{13d7}"), // CHEROKEE LETTER DI
    ('\u{13d8}', "\u{13d8}"), // CHEROKEE LETTER TI
    ('\u{13d9}', "\u{13d9}"), // CHEROKEE LETTER DO
    ('\u{13da}', "\u{13da}"), // CHEROKEE LETTER DU
    ('\u{13db}', "\u{13db}"), // CHEROKEE LETTER DV
    ('\u{13dc}', "\u{13dc}"), // CHEROKEE LETTER DLA
    ('\u{13dd}', "\u{13dd}"), // CHEROKEE LETTER TLA
    ('\u{13de}', "\u{13de}"), // CHEROKEE LETTER TLE
    ('\u{13df}', "\u{13df}"), // CHEROKEE LETTER TLI
    ('\u{13e0}', "\u{13e0}"), // CHEROKEE LETTER TLO
    ('\u{13e1}', "\u{13e1}"), // CHEROKEE LETTER TLU
    ('\u{13e2}', "\u{13e2}"), // CHEROKEE LETTER TLV
    ('\u{13e3}', "\u{13e3}"), // CHEROKEE LETTER TSA
    ('\u{13e4}', "\u{13e4}"), // CHEROKEE LETTER TSE
    ('\u{13e5}', "\u{13e5}"), // CHEROKEE LETTER TSI
    ('\u{13e6}', "\u{13e6}"), // CHEROKEE LETTER TSO
    ('\u{13e7}', "\u{13e7}"), // CHEROKEE LETTER TSU
    ('\u{13e8}', "\u{13e8}"), // CHEROKEE LETTER TSV
    ('\u{13e9}', "\u{13e9}"), // CHEROKEE LETTER WA
    ('\u{13ea}', "\u{13ea}"), // CHEROKEE LETTER WE
    ('\u{13eb}', "\u{13eb}"), // CHEROKEE LETTER WI
    ('\u{13ec}', "\u{13ec}"), // CHEROKEE LETTER WO
    ('\u{13ed}', "\u{13ed}"), // CHEROKEE LETTER WU
    ('\u{13ee}', "\u{13ee}"), // CHEROKEE LETTER WV
    ('\u{13ef}', "\u{13ef}"), // CHEROKEE LETTER YA
    ('\u{13f0}', "\u{13f0}"), // CHEROKEE LETTER YE
    ('\u{13f1}', "\u{13f1}"), // CHEROKEE LETTER YI
    ('\u{13f2}', "\u{13f2}"), // CHEROKEE LETTER YO
    ('\u{13f3}', "\u{13f3}"), // CHEROKEE LETTER YU
    ('\u{13f4}', "\u{13f4}"), // CHEROKEE LETTER YV
    ('\u{13f5}', "\u{13f5}"), // CHEROKEE LETTER MV
    ('\u{13f8}', "\u{13f0}"), // CHEROKEE SMALL LETTER YE
    ('\u{13f9}', "\u{13f1}"), // CHEROKEE SMALL LETTER YI
    ('\u{13fa}', "\u{13f2}"), // CHEROKEE SMALL LETTER YO
    ('\u{13fb}', "\u{13f3}"), // CHEROKEE SMALL LETTER YU
    ('\u{13fc}', "\u{13f4}"), // CHEROKEE SMALL LETTER YV
    ('\u{13fd}', "\u{13f5}"), // CHEROKEE SMALL LETTER MV
    ('\u{1c80}', "\u{432}"), // CYRILLIC SMALL LETTER ROUNDED VE
    ('\u{1c81}', "\u{434}"), // CYRILLIC SMALL LETTER LONG-LEGGED DE
    ('\u{1c82}', "\u{43e}"), // CYRILLIC SMALL LETTER NARROW O
    ('\u{1c83}', "\u{441}"), // CYRILLIC SMALL LETTER WIDE ES
    ('\u{1c84}', "\u{442}"), // CYRILLIC SMALL LETTER TALL TE
    ('\u{1c85}', "\u{442}"), // CYRILLIC SMALL LETTER THREE-LEGGED TE
    ('\u{1c86}', "\u{44a}"), // CYRILLIC SMALL LETTER TALL HARD SIGN
    ('\u{1c87}', "\u{463}"), // CYRILLIC SMALL LETTER TALL YAT
    ('\u{1c88}', "\u{a64b}"), // CYRILLIC SMALL LETTER UNBLENDED UK
    ('\u{1e96}', "h\u{331}"), // LATIN SMALL LETTER H WITH LINE BELOW
    ('\u{1e97}', "t\u{308}"), // LATIN SMALL LETTER T WITH DIAERESIS
    ('\u{1e98}', "w\u{30a}"), // LATIN SMALL LETTER W WITH RING ABOVE
    ('\u{1e99}', "y\u{30a}"), // LATIN SMALL LETTER Y WITH RING ABOVE
    ('\u{1e9a}', "a\u{2be}"), // LATIN SMALL LETTER A WITH RIGHT HALF RING
    ('\u{1e9b}', "\u{1e61}"), // LATIN SMALL LETTER LONG S WITH DOT ABOVE
    ('\u{1e9e}', "ss"), // LATIN CAPITAL LETTER SHARP S
    ('\u{1f50}', "\u{3c5}\u{313}"), // GREEK SMALL LETTER UPSILON WITH PSILI
    ('\u{1f52}', "\u{3c5}\u{313}\u{300}"), // GREEK SMALL LETTER UPSILON WITH PSILI AND VARIA
    ('\u{1f54}', "\u{3c5}\u{313}\u{301}"), // GREEK SMALL LETTER UPSILON WITH PSILI AND OXIA
    ('\u{1f56}', "\u{3c5}\u{313}\u{342}"), // GREEK SMALL LETTER UPSILON WITH PSILI AND PERISPOMENI
    ('\u{1f80}', "\u{1f00}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH PSILI AND YPOGEGRAMMENI
    ('\u{1f81}', "\u{1f01}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH DASIA AND YPOGEGRAMMENI
    ('\u{1f82}', "\u{1f02}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH PSILI AND VARIA AND YPOGEGRAMMENI
    ('\u{1f83}', "\u{1f03}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH DASIA AND VARIA AND YPOGEGRAMMENI
    ('\u{1f84}', "\u{1f04}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH PSILI AND OXIA AND YPOGEGRAMMENI
    ('\u{1f85}', "\u{1f05}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH DASIA AND OXIA AND YPOGEGRAMMENI
    ('\u{1f86}', "\u{1f06}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH PSILI AND PERISPOMENI AND YPOGEGRAMMENI
    ('\u{1f87}', "\u{1f07}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH DASIA AND PERISPOMENI AND YPOGEGRAMMENI
    ('\u{1f88}', "\u{1f00}\u{3b9}"), // GREEK CAPITAL LETTER ALPHA WITH PSILI AND PROSGEGRAMMENI
    ('\u{1f89}', "\u{1f01}\u{3b9}"), // GREEK CAPITAL LETTER ALPHA WITH DASIA AND PROSGEGRAMMENI
    ('\u{1f8a}', "\u{1f02}\u{3b9}"), // GREEK CAPITAL LETTER ALPHA WITH PSILI AND VARIA AND PROSGEGRAMMENI
    ('\u{1f8b}', "\u{1f03}\u{3b9}"), // GREEK CAPITAL LETTER ALPHA WITH DASIA AND VARIA AND PROSGEGRAMMENI
    ('\u{1f8c}', "\u{1f04}\u{3b9}"), // GREEK CAPITAL LETTER ALPHA WITH PSILI AND OXIA AND PROSGEGRAMMENI
    ('\u{1f8d}', "\u{1f05}\u{3b9}"), // GREEK CAPITAL LETTER ALPHA WITH DASIA AND OXIA AND PROSGEGRAMMENI
    ('\u{1f8e}', "\u{1f06}\u{3b9}"), // GREEK CAPITAL LETTER ALPHA WITH PSILI AND PERISPOMENI AND PROSGEGRAMMENI
    ('\u{1f8f}', "\u{1f07}\u{3b9}"), // GREEK CAPITAL LETTER ALPHA WITH DASIA AND PERISPOMENI AND PROSGEGRAMMENI
    ('\u{1f90}', "\u{1f20}\u{3b9}"), // GREEK SMALL LETTER ETA WITH PSILI AND YPOGEGRAMMENI
    ('\u{1f91}', "\u{1f21}\u{3b9}"), // GREEK SMALL LETTER ETA WITH DASIA AND YPOGEGRAMMENI
    ('\u{1f92}', "\u{1f22}\u{3b9}"), // GREEK SMALL LETTER ETA WITH PSILI AND VARIA AND YPOGEGRAMMENI
    ('\u{1f93}', "\u{1f23}\u{3b9}"), // GREEK SMALL LETTER ETA WITH DASIA AND VARIA AND YPOGEGRAMMENI
    ('\u{1f94}', "\u{1f24}\u{3b9}"), // GREEK SMALL LETTER ETA WITH PSILI AND OXIA AND YPOGEGRAMMENI
    ('\u{1f95}', "\u{1f25}\u{3b9}"), // GREEK SMALL LETTER ETA WITH DASIA AND OXIA AND YPOGEGRAMMENI
    ('\u{1f96}', "\u{1f26}\u{3b9}"), // GREEK SMALL LETTER ETA WITH PSILI AND PERISPOMENI AND YPOGEGRAMMENI
    ('\u{1f97}', "\u{1f27}\u{3b9}"), // GREEK SMALL LETTER ETA WITH DASIA AND PERISPOMENI AND YPOGEGRAMMENI
    ('\u{1f98}', "\u{1f20}\u{3b9}"), // GREEK CAPITAL LETTER ETA WITH PSILI AND PROSGEGRAMMENI
    ('\u{1f99}', "\u{1f21}\u{3b9}"), // GREEK CAPITAL LETTER ETA WITH DASIA AND PROSGEGRAMMENI
    ('\u{1f9a}', "\u{1f22}\u{3b9}"), // GREEK CAPITAL LETTER ETA WITH PSILI AND VARIA AND PROSGEGRAMMENI
    ('\u{1f9b}', "\u{1f23}\u{3b9}"), // GREEK CAPITAL LETTER ETA WITH DASIA AND VARIA AND PROSGEGRAMMENI
    ('\u{1f9c}', "\u{1f24}\u{3b9}"), // GREEK CAPITAL LETTER ETA WITH PSILI AND OXIA AND PROSGEGRAMMENI
    ('\u{1f9d}', "\u{1f25}\u{3b9}"), // GREEK CAPITAL LETTER ETA WITH DASIA AND OXIA AND PROSGEGRAMMENI
    ('\u{1f9e}', "\u{1f26}\u{3b9}"), // GREEK CAPITAL LETTER ETA WITH PSILI AND PERISPOMENI AND PROSGEGRAMMENI
    ('\u{1f9f}', "\u{1f27}\u{3b9}"), // GREEK CAPITAL LETTER ETA WITH DASIA AND PERISPOMENI AND PROSGEGRAMMENI
    ('\u{1fa0}', "\u{1f60}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH PSILI AND YPOGEGRAMMENI
    ('\u{1fa1}', "\u{1f61}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH DASIA AND YPOGEGRAMMENI
    ('\u{1fa2}', "\u{1f62}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH PSILI AND VARIA AND YPOGEGRAMMENI
    ('\u{1fa3}', "\u{1f63}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH DASIA AND VARIA AND YPOGEGRAMMENI
    ('\u{1fa4}', "\u{1f64}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH PSILI AND OXIA AND YPOGEGRAMMENI
    ('\u{1fa5}', "\u{1f65}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH DASIA AND OXIA AND YPOGEGRAMMENI
    ('\u{1fa6}', "\u{1f66}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH PSILI AND PERISPOMENI AND YPOGEGRAMMENI
    ('\u{1fa7}', "\u{1f67}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH DASIA AND PERISPOMENI AND YPOGEGRAMMENI
    ('\u{1fa8}', "\u{1f60}\u{3b9}"), // GREEK CAPITAL LETTER OMEGA WITH PSILI AND PROSGEGRAMMENI
    ('\u{1fa9}', "\u{1f61}\u{3b9}"), // GREEK CAPITAL LETTER OMEGA WITH DASIA AND PROSGEGRAMMENI
    ('\u{1faa}', "\u{1f62}\u{3b9}"), // GREEK CAPITAL LETTER OMEGA WITH PSILI AND VARIA AND PROSGEGRAMMENI
    ('\u{1fab}', "\u{1f63}\u{3b9}"), // GREEK CAPITAL LETTER OMEGA WITH DASIA AND VARIA AND PROSGEGRAMMENI
    ('\u{1fac}', "\u{1f64}\u{3b9}"), // GREEK CAPITAL LETTER OMEGA WITH PSILI AND OXIA AND PROSGEGRAMMENI
    ('\u{1fad}', "\u{1f65}\u{3b9}"), // GREEK CAPITAL LETTER OMEGA WITH DASIA AND OXIA AND PROSGEGRAMMENI
    ('\u{1fae}', "\u{1f66}\u{3b9}"), // GREEK CAPITAL LETTER OMEGA WITH PSILI AND PERISPOMENI AND PROSGEGRAMMENI
    ('\u{1faf}', "\u{1f67}\u{3b9}"), // GREEK CAPITAL LETTER OMEGA WITH DASIA AND PERISPOMENI AND PROSGEGRAMMENI
    ('\u{1fb2}', "\u{1f70}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH VARIA AND YPOGEGRAMMENI
    ('\u{1fb3}', "\u{3b1}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH YPOGEGRAMMENI
    ('\u{1fb4}', "\u{3ac}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH OXIA AND YPOGEGRAMMENI
    ('\u{1fb6}', "\u{3b1}\u{342}"), // GREEK SMALL LETTER ALPHA WITH PERISPOMENI
    ('\u{1fb7}', "\u{3b1}\u{342}\u{3b9}"), // GREEK SMALL LETTER ALPHA WITH PERISPOMENI AND YPOGEGRAMMENI
    ('\u{1fbc}', "\u{3b1}\u{3b9}"), // GREEK CAPITAL LETTER ALPHA WITH PROSGEGRAMMENI
    ('\u{1fbe}', "\u{3b9}"), // GREEK PROSGEGRAMMENI
    ('\u{1fc2}', "\u{1f74}\u{3b9}"), // GREEK SMALL LETTER ETA WITH VARIA AND YPOGEGRAMMENI
    ('\u{1fc3}', "\u{3b7}\u{3b9}"), // GREEK SMALL LETTER ETA WITH YPOGEGRAMMENI
    ('\u{1fc4}', "\u{3ae}\u{3b9}"), // GREEK SMALL LETTER ETA WITH OXIA AND YPOGEGRAMMENI
    ('\u{1fc6}', "\u{3b7}\u{342}"), // GREEK SMALL LETTER ETA WITH PERISPOMENI
    ('\u{1fc7}', "\u{3b7}\u{342}\u{3b9}"), // GREEK SMALL LETTER ETA WITH PERISPOMENI AND YPOGEGRAMMENI
    ('\u{1fcc}', "\u{3b7}\u{3b9}"), // GREEK CAPITAL LETTER ETA WITH PROSGEGRAMMENI
    ('\u{1fd2}', "\u{3b9}\u{308}\u{300}"), // GREEK SMALL LETTER IOTA WITH DIALYTIKA AND VARIA
    ('\u{1fd3}', "\u{3b9}\u{308}\u{301}"), // GREEK SMALL LETTER IOTA WITH DIALYTIKA AND OXIA
    ('\u{1fd6}', "\u{3b9}\u{342}"), // GREEK SMALL LETTER IOTA WITH PERISPOMENI
    ('\u{1fd7}', "\u{3b9}\u{308}\u{342}"), // GREEK SMALL LETTER IOTA WITH DIALYTIKA AND PERISPOMENI
    ('\u{1fe2}', "\u{3c5}\u{308}\u{300}"), // GREEK SMALL LETTER UPSILON WITH DIALYTIKA AND VARIA
    ('\u{1fe3}', "\u{3c5}\u{308}\u{301}"), // GREEK SMALL LETTER UPSILON WITH DIALYTIKA AND OXIA
    ('\u{1fe4}', "\u{3c1}\u{313}"), // GREEK SMALL LETTER RHO WITH PSILI
    ('\u{1fe6}', "\u{3c5}\u{342}"), // GREEK SMALL LETTER UPSILON WITH PERISPOMENI
    ('\u{1fe7}', "\u{3c5}\u{308}\u{342}"), // GREEK SMALL LETTER UPSILON WITH DIALYTIKA AND PERISPOMENI
    ('\u{1ff2}', "\u{1f7c}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH VARIA AND YPOGEGRAMMENI
    ('\u{1ff3}', "\u{3c9}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH YPOGEGRAMMENI
    ('\u{1ff4}', "\u{3ce}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH OXIA AND YPOGEGRAMMENI
    ('\u{1ff6}', "\u{3c9}\u{342}"), // GREEK SMALL LETTER OMEGA WITH PERISPOMENI
    ('\u{1ff7}', "\u{3c9}\u{342}\u{3b9}"), // GREEK SMALL LETTER OMEGA WITH PERISPOMENI AND YPOGEGRAMMENI
    ('\u{1ffc}', "\u{3c9}\u{3b9}"), // GREEK CAPITAL LETTER OMEGA WITH PROSGEGRAMMENI
    ('\u{ab70}', "\u{13a0}"), // CHEROKEE SMALL LETTER A
    ('\u{ab71}', "\u{13a1}"), // CHEROKEE SMALL LETTER E
    ('\u{ab72}', "\u{13a2}"), // CHEROKEE SMALL LETTER I
    ('\u{ab73}', "\u{13a3}"), // CHEROKEE SMALL LETTER O
    ('\u{ab74}', "\u{13a4}"), // CHEROKEE SMALL LETTER U
    ('\u{ab75}', "\u{13a5}"), // CHEROKEE SMALL LETTER V
    ('\u{ab76}', "\u{13a6}"), // CHEROKEE SMALL LETTER GA
    ('\u{ab77}', "\u{13a7}"), // CHEROKEE SMALL LETTER KA
    ('\u{ab78}', "\u{13a8}"), // CHEROKEE SMALL LETTER GE
    ('\u{ab79}', "\u{13a9}"), // CHEROKEE SMALL LETTER GI
    ('\u{ab7a}', "\u{13aa}"), // CHEROKEE SMALL LETTER GO
    ('\u{ab7b}', "\u{13ab}"), // CHEROKEE SMALL LETTER GU
    ('\u{ab7c}', "\u{13ac}"), // CHEROKEE SMALL LETTER GV
    ('\u{ab7d}', "\u{13ad}"), // CHEROKEE SMALL LETTER HA
    ('\u{ab7e}', "\u{13ae}"), // CHEROKEE SMALL LETTER HE
    ('\u{ab7f}', "\u{13af}"), // CHEROKEE SMALL LETTER HI
    ('\u{ab80}', "\u{13b0}"), // CHEROKEE SMALL LETTER HO
    ('\u{ab81}', "\u{13b1}"), // CHEROKEE SMALL LETTER HU
    ('\u{ab82}', "\u{13b2}"), // CHEROKEE SMALL LETTER HV
    ('\u{ab83}', "\u{13b3}"), // CHEROKEE SMALL LETTER LA
    ('\u{ab84}', "\u{13b4}"), // CHEROKEE SMALL LETTER LE
    ('\u{ab85}', "\u{13b5}"), // CHEROKEE SMALL LETTER LI
    ('\u{ab86}', "\u{13b6}"), // CHEROKEE SMALL LETTER LO
    ('\u{ab87}', "\u{13b7}"), // CHEROKEE SMALL LETTER LU
    ('\u{ab88}', "\u{13b8}"), // CHEROKEE SMALL LETTER LV
    ('\u{ab89}', "\u{13b9}"), // CHEROKEE SMALL LETTER MA
    ('\u{ab8a}', "\u{13ba}"), // CHEROKEE SMALL LETTER ME
    ('\u{ab8b}', "\u{13bb}"), // CHEROKEE SMALL LETTER MI
    ('\u{ab8c}', "\u{13bc}"), // CHEROKEE SMALL LETTER MO
    ('\u{ab8d}', "\u{13bd}"), // CHEROKEE SMALL LETTER MU
    ('\u{ab8e}', "\u{13be}"), // CHEROKEE SMALL LETTER NA
    ('\u{ab8f}', "\u{13bf}"), // CHEROKEE SMALL LETTER HNA
    ('\u{ab90}', "\u{13c0}"), // CHEROKEE SMALL LETTER NAH
    ('\u{ab91}', "\u{13c1}"), // CHEROKEE SMALL LETTER NE
    ('\u{ab92}', "\u{13c2}"), // CHEROKEE SMALL LETTER NI
    ('\u{ab93}', "\u{13c3}"), // CHEROKEE SMALL LETTER NO
    ('\u{ab94}', "\u{13c4}"), // CHEROKEE SMALL LETTER NU
    ('\u{ab95}', "\u{13c5}"), // CHEROKEE SMALL LETTER NV
    ('\u{ab96}', "\u{13c6}"), // CHEROKEE SMALL LETTER QUA
    ('\u{ab97}', "\u{13c7}"), // CHEROKEE SMALL LETTER QUE
    ('\u{ab98}', "\u{13c8}"), // CHEROKEE SMALL LETTER QUI
    ('\u{ab99}', "\u{13c9}"), // CHEROKEE SMALL LETTER QUO
    ('\u{ab9a}', "\u{13ca}"), // CHEROKEE SMALL LETTER QUU
    ('\u{ab9b}', "\u{13cb}"), // CHEROKEE SMALL LETTER QUV
    ('\u{ab9c}', "\u{13cc}"), // CHEROKEE SMALL LETTER SA
    ('\u{ab9d}', "\u{13cd}"), // CHEROKEE SMALL LETTER S
    ('\u{ab9e}', "\u{13ce}"), // CHEROKEE SMALL LETTER SE
    ('\u{ab9f}', "\u{13cf}"), // CHEROKEE SMALL LETTER SI
    ('\u{aba0}', "\u{13d0}"), // CHEROKEE SMALL LETTER SO
    ('\u{aba1}', "\u{13d1}"), // CHEROKEE SMALL LETTER SU
    ('\u{aba2}', "\u{13d2}"), // CHEROKEE SMALL LETTER SV
    ('\u{aba3}', "\u{13d3}"), // CHEROKEE SMALL LETTER DA
    ('\u{aba4}', "\u{13d4}"), // CHEROKEE SMALL LETTER TA
    ('\u{aba5}', "\u{13d5}"), // CHEROKEE SMALL LETTER DE
    ('\u{aba6}', "\u{13d6}"), // CHEROKEE SMALL LETTER TE
    ('\u{aba7}', "\u{13d7}"), // CHEROKEE SMALL LETTER DI
    ('\u{aba8}', "\u{13d8}"), // CHEROKEE SMALL LETTER TI
    ('\u{aba9}', "\u{13d9}"), // CHEROKEE SMALL LETTER DO
    ('\u{abaa}', "\u{13da}"), // CHEROKEE SMALL LETTER DU
    ('\u{abab}', "\u{13db}"), // CHEROKEE SMALL LETTER DV
    ('\u{abac}', "\u{13dc}"), // CHEROKEE SMALL LETTER DLA
    ('\u{abad}', "\u{13dd}"), // CHEROKEE SMALL LETTER TLA
    ('\u{abae}', "\u{13de}"), // CHEROKEE SMALL LETTER TLE
    ('\u{abaf}', "\u{13df}"), // CHEROKEE SMALL LETTER TLI
    ('\u{abb0}', "\u{13e0}"), // CHEROKEE SMALL LETTER TLO
    ('\u{abb1}', "\u{13e1}"), // CHEROKEE SMALL LETTER TLU
    ('\u{abb2}', "\u{13e2}"), // CHEROKEE SMALL LETTER TLV
    ('\u{abb3}', "\u{13e3}"), // CHEROKEE SMALL LETTER TSA
    ('\u{abb4}', "\u{13e4}"), // CHEROKEE SMALL LETTER TSE
    ('\u{abb5}', "\u{13e5}"), // CHEROKEE SMALL LETTER TSI
    ('\u{abb6}', "\u{13e6}"), // CHEROKEE SMALL LETTER TSO
    ('\u{abb7}', "\u{13e7}"), // CHEROKEE SMALL LETTER TSU
    ('\u{abb8}', "\u{13e8}"), // CHEROKEE SMALL LETTER TSV
    ('\u{abb9}', "\u{13e9}"), // CHEROKEE SMALL LETTER WA
    ('\u{abba}', "\u{13ea}"), // CHEROKEE SMALL LETTER WE
    ('\u{abbb}', "\u{13eb}"), // CHEROKEE SMALL LETTER WI
    ('\u{abbc}', "\u{13ec}"), // CHEROKEE SMALL LETTER WO
    ('\u{abbd}', "\u{13ed}"), // CHEROKEE SMALL LETTER WU
    ('\u{abbe}', "\u{13ee}"), // CHEROKEE SMALL LETTER WV
    ('\u{abbf}', "\u{13ef}"), // CHEROKEE SMALL LETTER YA
    ('\u{fb00}', "ff"), // LATIN SMALL LIGATURE FF
    ('\u{fb01}', "fi"), // LATIN SMALL LIGATURE FI
    ('\u{fb02}', "fl"), // LATIN SMALL LIGATURE FL
    ('\u{fb03}', "ffi"), // LATIN SMALL LIGATURE FFI
    ('\u{fb04}', "ffl"), // LATIN SMALL LIGATURE FFL
    ('\u{fb05}', "st"), // LATIN SMALL LIGATURE LONG S T
    ('\u{fb06}', "st"), // LATIN SMALL LIGATURE ST
    ('\u{fb13}', "\u{574}\u{576}"), // ARMENIAN SMALL LIGATURE MEN NOW
    ('\u{fb14}', "\u{574}\u{565}"), // ARMENIAN SMALL LIGATURE MEN ECH
    ('\u{fb15}', "\u{574}\u{56b}"), // ARMENIAN SMALL LIGATURE MEN INI
    ('\u{fb16}', "\u{57e}\u{576}"), // ARMENIAN SMALL LIGATURE VEW NOW
    ('\u{fb17}', "\u{574}\u{56d}"), // ARMENIAN SMALL LIGATURE MEN XEH
];

/// Return `true` if `a` and `b` are equal under APFS case-insensitive
/// comparison (NFD case-fold of both sides match).
pub fn apfs_names_match(a: &str, b: &str) -> bool {
    apfs_case_fold(a) == apfs_case_fold(b)
}

/// Produce the APFS canonical form of `s` WITHOUT folding case: NFD decompose
/// only.
///
/// This is the comparison key for a volume that hashes its names but is
/// case-SENSITIVE (the `APFSX` personality, which sets only
/// NORMALIZATION_INSENSITIVE). Such a volume still needs NFC and NFD spellings
/// of the same name unified, but `README` and `readme` are two distinct files.
pub fn apfs_normalize(s: &str) -> String {
    s.nfd().collect()
}

/// Return `true` if `a` and `b` are the same name on a volume that normalizes
/// but does not fold case.
pub fn apfs_names_match_case_sensitive(a: &str, b: &str) -> bool {
    apfs_normalize(a) == apfs_normalize(b)
}

/// The 22-bit hash a hashed directory record stores for `name`.
///
/// CRC32C (Castagnoli, reflected, initial value all ones, no final xor) over
/// the UTF-32-LE scalars of the name in the form the volume compares names:
/// the full case fold after NFD on a case-insensitive volume (`case_fold`),
/// NFD alone on a case-sensitive one that normalizes (`normalize`). The
/// stored name keeps the bytes it was given; only the hash uses this form.
/// [measured on macOS: fsck_apfs checks "straße" against the hash of
///  "strasse"; matched baseline values private-dir=0x2b29a3, root=0x2d9c79]
pub fn apfs_name_hash(name: &str, case_fold: bool, normalize: bool) -> u32 {
    let form: String = if case_fold {
        apfs_case_fold(name)
    } else if normalize {
        apfs_normalize(name)
    } else {
        name.to_string()
    };
    let mut crc: u32 = 0xFFFF_FFFF;
    for ch in form.chars() {
        for b in (ch as u32).to_le_bytes() {
            let entry = CRC32C_TABLE
                .get(usize::from((crc as u8) ^ b))
                .copied()
                .unwrap_or(0);
            crc = (crc >> 8) ^ entry;
        }
    }
    crc & NAME_HASH_MASK
}

/// The bits of `name_len_and_hash` the hash takes, shifted down.
const NAME_HASH_MASK: u32 = 0x003F_FFFF;

/// CRC32C (Castagnoli) reflected polynomial.
const CRC32C_POLY: u32 = 0x82F6_3B78;

/// The CRC32C table, one entry per byte value.
// Evaluated at compile time with `i < 256`: an out-of-range index would fail
// the build, never a running program.
#[allow(clippy::indexing_slicing)]
const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ CRC32C_POLY
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

#[cfg(test)]
mod tests {
    use super::{apfs_case_fold, apfs_name_hash, apfs_names_match};

    // Hashes macOS stored on a freshly formatted volume.
    #[test]
    fn name_hashes_match_the_ones_macos_stores() {
        assert_eq!(apfs_name_hash("private-dir", true, true), 0x2b29a3);
        assert_eq!(apfs_name_hash("root", true, true), 0x2d9c79);
        // Case and normalization variants of one name hash alike when folded.
        assert_eq!(
            apfs_name_hash("STRASSE", true, true),
            apfs_name_hash("straße", true, true)
        );
        assert_eq!(
            apfs_name_hash("cafe\u{301}", true, true),
            apfs_name_hash("caf\u{e9}", true, true)
        );
    }

    // --- ASCII regression (must still work) ---

    #[test]
    fn ascii_basic() {
        assert!(apfs_names_match("Foo.txt", "foo.txt"));
        assert!(apfs_names_match("FOO", "foo"));
        assert!(apfs_names_match("hello.rs", "HELLO.RS"));
        assert!(!apfs_names_match("foo.txt", "bar.txt"));
    }

    // --- Turkish İ (U+0130 LATIN CAPITAL LETTER I WITH DOT ABOVE) ---
    // Unicode SpecialCasing: İ lowercases to "i\u{307}" (two scalars).
    // This is DIFFERENT from plain "i", so İSTANBUL ≠ istanbul on APFS -
    // they are genuinely distinct names that the filesystem keeps separate.
    // What DOES match is the same name with different ASCII casing variations.

    #[test]
    fn turkish_dotted_capital_i_fold_value() {
        // Confirm the fold value: U+0130 → i + combining dot above (U+0307).
        let folded = apfs_case_fold("\u{0130}");
        assert_eq!(folded, "i\u{0307}", "İ should fold to i + combining dot");
    }

    #[test]
    fn turkish_dotted_i_same_name_matches() {
        // Same name written with different case of the dotted-I matches.
        assert!(apfs_names_match("\u{0130}stanbul", "\u{0130}STANBUL"));
    }

    #[test]
    fn turkish_dotted_i_differs_from_plain_i() {
        // İ (U+0130) and I (U+0049) fold to different strings - APFS keeps
        // them distinct even on case-insensitive volumes.
        assert!(
            !apfs_names_match("\u{0130}STANBUL", "istanbul"),
            "İSTANBUL and istanbul are distinct Unicode names"
        );
    }

    // --- German ß (U+00DF) ---
    // macOS applies FULL Unicode case folding, which expands: ß folds to ss.
    // Verified against the arbiter - on a case-insensitive volume macOS
    // formatted, a file created as "straße" also answers to "STRASSE", and one
    // created as "ﬁle" answers to "file".

    #[test]
    fn german_eszett_fold_value() {
        assert_eq!(apfs_case_fold("ß"), "ss");
        assert_eq!(apfs_case_fold("ẞ"), "ss");
    }

    #[test]
    fn german_eszett_same_name_matches() {
        assert!(apfs_names_match("straße", "straße"));
        assert!(apfs_names_match("Größe", "größe"));
        assert!(apfs_names_match("MESSERSTRAßE", "messerstraße"));
    }

    #[test]
    fn german_eszett_matches_ss() {
        // The name macOS resolves to the same file must resolve to the same
        // file here: this is what let a macOS-written "straße" be found.
        assert!(
            apfs_names_match("straße", "STRASSE"),
            "macOS folds ß to ss, so these are one name"
        );
        assert!(apfs_names_match("STRASSE", "strasse"));
    }

    #[test]
    fn compatibility_ligatures_fold_to_their_letters() {
        // Confirmed on the same volume: a file created as "ﬁle" answers to "file".
        assert_eq!(apfs_case_fold("ﬁle"), "file");
        assert!(apfs_names_match("ﬁle", "FILE"));
        assert_eq!(apfs_case_fold("ﬀ"), "ff");
    }

    #[test]
    fn full_fold_leaves_ordinary_names_alone() {
        // The exception table must not disturb the common path.
        assert_eq!(apfs_case_fold("ReadMe.TXT"), "readme.txt");
        assert_eq!(apfs_case_fold("dosya"), "dosya");
    }

    // --- Umlauts (ä ö ü Ä Ö Ü) - these DO round-trip via NFD decompose ---
    // NFD: Ä → A + combining diaeresis, then lowercase A → a.
    // So fold("ÄPFEL") = "a\u{308}pfel" and fold("äpfel") = "a\u{308}pfel" ✓

    #[test]
    fn german_umlauts() {
        assert!(apfs_names_match("ÄPFEL", "äpfel"));
        assert!(apfs_names_match("Über", "über"));
        assert!(apfs_names_match("MÜNCHEN", "münchen"));
        assert!(apfs_names_match("GRÖSSER", "grösser"));
    }

    // --- Turkish lower-dotless-i / upper-I are DIFFERENT codepoints ---
    // U+0049 I  → U+0069 i   (ASCII)
    // U+0130 İ  → i + U+0307 (dotted capital → dotted lower, two scalars)
    // U+0131 ı  → U+0131 ı   (dotless lowercase stays dotless)

    #[test]
    fn turkish_dotless_i_not_confused() {
        // dotless-ı (U+0131) lowercases to itself; must NOT equal regular i.
        assert!(!apfs_names_match("ı", "i"));
    }

    // --- Greek capital letters → lowercase ---
    // Greek letters DO fold correctly via Rust to_lowercase.

    #[test]
    fn greek_sigma() {
        assert!(apfs_names_match("ΣΎΝΘΕΣΗ", "σύνθεση"));
    }

    // --- NFD vs NFC input normalization ---
    // macOS Finder stores filenames in NFD; AFP and some apps produce NFC.
    // NFD step unifies both before lowercase fold - critical for macOS interop.

    #[test]
    fn nfd_vs_nfc_input_match() {
        // "ä" NFC = U+00E4 (precomposed); NFD = U+0061 + U+0308.
        let nfc = "\u{00E4}pfel";
        let nfd = "a\u{0308}pfel";
        assert!(apfs_names_match(nfc, nfd));
        // Uppercase NFC vs lowercase NFD also matches.
        assert!(apfs_names_match("\u{00C4}PFEL", nfd));
    }

    #[test]
    fn nfc_nfd_umlaut_roundtrip() {
        // NFC "Ö" (U+00D6) vs NFD "O\u{308}" - same after NFD step.
        assert!(apfs_names_match("\u{00D6}sterreich", "O\u{0308}sterreich"));
        assert!(apfs_names_match("\u{00D6}STERREICH", "\u{00F6}sterreich"));
    }

    // --- Accented Latin letters that fold correctly ---

    #[test]
    fn accented_latin_fold() {
        let pairs = [
            ("café", "CAFÉ"),
            ("naïve", "NAÏVE"),
            ("résumé", "RÉSUMÉ"),
            ("GARÇON", "garçon"),
            ("MĂR", "măr"),
        ];
        for (a, b) in pairs {
            assert!(
                apfs_names_match(a, b),
                "{a:?} vs {b:?} should match under Unicode NFD fold"
            );
        }
    }
}
