//! btree_node_phys_t - APFS B-tree node (FIXED_KV path only for M1c).
//! Layout rules + offsets: ctx_search(source:"the APFS specification").
use crate::checksum::verify_block;
use crate::endian::{u16_le, u32_le, ParseError};
use crate::obj::ObjPhys;

pub const BTNODE_ROOT: u16 = 0x0001;
pub const BTNODE_LEAF: u16 = 0x0002;
pub const BTNODE_FIXED_KV_SIZE: u16 = 0x0004;
const DATA_BASE: usize = 56; // start of btn_data (after fixed node header)
const BTREE_INFO_SIZE: usize = 40; // btree_info_t at end of a ROOT node
/// Table-of-contents entry sizes: `kvoff_t` (two u16 offsets) when keys and
/// values have fixed sizes, `kvloc_t` (two offset/length pairs) otherwise.
const KVOFF_SIZE: usize = 4;
const KVLOC_SIZE: usize = 8;

/// The error for a node whose `btn_level` contradicts its place in the tree.
pub(crate) fn bad_level(level: u16) -> ParseError {
    ParseError::BadField {
        field: "btn_level",
        value: u64::from(level),
    }
}

/// A parsed, checksum-validated B-tree node (FIXED_KV).
#[derive(Debug, Clone)]
pub struct BtreeNode {
    pub obj: ObjPhys,
    pub flags: u16,
    pub level: u16,
    pub nkeys: u32,
    pub toc_off: u16,
    pub toc_len: u16,
    raw: Vec<u8>,
}

impl BtreeNode {
    pub const fn is_leaf(&self) -> bool {
        self.flags & BTNODE_LEAF != 0
    }
    pub const fn is_root(&self) -> bool {
        self.flags & BTNODE_ROOT != 0
    }
    pub const fn is_fixed_kv(&self) -> bool {
        self.flags & BTNODE_FIXED_KV_SIZE != 0
    }

    /// Parse + checksum-validate a B-tree node block.
    ///
    /// The table of contents, the keys after it and the values before the
    /// node's end (or before a root's `btree_info` footer) partition the
    /// node, and every accessor takes its offsets relative to those bounds.
    /// A table that runs past the node, or a record count that needs more
    /// table than there is, is refused here: otherwise the entries past the
    /// table would be read out of the key bytes and handed back as records
    /// no writer stored.
    pub fn parse(block: &[u8]) -> Result<Self, ParseError> {
        verify_block(block)?;
        Self::parse_layout(block)
    }

    /// Parse a node stored without an object header (its object map entry
    /// carries `OMAP_VAL_NOHEADER`, as a sealed volume's catalog nodes do).
    /// The first 32 bytes are not a header there, so there is no checksum
    /// to verify and no type to check; the node layout is checked the same
    /// way as for [`BtreeNode::parse`].
    pub fn parse_headerless(block: &[u8]) -> Result<Self, ParseError> {
        Self::parse_layout(block)
    }

    fn parse_layout(block: &[u8]) -> Result<Self, ParseError> {
        let obj = ObjPhys::parse(block)?;
        let flags = u16_le(block, 32)?;
        let level = u16_le(block, 34)?;
        let nkeys = u32_le(block, 36)?;
        let toc_off = u16_le(block, 40)?;
        let toc_len = u16_le(block, 42)?;
        let node = Self {
            obj,
            flags,
            level,
            nkeys,
            toc_off,
            toc_len,
            raw: block.to_vec(),
        };
        let (key_area, val_area_end) = node.areas()?;
        if key_area > val_area_end {
            return Err(ParseError::BadField {
                field: "btn_table_space",
                value: u64::from(toc_len),
            });
        }
        let entry_size = if node.is_fixed_kv() {
            KVOFF_SIZE
        } else {
            KVLOC_SIZE
        };
        let table_needed = (nkeys as usize).checked_mul(entry_size);
        if table_needed.is_none_or(|needed| needed > toc_len as usize) {
            return Err(ParseError::BadField {
                field: "btn_nkeys",
                value: u64::from(nkeys),
            });
        }
        Ok(node)
    }

    /// Where the key area starts (right after the table of contents) and
    /// where the value area ends (the node's end, or the start of a root's
    /// `btree_info` footer). Values are addressed backwards from that end.
    fn areas(&self) -> Result<(usize, usize), ParseError> {
        let key_area = DATA_BASE + self.toc_off as usize + self.toc_len as usize;
        let footer = if self.is_root() { BTREE_INFO_SIZE } else { 0 };
        let val_area_end = self
            .raw
            .len()
            .checked_sub(footer)
            .ok_or(ParseError::Short {
                at: 0,
                need: footer,
                len: self.raw.len(),
            })?;
        Ok((key_area, val_area_end))
    }

    /// The `len` bytes of a key `off` bytes into the key area. A key lies
    /// between the table of contents and the end of the value area.
    fn key_bytes(
        &self,
        key_area: usize,
        val_area_end: usize,
        off: usize,
        len: usize,
    ) -> Result<&[u8], ParseError> {
        let start = key_area.checked_add(off);
        let end = start.and_then(|s| s.checked_add(len));
        match (start, end) {
            (Some(start), Some(end)) if end <= val_area_end => {
                self.raw.get(start..end).ok_or(ParseError::Short {
                    at: start,
                    need: len,
                    len: self.raw.len(),
                })
            }
            _ => Err(ParseError::Short {
                at: key_area.saturating_add(off),
                need: len,
                len: val_area_end,
            }),
        }
    }

    /// The `len` bytes of a value `off` bytes back from the end of the value
    /// area. A value lies between the start of the key area and that end;
    /// an offset reaching further back points into the table or the header.
    fn value_bytes(
        &self,
        key_area: usize,
        val_area_end: usize,
        off: usize,
        len: usize,
    ) -> Result<&[u8], ParseError> {
        let start = val_area_end.checked_sub(off).filter(|&s| s >= key_area);
        let end = start
            .and_then(|s| s.checked_add(len))
            .filter(|&e| e <= val_area_end);
        match (start, end) {
            (Some(start), Some(end)) => self.raw.get(start..end).ok_or(ParseError::Short {
                at: start,
                need: len,
                len: self.raw.len(),
            }),
            _ => Err(ParseError::Short {
                at: val_area_end.saturating_sub(off),
                need: len,
                len: val_area_end.saturating_sub(key_area),
            }),
        }
    }

    /// For a `FIXED_KV` node, return (`key_bytes`, `value_bytes`) for entry `i`,
    /// using fixed sizes `ksz`/`vsz`. Bounds-checked; no panic.
    pub fn fixed_kv(&self, i: u32, ksz: usize, vsz: usize) -> Result<(&[u8], &[u8]), ParseError> {
        if !self.is_fixed_kv() || i >= self.nkeys {
            return Err(ParseError::Short {
                at: i as usize,
                need: 1,
                len: self.nkeys as usize,
            });
        }
        let (key_area, val_area_end) = self.areas()?;
        // kvoff_t { u16 k; u16 v; } per entry.
        let ent = DATA_BASE + self.toc_off as usize + (i as usize) * KVOFF_SIZE;
        let k_off = u16_le(&self.raw, ent)? as usize;
        let v_off = u16_le(&self.raw, ent + 2)? as usize;
        let key = self.key_bytes(key_area, val_area_end, k_off, ksz)?;
        let value = self.value_bytes(key_area, val_area_end, v_off, vsz)?;
        Ok((key, value))
    }

    /// For a variable-KV node (`BTNODE_FIXED_KV_SIZE` NOT set, e.g. the
    /// file-system/catalog tree), return (`key_bytes`, `value_bytes`) for entry
    /// `i` using the kvloc_t TOC. Bounds-checked; no panic.
    pub fn var_kv(&self, i: u32) -> Result<(&[u8], &[u8]), ParseError> {
        if self.is_fixed_kv() || i >= self.nkeys {
            return Err(ParseError::Short {
                at: i as usize,
                need: 1,
                len: self.nkeys as usize,
            });
        }
        let (key_area, val_area_end) = self.areas()?;
        // kvloc_t { nloc_t k{u16 off,u16 len}; nloc_t v{u16 off,u16 len}; } = 8 bytes.
        let ent = DATA_BASE + self.toc_off as usize + (i as usize) * KVLOC_SIZE;
        let k_off = u16_le(&self.raw, ent)? as usize;
        let k_len = u16_le(&self.raw, ent + 2)? as usize;
        let v_off = u16_le(&self.raw, ent + 4)? as usize;
        let v_len = u16_le(&self.raw, ent + 6)? as usize;
        let key = self.key_bytes(key_area, val_area_end, k_off, k_len)?;
        let value = self.value_bytes(key_area, val_area_end, v_off, v_len)?;
        Ok((key, value))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::checksum::fletcher64;
    use crate::obj::OBJECT_TYPE_BTREE;

    /// Build a minimal FIXED_KV leaf node: 1 entry, key 8 bytes = 0xAA.., value
    /// 8 bytes = 0xBB.. ; toc right at DATA_BASE, key area after a 4-byte toc.
    fn make_leaf() -> [u8; 4096] {
        let mut b = [0u8; 4096];
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes()); // o_type
        b[32..34].copy_from_slice(&(BTNODE_LEAF | BTNODE_FIXED_KV_SIZE).to_le_bytes());
        b[34..36].copy_from_slice(&0u16.to_le_bytes()); // level 0
        b[36..40].copy_from_slice(&1u32.to_le_bytes()); // nkeys 1
        b[40..42].copy_from_slice(&0u16.to_le_bytes()); // toc_off 0 (=DATA_BASE)
        b[42..44].copy_from_slice(&4u16.to_le_bytes()); // toc_len 4 (1 kvoff_t)
                                                        // kvoff_t at DATA_BASE: k=0, v=8 (value 8 bytes back from val_area_end)
        b[56..58].copy_from_slice(&0u16.to_le_bytes()); // k_off
        b[58..60].copy_from_slice(&8u16.to_le_bytes()); // v_off
                                                        // key area = DATA_BASE + toc_off(0) + toc_len(4) = 60 ; key 8 bytes
        b[60..68].copy_from_slice(&[0xAA; 8]);
        // value: leaf, not root -> val_area_end = 4096 ; value at 4096-8 = 4088
        b[4088..4096].copy_from_slice(&[0xBB; 8]);
        let c = fletcher64(&b);
        b[0..8].copy_from_slice(&c.to_le_bytes());
        b
    }

    #[test]
    fn parses_and_reads_fixed_kv_entry() {
        let blk = make_leaf();
        let n = BtreeNode::parse(&blk).expect("parse btree node");
        assert!(n.is_leaf() && n.is_fixed_kv() && !n.is_root());
        assert_eq!(n.nkeys, 1);
        let (k, v) = n.fixed_kv(0, 8, 8).expect("kv 0");
        assert_eq!(k, &[0xAA; 8]);
        assert_eq!(v, &[0xBB; 8]);
        assert!(n.fixed_kv(1, 8, 8).is_err()); // out of range, no panic
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut blk = make_leaf();
        blk[100] ^= 0xFF;
        assert!(BtreeNode::parse(&blk).is_err());
    }

    #[test]
    fn var_kv_reads_kvloc_entry() {
        // Synthetic 256-byte LEAF node, not root, not fixed-kv, 1 entry.
        let mut buf = vec![0u8; 256];
        // obj_phys: o_type @24 = OBJECT_TYPE_BTREE (cosmetic; parse() doesn't enforce).
        buf[24] = 0x02;
        // btn_flags @32 = BTNODE_LEAF (0x0002) - not ROOT, not FIXED_KV.
        buf[32..34].copy_from_slice(&0x0002u16.to_le_bytes());
        // btn_level @34 = 0
        // btn_nkeys @36 = 1
        buf[36..40].copy_from_slice(&1u32.to_le_bytes());
        // btn_table_space @40: off=0, len=8 (one 8-byte kvloc entry)
        buf[40..42].copy_from_slice(&0u16.to_le_bytes());
        buf[42..44].copy_from_slice(&8u16.to_le_bytes());
        // kvloc_t at DATA_BASE(56)+toc_off(0) = 56: k.off=0,k.len=4,v.off=4,v.len=2
        buf[56..58].copy_from_slice(&0u16.to_le_bytes()); // k.off
        buf[58..60].copy_from_slice(&4u16.to_le_bytes()); // k.len
        buf[60..62].copy_from_slice(&4u16.to_le_bytes()); // v.off
        buf[62..64].copy_from_slice(&2u16.to_le_bytes()); // v.len
                                                          // key_area = 56 + 0 + 8 = 64 ; key bytes at 64..68
        buf[64..68].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        // val_area_end = 256 (non-root) ; value at 256 - v.off(4) = 252..254
        buf[252..254].copy_from_slice(&[0x11, 0x22]);
        // Fix up Fletcher-64 so parse()'s verify_block passes.
        let ck = crate::checksum::fletcher64(&buf);
        buf[0..8].copy_from_slice(&ck.to_le_bytes());

        let node = BtreeNode::parse(&buf).unwrap();
        assert!(!node.is_fixed_kv());
        let (k, v) = node.var_kv(0).unwrap();
        assert_eq!(k, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(v, &[0x11, 0x22]);
        assert!(node.var_kv(1).is_err()); // out of range -> error, not panic
    }

    fn reseal(blk: &mut [u8]) {
        let c = fletcher64(blk);
        blk[0..8].copy_from_slice(&c.to_le_bytes());
    }

    /// A record count larger than the table of contents can hold must be
    /// refused when the node is parsed: the entries past the table would be
    /// read out of the key bytes that follow it, and the accessor would hand
    /// back a key and a value no writer ever stored.
    #[test]
    fn rejects_record_count_beyond_the_table_of_contents() {
        let mut blk = make_leaf();
        // nkeys = 2 while the table still holds one 4-byte kvoff_t: entry 1
        // would be the first four key bytes (0xAA..).
        blk[36..40].copy_from_slice(&2u32.to_le_bytes());
        reseal(&mut blk);
        assert!(
            BtreeNode::parse(&blk).is_err(),
            "a record count past the table of contents must not parse"
        );
    }

    /// The table of contents has to lie inside the node.
    #[test]
    fn rejects_table_of_contents_past_the_node() {
        let mut blk = make_leaf();
        blk[42..44].copy_from_slice(&0xFFF0u16.to_le_bytes()); // toc_len past the block
        reseal(&mut blk);
        assert!(
            BtreeNode::parse(&blk).is_err(),
            "a table of contents running past the node must not parse"
        );
    }

    /// A value offset that reaches back over the key area into the header is
    /// not a value; the accessor must refuse it rather than return header
    /// bytes as data.
    #[test]
    fn rejects_value_that_lands_inside_the_header() {
        let mut blk = make_leaf();
        // v_off 4090 puts the 8-byte value at 4096 - 4090 = 6, in obj_phys.
        blk[58..60].copy_from_slice(&4090u16.to_le_bytes());
        reseal(&mut blk);
        let n = BtreeNode::parse(&blk).expect("the table geometry itself is sound");
        assert!(
            n.fixed_kv(0, 8, 8).is_err(),
            "a value inside the header must be refused"
        );
    }

    /// A key that crosses the end of the value area (into the `btree_info`
    /// footer of a root node, here) is refused the same way.
    #[test]
    fn rejects_key_that_runs_into_the_root_footer() {
        let mut blk = make_leaf();
        blk[32..34]
            .copy_from_slice(&(BTNODE_ROOT | BTNODE_LEAF | BTNODE_FIXED_KV_SIZE).to_le_bytes());
        // A root's value area ends at 4096 - 40 = 4056; k_off 3992 puts the
        // 8-byte key at 60 + 3992 = 4052, straddling that boundary.
        blk[56..58].copy_from_slice(&3992u16.to_le_bytes());
        reseal(&mut blk);
        let n = BtreeNode::parse(&blk).expect("the table geometry itself is sound");
        assert!(
            n.fixed_kv(0, 8, 8).is_err(),
            "a key crossing into the footer must be refused"
        );
    }
}
