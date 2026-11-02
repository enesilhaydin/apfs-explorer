//! Copy-on-write edits that rewrite only the nodes on the paths to the
//! records they change.
//!
//! The rebuild paths (`rewrite_fstree_impl`'s fallback, `build_extref_tree`,
//! `build_omap_tree`) write every node of a tree for each change, so one file
//! operation on a volume of a few thousand files read and wrote hundreds of
//! blocks. A batch of edits here descends only into the subtrees its keys fall
//! in. A node that overflows is split, a node that empties is dropped from its
//! parent, and every index key stays equal to the first key of the child it
//! names. Nodes are never merged: the reference writer (linux-apfs-rw
//! btree.c) does not merge them either, and removes a node that empties the
//! same way. A root left with no records becomes an empty leaf.
//!
//! The catalog is a virtual tree: a rewritten node keeps its oid and moves to
//! a new block, so its parent only changes when the node's first key does or
//! the node splits or goes away; the new blocks are mapped through the volume
//! object map by [`remap_catalog_nodes`]. The extent-reference tree and the
//! object map are physical: every rewritten node gets a new address, so the
//! whole path up to the root is rewritten.

use super::{
    build_extref_node, build_fstree_node, cat_key_cmp, fstree_child_level, fstree_too_deep,
    node_capacity, omap_child_level, omap_node_layout, omap_resolve, packed_size, parse_cat_leaf,
    rd_u16, rd_u32, rd_u64, serialize_omap_node, wr_u32, wr_u64, CatRec, FstreeFooter, OmapRec,
    BTREE_INFO_SIZE, DATA_BASE, MAX_OMAP_TREE_DEPTH, MAX_TREE_DEPTH, OMAP_INDEX_VAL_SIZE,
    OMAP_KEY_SIZE, OMAP_LEAF_VAL_SIZE, OMAP_TOC_ENTRY, OMAP_VAL_DELETED,
};
use crate::txn::{update_checksum_in_place, Transaction, TxnError};
use apfs_core::block_device::WritableBlockDevice;
use std::cmp::Ordering;
use std::collections::HashMap;

/// Bytes of an index record's value: the child's oid or address.
const CHILD_REF_LEN: usize = 8;

/// Bytes at the start of a root's tree description that no edit changes
/// (`bt_flags`, `bt_node_size`, `bt_key_size`, `bt_val_size`).
const FOOTER_FIXED_LEN: usize = 16;

/// A change made to a record's value in place.
pub(super) type Patch = Box<dyn Fn(&mut Vec<u8>) -> Result<(), TxnError>>;

/// What an edit does to the record under its key.
pub(super) enum EditOp {
    /// Add the record; a record already under the key is an error.
    Insert(Vec<u8>),
    /// Replace the value of the record under the key, if there is one.
    Update(Vec<u8>),
    /// Replace the value, or add the record if there is none.
    Upsert(Vec<u8>),
    /// Remove the record under the key, if there is one.
    Remove,
    /// Change the value of the record under the key, if there is one.
    Patch(Patch),
}

/// Edits for one tree, at most one per key. A second edit of a key folds
/// into the first, in the order the edits were given.
#[derive(Default)]
pub(super) struct EditBatch {
    edits: Vec<(Vec<u8>, EditOp)>,
    at: HashMap<Vec<u8>, usize>,
}

impl EditBatch {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Add an edit of `key`, folded into an earlier edit of the same key.
    pub(super) fn push(&mut self, key: Vec<u8>, op: EditOp) -> Result<(), TxnError> {
        let Some(&i) = self.at.get(&key) else {
            self.at.insert(key.clone(), self.edits.len());
            self.edits.push((key, op));
            return Ok(());
        };
        let slot = self
            .edits
            .get_mut(i)
            .ok_or_else(|| TxnError::InvalidArgument("edit batch index out of range".into()))?;
        let prev = std::mem::replace(&mut slot.1, EditOp::Remove);
        slot.1 = fold(prev, op, &key)?;
        Ok(())
    }

    fn into_sorted(self, cmp: impl Fn(&[u8], &[u8]) -> Ordering) -> Vec<(Vec<u8>, EditOp)> {
        let mut edits = self.edits;
        edits.sort_by(|a, b| cmp(&a.0, &b.0));
        edits
    }
}

/// `prev` followed by `next` on the same key, as one edit.
fn fold(prev: EditOp, next: EditOp, key: &[u8]) -> Result<EditOp, TxnError> {
    use EditOp::*;
    Ok(match (prev, next) {
        (Insert(_) | Upsert(_), Insert(_)) => {
            return Err(TxnError::AlreadyExists(format!(
                "tree key {key:02x?} inserted twice"
            )))
        }
        (_, Remove) => Remove,
        (Remove, Insert(v) | Upsert(v)) => Upsert(v),
        (Remove, Update(_) | Patch(_)) => Remove,
        (Update(_) | Patch(_), Insert(v)) => Insert(v),
        (Update(_) | Patch(_) | Upsert(_), Upsert(v)) => Upsert(v),
        (Insert(_), Upsert(v)) => Insert(v),
        (Insert(_), Update(v)) => Insert(v),
        (Upsert(_), Update(v)) => Upsert(v),
        (Update(_) | Patch(_), Update(v)) => Update(v),
        (Insert(mut v), Patch(f)) => {
            f(&mut v)?;
            Insert(v)
        }
        (Upsert(mut v), Patch(f)) => {
            f(&mut v)?;
            Upsert(v)
        }
        (Update(mut v), Patch(f)) => {
            f(&mut v)?;
            Update(v)
        }
        (Patch(g), Patch(f)) => Patch(Box::new(move |v| {
            g(v)?;
            f(v)
        })),
    })
}

/// Which tree a [`CowTree`] edits.
pub(super) enum Shape<'a> {
    /// The volume's file-system tree: virtual nodes, found through the
    /// volume object map whose root node is given.
    Catalog { omap_root: &'a [u8] },
    /// The extent-reference tree: physical nodes.
    ExtentRef,
    /// A volume object map's tree: physical nodes of fixed-size records.
    ObjectMap,
}

/// One path-local edit of a tree, and what it did.
pub(super) struct CowTree<'a> {
    shape: Shape<'a>,
    xid: u64,
    bsz: usize,
    /// The root as it was, for its tree description.
    old_root: Vec<u8>,
    /// Physical trees: the blocks of the node versions replaced or dropped.
    pub(super) superseded: Vec<u64>,
    /// Catalog: every node rewritten (oid, new block) or dropped (oid, None).
    pub(super) remapped: Vec<(u64, Option<u64>)>,
    /// Nodes the tree gained (negative: lost).
    pub(super) nodes_added: i64,
    /// Records the tree gained (negative: lost).
    pub(super) keys_added: i64,
    longest_key: u32,
    longest_val: u32,
    /// Refuses a rewritten leaf's records the tree cannot hold.
    leaf_check: Option<LeafCheck>,
    /// Node (oid or address) -> the block it was read from, for nodes this
    /// edit may rewrite.
    located: HashMap<u64, u64>,
}

/// How a parent names a child: the child's first key and its oid or address.
type ChildRef = (Vec<u8>, u64);

/// A check of a rewritten leaf's records.
pub(super) type LeafCheck = fn(&[CatRec]) -> Result<(), TxnError>;

impl<'a> CowTree<'a> {
    pub(super) fn new(shape: Shape<'a>, xid: u64, bsz: usize) -> Self {
        Self {
            shape,
            xid,
            bsz,
            old_root: Vec::new(),
            superseded: Vec::new(),
            remapped: Vec::new(),
            nodes_added: 0,
            keys_added: 0,
            longest_key: 0,
            longest_val: 0,
            leaf_check: None,
            located: HashMap::new(),
        }
    }

    /// Run `check` on the records of every leaf this edit rewrites.
    pub(super) fn with_leaf_check(mut self, check: LeafCheck) -> Self {
        self.leaf_check = Some(check);
        self
    }

    /// Apply `batch` to the tree whose root is `root` (an oid for the
    /// catalog, an address otherwise) and return the root as it is now.
    pub(super) fn apply<D: WritableBlockDevice>(
        &mut self,
        txn: &mut Transaction<D>,
        root: u64,
        batch: EditBatch,
    ) -> Result<u64, TxnError> {
        let edits = batch.into_sorted(|a, b| self.cmp(a, b));
        if edits.is_empty() {
            return Ok(root);
        }
        let (raw, mut level) = self.load_for_edit(txn, root, None)?;
        self.old_root = raw.clone();
        let edited = self.edited_records(txn, &raw, level, &edits, 0)?;
        let footer = self.root_footer()?;
        let footer_moved = footer.key_count != self.old_footer_u64(24)
            || footer.node_count != self.old_footer_u64(32)
            || footer.longest_key != self.old_footer_u32(16)
            || footer.longest_val != self.old_footer_u32(20);
        // A root whose records stayed is rewritten only for its description;
        // the catalog's rewritten nodes kept their oids, so it still names them.
        let (mut recs, appending) = match edited {
            Some(e) => e,
            None if !footer_moved => return Ok(root),
            None => (self.records(&raw, level)?, false),
        };
        if recs.is_empty() {
            level = 0;
        }
        // A root that overflows hands its records to new nodes one level down
        // and takes their first keys, until it fits.
        while !self.fits(&recs, level, true) {
            if u32::from(level) >= MAX_TREE_DEPTH {
                return Err(fstree_too_deep());
            }
            let mut up = Vec::new();
            for chunk in self.split(recs, level, appending)? {
                let first = first_key(&chunk)?;
                let r = self.store(txn, None, level, false, &chunk, None)?;
                up.push((first, r.to_le_bytes().to_vec()));
            }
            recs = up;
            level += 1;
        }
        let footer = self.root_footer()?;
        self.store(txn, Some(root), level, true, &recs, Some(footer))
    }

    fn old_footer_u64(&self, at: usize) -> u64 {
        rd_u64(&self.old_root, self.bsz - BTREE_INFO_SIZE + at)
    }

    fn old_footer_u32(&self, at: usize) -> u32 {
        rd_u32(&self.old_root, self.bsz - BTREE_INFO_SIZE + at)
    }

    /// The root's tree description after this edit: the records and nodes
    /// the tree gained, and the longest key and value it has ever held.
    fn root_footer(&self) -> Result<FstreeFooter, TxnError> {
        let moved = |old: u64, by: i64, what: &str| {
            old.checked_add_signed(by).ok_or_else(|| {
                TxnError::VolumeParse(format!(
                    "tree description counts {old} {what}, and this edit changes that by {by}"
                ))
            })
        };
        Ok(FstreeFooter {
            longest_key: self.old_footer_u32(16).max(self.longest_key),
            longest_val: self.old_footer_u32(20).max(self.longest_val),
            key_count: moved(self.old_footer_u64(24), self.keys_added, "records")?,
            node_count: moved(self.old_footer_u64(32), self.nodes_added, "nodes")?,
        })
    }

    fn cmp(&self, a: &[u8], b: &[u8]) -> Ordering {
        match self.shape {
            Shape::ObjectMap => omap_key(a).cmp(&omap_key(b)),
            _ => cat_key_cmp(a, b),
        }
    }

    /// [`Self::load`] for a node this edit may rewrite: remembers the block
    /// it sits in, so a node this transaction already wrote is rewritten in
    /// place.
    fn load_for_edit<D: WritableBlockDevice>(
        &mut self,
        txn: &mut Transaction<D>,
        r: u64,
        parent_level: Option<u16>,
    ) -> Result<(Vec<u8>, u16), TxnError> {
        let (raw, level, paddr) = self.load(txn, r, parent_level)?;
        self.located.insert(r, paddr);
        Ok((raw, level))
    }

    /// Read node `r` (an oid for the catalog, an address otherwise), checked
    /// to sit one level below `parent_level` when it has a parent; with the
    /// block it sits in.
    fn load<D: WritableBlockDevice>(
        &self,
        txn: &mut Transaction<D>,
        r: u64,
        parent_level: Option<u16>,
    ) -> Result<(Vec<u8>, u16, u64), TxnError> {
        let paddr = match self.shape {
            Shape::Catalog { omap_root } => omap_resolve(txn, omap_root, r, u64::MAX, self.bsz)?
                .ok_or_else(|| {
                    TxnError::VolumeParse(format!(
                        "tree node {r:#x} is not in the volume object map"
                    ))
                })?,
            _ => r,
        };
        let mut raw = vec![0u8; self.bsz];
        txn.read_block(paddr, &mut raw)?;
        let level = match (&self.shape, parent_level) {
            (Shape::ObjectMap, Some(parent)) => omap_child_level(&raw, paddr, parent)?,
            (Shape::ObjectMap, None) => rd_u16(&raw, 34),
            _ => fstree_child_level(&raw, r, parent_level)?,
        };
        Ok((raw, level, paddr))
    }

    /// The records of a node at `level`.
    fn records(&self, raw: &[u8], level: u16) -> Result<Vec<CatRec>, TxnError> {
        if !matches!(self.shape, Shape::ObjectMap) {
            return parse_cat_leaf(raw)
                .map(|recs| recs.into_iter().map(|r| (r.key, r.val)).collect())
                .map_err(|e| TxnError::VolumeParse(e.into()));
        }
        let layout = omap_node_layout(raw, self.bsz)?;
        let vsz = omap_val_len(level);
        (0..layout.nkeys)
            .map(|i| {
                let k = layout.key_at(raw, i)?;
                let v = layout.val_at(raw, i, vsz)?;
                match (raw.get(k..k + OMAP_KEY_SIZE), raw.get(v..v + vsz)) {
                    (Some(key), Some(val)) => Ok((key.to_vec(), val.to_vec())),
                    _ => Err(TxnError::OmapParse(format!(
                        "omap record {i} lies outside its node"
                    ))),
                }
            })
            .collect()
    }

    /// The records of the node in `raw` after `edits`, and whether every
    /// change landed after its last record; `None` when nothing changed.
    fn edited_records<D: WritableBlockDevice>(
        &mut self,
        txn: &mut Transaction<D>,
        raw: &[u8],
        level: u16,
        edits: &[(Vec<u8>, EditOp)],
        depth: u32,
    ) -> Result<Option<(Vec<CatRec>, bool)>, TxnError> {
        let recs = self.records(raw, level)?;
        if level == 0 {
            return self.apply_leaf(recs, edits);
        }
        let n = recs.len();
        let pivots: Vec<Vec<u8>> = recs.iter().map(|(k, _)| k.clone()).collect();
        let mut out: Vec<CatRec> = Vec::with_capacity(n + 1);
        let mut first_changed: Option<usize> = None;
        let mut start = 0usize;
        for (i, (pivot, val)) in recs.into_iter().enumerate() {
            // Child i holds the keys from its pivot up to the next one; keys
            // below the first pivot belong to the first child.
            let rest = edits.get(start..).unwrap_or(&[]);
            let take = match pivots.get(i + 1) {
                Some(next) => rest.partition_point(|(k, _)| self.cmp(k, next) == Ordering::Less),
                None => rest.len(),
            };
            let sub = rest.get(..take).unwrap_or(&[]);
            start += take;
            if sub.is_empty() {
                out.push((pivot, val));
                continue;
            }
            let child = child_ref(&val)?;
            let Some(nodes) = self.edit_child(txn, child, level, sub, depth + 1)? else {
                out.push((pivot, val));
                continue;
            };
            let same = matches!(nodes.as_slice(), [(k, r)] if *k == pivot && *r == child);
            if !same {
                first_changed.get_or_insert(i);
            }
            out.extend(
                nodes
                    .into_iter()
                    .map(|(k, r)| (k, r.to_le_bytes().to_vec())),
            );
        }
        Ok(first_changed.map(|i| (out, i + 1 == n)))
    }

    /// Apply sorted `edits` to a leaf's records.
    fn apply_leaf(
        &mut self,
        recs: Vec<CatRec>,
        edits: &[(Vec<u8>, EditOp)],
    ) -> Result<Option<(Vec<CatRec>, bool)>, TxnError> {
        let last = recs.last().map(|(k, _)| k.clone());
        let mut out: Vec<CatRec> = Vec::with_capacity(recs.len() + edits.len());
        let mut changed = false;
        let mut appending = true;
        let mut it = recs.into_iter().peekable();
        for (key, op) in edits {
            while let Some((k, _)) = it.peek() {
                if self.cmp(k, key) != Ordering::Less {
                    break;
                }
                if let Some(r) = it.next() {
                    out.push(r);
                }
            }
            let present = match it.peek() {
                Some((k, _)) if self.cmp(k, key) == Ordering::Equal => {
                    if k != key {
                        return Err(TxnError::VolumeParse(format!(
                            "tree keys {k:02x?} and {key:02x?} differ but sort as one"
                        )));
                    }
                    it.next()
                }
                _ => None,
            };
            let after_last = last
                .as_ref()
                .is_none_or(|l| self.cmp(key, l) == Ordering::Greater);
            match (op, present) {
                (EditOp::Insert(_), Some(_)) => {
                    return Err(TxnError::AlreadyExists(format!(
                        "tree key {key:02x?} already present"
                    )))
                }
                (EditOp::Insert(v) | EditOp::Upsert(v), None) => {
                    self.note_longest(key, v);
                    out.push((key.clone(), v.clone()));
                    self.keys_added += 1;
                    changed = true;
                    appending &= after_last;
                }
                (EditOp::Update(v) | EditOp::Upsert(v), Some((k, old))) => {
                    if old != *v {
                        self.note_longest(&k, v);
                        changed = true;
                        appending = false;
                    }
                    out.push((k, v.clone()));
                }
                (EditOp::Patch(f), Some((k, mut v))) => {
                    let before = v.clone();
                    f(&mut v)?;
                    if v != before {
                        self.note_longest(&k, &v);
                        changed = true;
                        appending = false;
                    }
                    out.push((k, v));
                }
                (EditOp::Remove, Some(_)) => {
                    self.keys_added -= 1;
                    changed = true;
                    appending = false;
                }
                (EditOp::Update(_) | EditOp::Remove | EditOp::Patch(_), None) => {}
            }
        }
        out.extend(it);
        if !changed {
            return Ok(None);
        }
        if let Some(check) = self.leaf_check {
            check(&out)?;
        }
        Ok(Some((out, appending)))
    }

    fn note_longest(&mut self, key: &[u8], val: &[u8]) {
        self.longest_key = self.longest_key.max(key.len() as u32);
        self.longest_val = self.longest_val.max(val.len() as u32);
    }

    /// Apply `edits` to the subtree under child `r` of a node at
    /// `parent_level`: `None` when it did not change, else the nodes that
    /// now stand in its place (none when it emptied).
    fn edit_child<D: WritableBlockDevice>(
        &mut self,
        txn: &mut Transaction<D>,
        r: u64,
        parent_level: u16,
        edits: &[(Vec<u8>, EditOp)],
        depth: u32,
    ) -> Result<Option<Vec<ChildRef>>, TxnError> {
        if depth > MAX_TREE_DEPTH {
            return Err(fstree_too_deep());
        }
        let (raw, level) = self.load_for_edit(txn, r, Some(parent_level))?;
        let Some((recs, appending)) = self.edited_records(txn, &raw, level, edits, depth)? else {
            return Ok(None);
        };
        if recs.is_empty() {
            self.drop_node(r);
            return Ok(Some(Vec::new()));
        }
        let chunks = if self.fits(&recs, level, false) {
            vec![recs]
        } else {
            self.split(recs, level, appending)?
        };
        let mut out = Vec::with_capacity(chunks.len());
        for (j, chunk) in chunks.into_iter().enumerate() {
            let first = first_key(&chunk)?;
            let old = if j == 0 { Some(r) } else { None };
            out.push((first, self.store(txn, old, level, false, &chunk, None)?));
        }
        Ok(Some(out))
    }

    /// Records an object-map node at `level` holds at most. The budget is the
    /// root's (its description takes room), so any node fits it, as in
    /// `build_omap_tree`.
    fn omap_cap(&self, level: u16) -> usize {
        let avail = self
            .bsz
            .saturating_sub(BTREE_INFO_SIZE)
            .saturating_sub(DATA_BASE);
        (avail / (OMAP_TOC_ENTRY + OMAP_KEY_SIZE + omap_val_len(level))).max(1)
    }

    fn fits(&self, recs: &[CatRec], level: u16, is_root: bool) -> bool {
        match self.shape {
            Shape::ObjectMap => recs.len() <= self.omap_cap(level),
            _ => packed_size(recs) <= node_capacity(self.bsz, is_root),
        }
    }

    /// Split an overflowing node's records into nodes that each fit. A node
    /// that only grew at its end keeps the earlier nodes full, the way a
    /// sequential insert fills a tree; any other node is split evenly so
    /// both halves have room for what lands in them next.
    fn split(
        &self,
        recs: Vec<CatRec>,
        level: u16,
        appending: bool,
    ) -> Result<Vec<Vec<CatRec>>, TxnError> {
        let (cap, weight): (usize, fn(&CatRec) -> usize) = match self.shape {
            Shape::ObjectMap => (self.omap_cap(level), |_| 1),
            _ => (node_capacity(self.bsz, false), |(k, v)| {
                8 + k.len() + v.len()
            }),
        };
        // An even split closes a node once it holds its share of the total;
        // the capacity bounds every node either way.
        let total: usize = recs.iter().map(weight).sum();
        let share = if appending {
            usize::MAX
        } else {
            total.div_ceil(total.div_ceil(cap).max(1))
        };
        let mut out: Vec<Vec<CatRec>> = Vec::new();
        let mut cur: Vec<CatRec> = Vec::new();
        let mut size = 0usize;
        for r in recs {
            let w = weight(&r);
            if w > cap {
                return Err(TxnError::InvalidArgument(format!(
                    "a record of {w} bytes does not fit a node of {cap}"
                )));
            }
            if size + w > cap && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                size = 0;
            }
            size += w;
            cur.push(r);
            if size >= share {
                out.push(std::mem::take(&mut cur));
                size = 0;
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        Ok(out)
    }

    /// Write a node and return how its parent names it. `old` is the node it
    /// replaces (none for a new node); a catalog node keeps the oid it
    /// replaces, a physical node gets a new address. A node this transaction
    /// already wrote is rewritten in the block it has: no checkpoint names
    /// that block yet, so the copy would only be released again at commit,
    /// and the parent (or, for the catalog, the object map) keeps naming it.
    fn store<D: WritableBlockDevice>(
        &mut self,
        txn: &mut Transaction<D>,
        old: Option<u64>,
        level: u16,
        is_root: bool,
        recs: &[CatRec],
        footer: Option<FstreeFooter>,
    ) -> Result<u64, TxnError> {
        let in_place = old
            .and_then(|r| self.located.get(&r).copied())
            .filter(|&p| txn.staged_live(p));
        let p = match in_place {
            Some(p) => p,
            None => txn.alloc_block()?,
        };
        if old.is_none() {
            self.nodes_added += 1;
        }
        let replaced = if in_place.is_some() { None } else { old };
        let keep_fixed = footer.is_some();
        let (mut node, r) = match self.shape {
            Shape::Catalog { .. } => {
                let oid = match old {
                    Some(o) => o,
                    None => txn.alloc_oid(),
                };
                if in_place.is_none() {
                    self.remapped.push((oid, Some(p)));
                }
                (
                    build_fstree_node(oid, self.xid, is_root, level, recs, footer, self.bsz),
                    oid,
                )
            }
            Shape::ExtentRef => {
                self.superseded.extend(replaced);
                (
                    build_extref_node(p, self.xid, is_root, level, recs, footer, self.bsz),
                    p,
                )
            }
            Shape::ObjectMap => {
                self.superseded.extend(replaced);
                let entries = recs
                    .iter()
                    .map(|(k, v)| {
                        let key: [u8; OMAP_KEY_SIZE] = k.as_slice().try_into().map_err(|_| {
                            TxnError::OmapParse(format!(
                                "omap key of {} bytes (want {OMAP_KEY_SIZE})",
                                k.len()
                            ))
                        })?;
                        Ok((key, v.clone()))
                    })
                    .collect::<Result<Vec<_>, TxnError>>()?;
                let (keys, nodes) = footer.map_or((0, 0), |f| (f.key_count, f.node_count));
                let node = serialize_omap_node(
                    &self.old_root,
                    level,
                    is_root,
                    &entries,
                    p,
                    self.xid,
                    keys,
                    nodes,
                    self.bsz,
                );
                (node, p)
            }
        };
        // A rewritten root keeps the description of its tree: the flags and
        // fixed sizes the tree was made with are not this edit's to change.
        if keep_fixed && !matches!(self.shape, Shape::ObjectMap) {
            let bti = self.bsz - BTREE_INFO_SIZE;
            if let (Some(dst), Some(src)) = (
                node.get_mut(bti..bti + FOOTER_FIXED_LEN),
                self.old_root.get(bti..bti + FOOTER_FIXED_LEN),
            ) {
                dst.copy_from_slice(src);
            }
            update_checksum_in_place(&mut node);
        }
        if in_place.is_some() {
            txn.restage(p, node);
        } else {
            txn.stage_raw(p, node);
        }
        Ok(r)
    }

    /// Take node `r` out of the tree.
    fn drop_node(&mut self, r: u64) {
        self.nodes_added -= 1;
        match self.shape {
            Shape::Catalog { .. } => self.remapped.push((r, None)),
            _ => self.superseded.push(r),
        }
    }
}

/// Bytes of an object-map record's value at `level`.
fn omap_val_len(level: u16) -> usize {
    if level == 0 {
        OMAP_LEAF_VAL_SIZE
    } else {
        OMAP_INDEX_VAL_SIZE
    }
}

/// An object-map key as `(oid, xid)`.
fn omap_key(k: &[u8]) -> (u64, u64) {
    (rd_u64(k, 0), rd_u64(k, 8))
}

fn first_key(recs: &[CatRec]) -> Result<Vec<u8>, TxnError> {
    recs.first()
        .map(|(k, _)| k.clone())
        .ok_or_else(|| TxnError::InvalidArgument("a tree node with no records".into()))
}

/// The child an index record names.
fn child_ref(val: &[u8]) -> Result<u64, TxnError> {
    if val.len() != CHILD_REF_LEN {
        return Err(TxnError::VolumeParse(format!(
            "index record value of {} bytes (want {CHILD_REF_LEN})",
            val.len()
        )));
    }
    Ok(rd_u64(val, 0))
}

/// The object-map key of `(oid, xid)`.
fn omap_key_bytes(oid: u64, xid: u64) -> Vec<u8> {
    let mut k = vec![0u8; OMAP_KEY_SIZE];
    wr_u64(&mut k, 0, oid);
    wr_u64(&mut k, 8, xid);
    k
}

/// The object-map value of a one-block object at `paddr`.
fn omap_live_val(paddr: u64, bsz: usize) -> Vec<u8> {
    let mut v = vec![0u8; OMAP_LEAF_VAL_SIZE];
    wr_u32(&mut v, 4, bsz as u32);
    wr_u64(&mut v, 8, paddr);
    v
}

/// Every mapping of `oid` in the object map under `node`.
pub(super) fn omap_versions<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    node: &[u8],
    oid: u64,
    bsz: usize,
) -> Result<Vec<OmapRec>, TxnError> {
    let mut out = Vec::new();
    omap_versions_below(txn, node, oid, bsz, 0, &mut out)?;
    Ok(out)
}

fn omap_versions_below<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    node: &[u8],
    oid: u64,
    bsz: usize,
    depth: u32,
    out: &mut Vec<OmapRec>,
) -> Result<(), TxnError> {
    if depth > MAX_OMAP_TREE_DEPTH {
        return Err(TxnError::OmapParse(format!(
            "omap tree is deeper than {MAX_OMAP_TREE_DEPTH} levels"
        )));
    }
    let level = rd_u16(node, 34);
    let layout = omap_node_layout(node, bsz)?;
    let keys = (0..layout.nkeys)
        .map(|i| Ok(omap_key(node.get(layout.key_at(node, i)?..).unwrap_or(&[]))))
        .collect::<Result<Vec<_>, TxnError>>()?;
    if level == 0 {
        for (i, &(k_oid, k_xid)) in keys.iter().enumerate() {
            if k_oid != oid {
                continue;
            }
            let v = layout.val_at(node, i, OMAP_LEAF_VAL_SIZE)?;
            out.push(OmapRec {
                oid,
                xid: k_xid,
                flags: rd_u32(node, v),
                size: rd_u32(node, v + 4),
                paddr: rd_u64(node, v + 8),
            });
        }
        return Ok(());
    }
    // Child i holds the keys from its key up to the next child's: it can
    // hold `oid` when its key is at most (oid, max) and the next is above
    // (oid, 0).
    for (i, &k) in keys.iter().enumerate() {
        if k > (oid, u64::MAX) {
            break;
        }
        if keys.get(i + 1).is_some_and(|&next| next <= (oid, 0)) {
            continue;
        }
        let v = layout.val_at(node, i, OMAP_INDEX_VAL_SIZE)?;
        let child = rd_u64(node, v);
        let mut buf = vec![0u8; bsz];
        txn.read_block(child, &mut buf)?;
        omap_child_level(&buf, child, level)?;
        omap_versions_below(txn, &buf, oid, bsz, depth + 1, out)?;
    }
    Ok(())
}

/// Every catalog record whose key lies in `[lo, hi)`, reading only the
/// nodes whose key ranges meet it.
pub(super) fn catalog_range<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_root: &[u8],
    root_oid: u64,
    lo: &[u8],
    hi: &[u8],
    bsz: usize,
) -> Result<Vec<CatRec>, TxnError> {
    let tree = CowTree::new(Shape::Catalog { omap_root }, 0, bsz);
    let mut out = Vec::new();
    tree.range_below(txn, root_oid, None, lo, hi, 0, &mut out)?;
    Ok(out)
}

/// Every extent-reference record that covers a block of `[start, end)`:
/// the last record starting at or before `start`, and every record starting
/// inside the range. Records never overlap, so no other one can.
pub(super) fn extref_covering<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    root: u64,
    start: u64,
    end: u64,
    bsz: usize,
) -> Result<Vec<CatRec>, TxnError> {
    let tree = CowTree::new(Shape::ExtentRef, 0, bsz);
    let key = |b: u64| super::encode_jkey(b, super::APFS_TYPE_EXTENT).to_le_bytes();
    let mut out = Vec::new();
    if let Some(before) = tree.floor_below(txn, root, None, &key(start), 0)? {
        out.push(before);
    }
    let lo = key(start.saturating_add(1));
    let mut inside = Vec::new();
    tree.range_below(txn, root, None, &lo, &key(end), 0, &mut inside)?;
    out.extend(inside);
    Ok(out)
}

impl CowTree<'_> {
    /// The record with the greatest key at or below `key`, if any.
    fn floor_below<D: WritableBlockDevice>(
        &self,
        txn: &mut Transaction<D>,
        r: u64,
        parent_level: Option<u16>,
        key: &[u8],
        depth: u32,
    ) -> Result<Option<CatRec>, TxnError> {
        if depth > MAX_TREE_DEPTH {
            return Err(fstree_too_deep());
        }
        let (raw, level, _) = self.load(txn, r, parent_level)?;
        let recs = self.records(&raw, level)?;
        let at = recs.partition_point(|(k, _)| self.cmp(k, key) != Ordering::Greater);
        let Some(i) = at.checked_sub(1) else {
            // Every key here is above `key`, and this node holds the
            // lowest keys of its parent's range: there is nothing below.
            return Ok(None);
        };
        let Some((_, val)) = recs.get(i) else {
            return Ok(None);
        };
        if level == 0 {
            return Ok(recs.into_iter().nth(i));
        }
        self.floor_below(txn, child_ref(val)?, Some(level), key, depth + 1)
    }

    #[allow(clippy::too_many_arguments)]
    fn range_below<D: WritableBlockDevice>(
        &self,
        txn: &mut Transaction<D>,
        r: u64,
        parent_level: Option<u16>,
        lo: &[u8],
        hi: &[u8],
        depth: u32,
        out: &mut Vec<CatRec>,
    ) -> Result<(), TxnError> {
        if depth > MAX_TREE_DEPTH {
            return Err(fstree_too_deep());
        }
        let (raw, level, _) = self.load(txn, r, parent_level)?;
        let recs = self.records(&raw, level)?;
        let below_hi = |k: &[u8]| self.cmp(k, hi) == Ordering::Less;
        if level == 0 {
            out.extend(
                recs.into_iter()
                    .filter(|(k, _)| self.cmp(k, lo) != Ordering::Less && below_hi(k)),
            );
            return Ok(());
        }
        // Child i holds the keys from its pivot up to the next pivot.
        for (i, (pivot, val)) in recs.iter().enumerate() {
            if i > 0 && !below_hi(pivot) {
                break;
            }
            if let Some((next, _)) = recs.get(i + 1) {
                if self.cmp(next, lo) != Ordering::Greater {
                    continue;
                }
            }
            self.range_below(txn, child_ref(val)?, Some(level), lo, hi, depth + 1, out)?;
        }
        Ok(())
    }
}

/// The volume object map after a catalog edit.
pub(super) struct Remapped {
    /// The object map's new root.
    pub(super) root: u64,
    /// Object-map nodes the edit replaced.
    pub(super) omap_superseded: Vec<u64>,
    /// Object-map nodes gained (negative: lost).
    pub(super) omap_nodes_added: i64,
    /// Blocks of the catalog node versions no mapping names any more.
    pub(super) catalog_superseded: Vec<u64>,
}

/// Point the volume object map whose root node is `omap_root` (at
/// `omap_root_paddr`) at the catalog nodes `remapped` rewrote or dropped.
///
/// Each such node loses every mapping newer than `newest_snap` (the newest
/// snapshot's transaction; 0 without one), and those versions' blocks are
/// returned to be released: a mapping no tree reaches is one fsck reports as
/// never used. A rewritten node gains its mapping at `xid`.
pub(super) fn remap_catalog_nodes<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_root: &[u8],
    omap_root_paddr: u64,
    remapped: &[(u64, Option<u64>)],
    xid: u64,
    newest_snap: u64,
    bsz: usize,
) -> Result<Remapped, TxnError> {
    let mut batch = EditBatch::new();
    let mut catalog_superseded = Vec::new();
    for &(oid, new) in remapped {
        for v in omap_versions(txn, omap_root, oid, bsz)? {
            if v.xid <= newest_snap {
                continue;
            }
            if v.flags & OMAP_VAL_DELETED == 0 {
                catalog_superseded.push(v.paddr);
            }
            batch.push(omap_key_bytes(oid, v.xid), EditOp::Remove)?;
        }
        if let Some(p) = new {
            batch.push(
                omap_key_bytes(oid, xid),
                EditOp::Upsert(omap_live_val(p, bsz)),
            )?;
        }
    }
    let mut tree = CowTree::new(Shape::ObjectMap, xid, bsz);
    let root = tree.apply(txn, omap_root_paddr, batch)?;
    Ok(Remapped {
        root,
        omap_superseded: tree.superseded,
        omap_nodes_added: tree.nodes_added,
        catalog_superseded,
    })
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn folded(ops: Vec<EditOp>) -> Result<EditOp, TxnError> {
        let mut batch = EditBatch::new();
        for op in ops {
            batch.push(vec![1], op)?;
        }
        let mut edits = batch.into_sorted(|a, b| a.cmp(b));
        assert_eq!(edits.len(), 1);
        Ok(edits.remove(0).1)
    }

    // The rebuild applies updates, then removals, then inserts; folding the
    // same edits of one key has to land where that order lands.
    #[test]
    fn edits_of_one_key_fold_in_the_rebuild_order() {
        use EditOp::*;
        assert!(matches!(
            folded(vec![Remove, Insert(vec![2])]).unwrap(),
            Upsert(v) if v == [2]
        ));
        assert!(matches!(
            folded(vec![Update(vec![1]), Remove]).unwrap(),
            Remove
        ));
        assert!(matches!(
            folded(vec![Update(vec![1]), Insert(vec![3])]).unwrap(),
            Insert(v) if v == [3]
        ));
        assert!(matches!(
            folded(vec![Patch(Box::new(|v| {
                v.push(9);
                Ok(())
            })), Update(vec![4])])
            .unwrap(),
            Update(v) if v == [4]
        ));
        assert!(matches!(
            folded(vec![
                Update(vec![5]),
                Patch(Box::new(|v| {
                    v.push(6);
                    Ok(())
                }))
            ])
            .unwrap(),
            Update(v) if v == [5, 6]
        ));
        assert!(matches!(
            folded(vec![Insert(vec![1]), Insert(vec![2])]),
            Err(TxnError::AlreadyExists(_))
        ));
    }

    fn recs(sizes: &[usize]) -> Vec<CatRec> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &n)| ((i as u64).to_le_bytes().to_vec(), vec![0u8; n]))
            .collect()
    }

    // A node that grew in the middle splits into halves of like size; one
    // that only grew at its end keeps the first node full.
    #[test]
    fn a_split_is_even_unless_the_node_only_grew_at_its_end() {
        let tree = CowTree::new(Shape::ExtentRef, 1, 4096);
        let cap = node_capacity(4096, false);
        let n = cap / 100 + 1;
        let input = recs(&vec![84; n]);
        let even = tree.split(input.clone(), 0, false).unwrap();
        assert_eq!(even.len(), 2);
        let (a, b) = (packed_size(&even[0]), packed_size(&even[1]));
        assert!(a.abs_diff(b) <= 100, "uneven halves: {a} and {b}");
        let full = tree.split(input, 0, true).unwrap();
        assert_eq!(full.len(), 2);
        assert_eq!(full[1].len(), 1, "the appended record stands alone");
        for chunk in even.iter().chain(&full) {
            assert!(packed_size(chunk) <= cap);
        }
    }

    #[test]
    fn a_record_larger_than_a_node_is_refused() {
        let tree = CowTree::new(Shape::ExtentRef, 1, 4096);
        let big = recs(&[node_capacity(4096, false)]);
        assert!(matches!(
            tree.split(big, 0, false),
            Err(TxnError::InvalidArgument(_))
        ));
    }
}
