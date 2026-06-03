#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! apfs-core - on-disk APFS structures, checksum, B-tree, `BlockDevice` trait.
//! M0: skeleton only. APFS structures land in the M1 plan (docs/refs/apfs-spec-sources.md).

pub mod block_device;
pub use block_device::{
    BlockDevice, BlockError, FileBlockDeviceRw, WritableBlockDevice, WritableOffsetBlockDevice,
};

pub mod endian;
pub use endian::ParseError;

pub mod checksum;

pub mod obj;
pub use obj::{ObjPhys, OBJECT_TYPE_FS};

pub mod nx;
pub use nx::NxSuperblock;

pub mod container;
pub use container::{Container, ContainerError, VolumeInfo};

pub mod checkpoint;

pub mod btree;
pub use btree::BtreeNode;

pub mod omap;
pub use omap::{Omap, OmapPhys};

pub mod volume;
pub use volume::{VolumeRole, VolumeSuperblock};

pub mod jkey;
pub use jkey::{JKey, ROOT_DIR_INO_NUM};

pub mod catalog;
pub use catalog::{Catalog, DirEntry};

pub mod inode;
pub use inode::{Inode, JDstream};

pub mod xattr;
pub use xattr::{XattrDstream, XattrEntry};

pub mod lzvn;
pub use lzvn::lzvn_decode;

pub mod decmpfs;
pub use decmpfs::{decompress as decmpfs_decompress, DecmpfsError, DecmpfsHeader};
