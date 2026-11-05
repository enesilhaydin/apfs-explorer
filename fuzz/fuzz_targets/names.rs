//! Name folding, normalization and hashing on arbitrary names.
#![no_main]

use apfs_core::case_fold::{
    apfs_case_fold, apfs_name_hash, apfs_names_match, apfs_names_match_case_sensitive,
    apfs_normalize,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let (a, b) = s.split_at(s.char_indices().nth(s.chars().count() / 2).map_or(0, |(i, _)| i));
    let _ = apfs_case_fold(&s);
    let _ = apfs_normalize(&s);
    let _ = apfs_names_match(a, b);
    let _ = apfs_names_match_case_sensitive(a, b);
    for (fold, norm) in [(false, false), (false, true), (true, false), (true, true)] {
        let _ = apfs_name_hash(&s, fold, norm);
    }
});
