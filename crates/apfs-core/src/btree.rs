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
    pub fn parse(block: &[u8]) -> Result<Self, ParseError> {
        verify_block(block)?;
        let obj = ObjPhys::parse(block)?;
        let flags = u16_le(block, 32)?;
        let level = u16_le(block, 34)?;
        let nkeys = u32_le(block, 36)?;
        let toc_off = u16_le(block, 40)?;
        let toc_len = u16_le(block, 42)?;
        Ok(Self {
            obj,
            flags,
            level,
            nkeys,
            toc_off,
            toc_len,
            raw: block.to_vec(),
        })
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
        let toc = DATA_BASE + self.toc_off as usize;
        // kvoff_t { u16 k; u16 v; } per entry.
        let ent = toc + (i as usize) * 4;
        let k_off = u16_le(&self.raw, ent)? as usize;
        let v_off = u16_le(&self.raw, ent + 2)? as usize;
        let key_area = DATA_BASE + self.toc_off as usize + self.toc_len as usize;
        let val_area_end = self
            .raw
            .len()
            .checked_sub(if self.is_root() { BTREE_INFO_SIZE } else { 0 })
            .ok_or(ParseError::Short {
                at: 0,
                need: BTREE_INFO_SIZE,
                len: self.raw.len(),
            })?;
        let kstart = key_area.checked_add(k_off).ok_or(ParseError::Short {
            at: key_area,
            need: k_off,
            len: self.raw.len(),
        })?;
        let key = self
            .raw
            .get(kstart..kstart + ksz)
            .ok_or(ParseError::Short {
                at: kstart,
                need: ksz,
                len: self.raw.len(),
            })?;
        let vstart = val_area_end.checked_sub(v_off).ok_or(ParseError::Short {
            at: val_area_end,
            need: v_off,
            len: self.raw.len(),
        })?;
        let value = self
            .raw
            .get(vstart..vstart + vsz)
            .ok_or(ParseError::Short {
                at: vstart,
                need: vsz,
                len: self.raw.len(),
            })?;
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
        let toc = DATA_BASE + self.toc_off as usize;
        // kvloc_t { nloc_t k{u16 off,u16 len}; nloc_t v{u16 off,u16 len}; } = 8 bytes.
        let ent = toc + (i as usize) * 8;
        let k_off = u16_le(&self.raw, ent)? as usize;
        let k_len = u16_le(&self.raw, ent + 2)? as usize;
        let v_off = u16_le(&self.raw, ent + 4)? as usize;
        let v_len = u16_le(&self.raw, ent + 6)? as usize;
        let key_area = DATA_BASE + self.toc_off as usize + self.toc_len as usize;
        let val_area_end = self
            .raw
            .len()
            .checked_sub(if self.is_root() { BTREE_INFO_SIZE } else { 0 })
            .ok_or(ParseError::Short {
                at: 0,
                need: BTREE_INFO_SIZE,
                len: self.raw.len(),
            })?;
        let kstart = key_area.checked_add(k_off).ok_or(ParseError::Short {
            at: key_area,
            need: k_off,
            len: self.raw.len(),
        })?;
        let key = self
            .raw
            .get(kstart..kstart + k_len)
            .ok_or(ParseError::Short {
                at: kstart,
                need: k_len,
                len: self.raw.len(),
            })?;
        let vstart = val_area_end.checked_sub(v_off).ok_or(ParseError::Short {
            at: val_area_end,
            need: v_off,
            len: self.raw.len(),
        })?;
        let value = self
            .raw
            .get(vstart..vstart + v_len)
            .ok_or(ParseError::Short {
                at: vstart,
                need: v_len,
                len: self.raw.len(),
            })?;
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
}
