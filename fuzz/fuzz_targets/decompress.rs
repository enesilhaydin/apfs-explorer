//! The decompressors on arbitrary input: a compressed file's bytes come
//! from the drive. The first byte picks the decoder and the next three the
//! expected size (capped, as a reader caps it by the file's size).
#![no_main]

use apfs_core::decmpfs::{decompress, decompress_range};
use apfs_core::{lzbitmap_decode, lzvn_decode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&pick, rest)) = data.split_first() else {
        return;
    };
    let Some((size, rest)) = rest.split_at_checked(3) else {
        return;
    };
    let want = (usize::from(size[0]) | usize::from(size[1]) << 8 | usize::from(size[2]) << 16)
        .min(1 << 20);
    match pick % 4 {
        0 => {
            let _ = lzvn_decode(rest, want);
        }
        1 => {
            let _ = lzbitmap_decode(rest, want);
        }
        2 => {
            // A decmpfs attribute, then the resource fork.
            let cut = (want).min(rest.len());
            let (attr, fork) = rest.split_at(cut);
            let _ = decompress(attr, Some(fork));
            let _ = decompress(attr, None);
        }
        _ => {
            let cut = (want).min(rest.len());
            let (attr, fork) = rest.split_at(cut);
            let mut f: &[u8] = fork;
            let _ = decompress_range(attr, fork.len(), &mut f, want / 3, 1 << 16);
        }
    }
});
