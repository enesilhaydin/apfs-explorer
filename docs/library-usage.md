# Library Usage Guide

`apfs-explorer` is structured as a set of composable Rust crates that can be used
as a library independently of the CLI/mount surface. This guide shows practical
usage patterns for embedding APFS read and write support in your own application.

> **Publication status:** Crates are not yet published to crates.io. Add via path
> or git dependency until the first release.
>
> ```toml
> # Cargo.toml — path dependency (local checkout)
> [dependencies]
> apfs-core = { path = "../apfs-explorer/crates/apfs-core" }
> apfs       = { path = "../apfs-explorer/crates/apfs" }
> apfs-write = { path = "../apfs-explorer/crates/apfs-write" }
>
> # Cargo.toml — git dependency (pinned commit)
> apfs-core = { git = "https://github.com/your-org/apfs-explorer", rev = "v1.0.0-rc1" }
>
> # Cargo.toml: apfs-core = "1.0"  // pending crates.io publish
> ```

---

## Crate Overview

| Crate | Role | Platform |
|-------|------|----------|
| `apfs-core` | On-disk structs, B-tree engine, block device trait | cross-platform |
| `apfs` | Read-only volume view, high-level API | cross-platform |
| `apfs-write` | Write path, transactions, CoW, snapshots | cross-platform |
| `apfs-mount` | WinFSP/FUSE mount binary | Windows / Linux |

The library crates (`apfs-core`, `apfs`, `apfs-write`) are fully
platform-independent. The mount binary requires WinFsp on Windows or FUSE on Linux.

---

## Block Device Setup

All operations begin with a block device. Two common setups:

### Raw image file

```rust
use apfs_core::block_device::FileBlockDevice;

let dev = FileBlockDevice::open("/path/to/volume.img")?;
```

### GPT disk image (auto-detect APFS partition)

```rust
use apfs_core::block_device::{FileBlockDevice, OffsetBlockDevice};
use apfs_core::gpt::find_apfs_partition;

let disk = FileBlockDevice::open("/path/to/disk.img")?;
let (offset, length) = find_apfs_partition(&disk)?;
let dev = OffsetBlockDevice::new(disk, offset, length);
```

### Physical disk (Windows raw device path)

```rust
use apfs_core::block_device::FileBlockDevice;

// Requires elevated privileges on Windows
let dev = FileBlockDevice::open(r"\\.\PhysicalDrive1")?;
```

---

## Read-Side API

### Open container and list volumes

```rust
use apfs::Container;

let container = Container::open(dev)?;

for vol in container.volumes()? {
    println!(
        "volume: {:?}  role: {:?}  case_sensitive: {}",
        vol.name(),
        vol.role(),
        vol.is_case_sensitive()
    );
}
```

### Open a volume and look up a file

```rust
use apfs::FsView;

let vol = container.volume_by_name("Data")?;
let fs = FsView::new(&vol)?;

let inode = fs.lookup("/Users/alice/Documents/notes.txt")?;
println!("size: {}  mtime: {:?}", inode.size(), inode.mtime());
```

### Read file contents

```rust
use std::io::Read;

let mut file = fs.open_file("/Users/alice/Documents/notes.txt")?;
let mut buf = Vec::new();
file.read_to_end(&mut buf)?;
println!("{}", String::from_utf8_lossy(&buf));
```

### Read directory entries

```rust
for entry in fs.read_dir("/Users/alice/")? {
    let entry = entry?;
    println!("{:?}  ino={}  type={:?}", entry.name(), entry.inode_id(), entry.file_type());
}
```

### Read extended attributes

```rust
// List all xattrs on a file
let xattrs = fs.list_xattrs("/Users/alice/photo.jpg")?;
for name in &xattrs {
    println!("xattr: {name}");
}

// Read a specific xattr value
let value = fs.get_xattr("/Users/alice/photo.jpg", "com.apple.quarantine")?;
println!("{}", String::from_utf8_lossy(&value));
```

### Read symbolic link target

```rust
let target = fs.read_link("/usr/local/bin/python")?;
println!("-> {:?}", target);
```

---

## Write-Side API

All mutations go through a `Transaction`. A transaction is atomic: either
the entire set of changes commits or none of them do.

### Begin a transaction

```rust
use apfs_write::Transaction;

let vol = container.volume_by_name("Data")?;
let mut tx = Transaction::begin(&vol)?;
```

### Create a file

```rust
tx.create_file("/tmp/hello.txt", &[])?;
```

### Write file contents

```rust
tx.write_file("/tmp/hello.txt", b"Hello, APFS!\n")?;
```

### Append to a file

```rust
use std::io::{Seek, SeekFrom, Write};

let mut f = tx.open_file_write("/tmp/hello.txt")?;
f.seek(SeekFrom::End(0))?;
f.write_all(b"another line\n")?;
```

### Rename a file

```rust
tx.rename("/tmp/hello.txt", "/tmp/renamed.txt")?;
```

### Clone a file (copy-on-write reflink)

```rust
// Instant CoW clone — shares extents until either copy is modified
tx.clone_file("/tmp/original.txt", "/tmp/clone.txt")?;
```

### Set extended attribute

```rust
tx.set_xattr("/tmp/hello.txt", "user.note", b"created by apfs-write")?;
```

### Create a directory

```rust
tx.create_dir("/tmp/mydir")?;
```

### Delete a file

```rust
tx.remove_file("/tmp/renamed.txt")?;
```

### Commit the transaction

```rust
tx.commit()?;
```

### Rollback (explicit, or automatic on drop)

```rust
tx.rollback()?;  // explicit
// or simply drop(tx) without calling commit()
```

---

## Snapshot API

```rust
use apfs_write::{Transaction, SnapshotManager};

// Create a named snapshot of the current volume state
let mut tx = Transaction::begin(&vol)?;
tx.create_snapshot("backup-2026-05-26")?;
tx.commit()?;

// List all snapshots
let mgr = SnapshotManager::new(&vol)?;
for snap in mgr.list_snapshots()? {
    println!("{:?}  xid={}", snap.name(), snap.transaction_id());
}

// Delete a snapshot
let mut tx = Transaction::begin(&vol)?;
tx.delete_snapshot("backup-2026-05-26")?;
tx.commit()?;

// Revert volume to a snapshot (destructive — data after snapshot is lost)
let mut tx = Transaction::begin(&vol)?;
tx.revert_to_snapshot("backup-2026-05-26")?;
tx.commit()?;
```

---

## Error Handling

All public APIs return `Result<T, apfs_core::Error>`. The `Error` type is
non-exhaustive and implements `std::error::Error`. No `unwrap`, `expect`, or
`panic!` propagates through the library boundary.

```rust
use apfs_core::Error;

match fs.lookup("/nonexistent") {
    Ok(inode) => { /* ... */ }
    Err(Error::NotFound) => eprintln!("path not found"),
    Err(Error::PermissionDenied) => eprintln!("access denied"),
    Err(Error::Io(io_err)) => eprintln!("I/O error: {io_err}"),
    Err(e) => eprintln!("unexpected error: {e}"),
}
```

For write paths, always handle transaction errors and ensure rollback:

```rust
fn safe_write(vol: &Volume) -> Result<(), apfs_core::Error> {
    let mut tx = Transaction::begin(vol)?;
    tx.create_file("/tmp/test.txt", &[])?;
    tx.write_file("/tmp/test.txt", b"data")?;
    tx.commit()?;
    Ok(())
    // If any step returns Err, tx is dropped and auto-rolled-back
}
```

---

## Cross-Platform Notes

- `apfs-core`, `apfs`, `apfs-write` compile on Windows, macOS, and Linux.
- No platform-specific syscalls are used in the library crates.
- `apfs-mount` (the binary) requires WinFsp on Windows; on Linux it requires FUSE.
- File path separators inside APFS are always `/` regardless of host OS.
- On Windows, pass raw device paths like `\\.\PhysicalDrive1` to `FileBlockDevice::open`.

---

## Crate Cross-References

```
apfs-mount
  └── apfs-write   (write path, transactions)
       └── apfs    (read-only volume view)
            └── apfs-core  (block device, B-tree, on-disk structs)
```

Each higher-level crate re-exports the types it builds on. For most use cases,
depending only on `apfs` (read) or `apfs-write` (read+write) is sufficient.
