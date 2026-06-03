//! Unicode NFD case-fold helpers for APFS case-insensitive volume lookups.
//!
//! Derived from Unicode UAX#15 (NFD decomposition) + APFS case-insensitive
//! spec (APFS_INCOMPAT_CASE_INSENSITIVE). Apple's kernel function
//! `utf8_normalizeOptCaseFoldAndCompare` applies NFD then Unicode lowercase
//! fold before any catalog comparison on case-insensitive volumes.
//!
//! This module provides the same semantic:
//!   1. NFD decompose (Unicode canonical decomposition)
//!   2. Per-scalar Unicode lowercase fold (`char::to_lowercase`)
//!   3. Collect back to UTF-8
//!
//! Key semantics (Rust `char::to_lowercase` follows Unicode SpecialCasing,
//! NOT full CaseFolding.txt):
//! - Plain ASCII: "FOO" → "foo"  (identical to `to_ascii_lowercase`)
//! - Umlauts: "ÄPFEL" → "a\u{308}pfel"  (NFD decomposes Ä → A + combining)
//! - Turkish İ (U+0130): folds to "i\u{307}" (i + combining dot above),
//!   NOT plain "i". So fold("İSTANBUL") ≠ fold("istanbul") - they are
//!   genuinely different Unicode names that APFS keeps distinct even on
//!   case-insensitive volumes.
//! - German ß (U+00DF): `to_lowercase` returns ß unchanged (Rust follows
//!   Unicode SpecialCasing lowercase, not full case-fold; ß→ss is only in
//!   Unicode CaseFolding.txt). fold("straße") = "straße"; fold("STRASSE") =
//!   "strasse" - different strings, correctly kept distinct by APFS.
//! - NFC vs NFD inputs: NFD decomposition unifies both forms so
//!   "ä" (U+00E4 NFC) == "a\u{308}" (NFD) after folding.
//!
//! Summary: `apfs_names_match` is correct for APFS case-insensitive lookup -
//! both sides receive the same NFD+lowercase transform, so names written with
//! the same original casing round-trip correctly. Names that are genuinely
//! different under Unicode SpecialCasing (İ vs i, ß vs SS) remain distinct.

use unicode_normalization::UnicodeNormalization;

/// Produce the APFS canonical case-fold of `s`: NFD decompose, then apply
/// Unicode lowercase fold to every scalar, then collect to UTF-8.
///
/// This is the string form used for comparison on case-insensitive APFS
/// volumes (APFS_INCOMPAT_CASE_INSENSITIVE). The STORED bytes on disk are
/// ALWAYS the original UTF-8; only the comparison key is folded.
pub fn apfs_case_fold(s: &str) -> String {
    s.nfd().flat_map(|c| c.to_lowercase()).collect()
}

/// Return `true` if `a` and `b` are equal under APFS case-insensitive
/// comparison (NFD case-fold of both sides match).
pub fn apfs_names_match(a: &str, b: &str) -> bool {
    apfs_case_fold(a) == apfs_case_fold(b)
}

#[cfg(test)]
mod tests {
    use super::{apfs_case_fold, apfs_names_match};

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
    // Rust `char::to_lowercase` follows Unicode SpecialCasing (NOT full
    // CaseFolding.txt): ß lowercases to ß (itself), not ss.
    // So fold("straße") = "straße" and fold("STRASSE") = "strasse" - distinct.
    // ß and SS are genuinely different names on APFS case-insensitive volumes.

    #[test]
    fn german_eszett_fold_value() {
        // ß to_lowercase = "ß" (unchanged by Unicode SpecialCasing lowercase).
        let folded = apfs_case_fold("ß");
        assert_eq!(folded, "ß");
    }

    #[test]
    fn german_eszett_same_name_matches() {
        // A name with ß matches itself with different surrounding ASCII casing.
        // fold("straße") = "straße"; fold("STRASSE") = "strasse" - different.
        // But fold("straße") = fold("straße") - same string, trivially true.
        assert!(apfs_names_match("straße", "straße"));
        // "Größe" vs "größe" - same ß, different leading-char casing.
        assert!(apfs_names_match("Größe", "größe"));
        // Surrounding ASCII letters fold correctly even when ß is present.
        assert!(apfs_names_match("MESSERSTRAßE", "messerstraße"));
    }

    #[test]
    fn german_eszett_differs_from_ss() {
        // ß and SS are genuinely different Unicode names - APFS keeps them
        // distinct (fold("ß") = "ß"; fold("SS") = "ss").
        assert!(
            !apfs_names_match("straße", "STRASSE"),
            "straße and STRASSE are distinct Unicode names"
        );
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
            ("ÇUKURBAĞ", "çukurbağ"),
        ];
        for (a, b) in pairs {
            assert!(
                apfs_names_match(a, b),
                "{a:?} vs {b:?} should match under Unicode NFD fold"
            );
        }
    }
}
