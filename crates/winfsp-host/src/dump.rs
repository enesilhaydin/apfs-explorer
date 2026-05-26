//! apfs-dump — oracle probe: walks the APFS USB via the trusted FsView stack
//! and prints a manifest for byte-exact differential comparison.
//!
//! Output format (one line per node):
//!   `<unix-path>`\t`<size>`\t`<sha256-hex>`   (regular file)
//!   `<unix-path>`\tDIR                        (directory)
//!
//! Usage: apfs-dump [--limit N]
//!   --limit N   stop after emitting N regular files (sampled DFS prefix).
//!               Omitted/0 => walk the whole volume.
//!
//! Each line is flushed immediately so a time-bounded or sampled run still
//! yields a usable manifest (a buffered writer that only flushes at the end
//! produces nothing if the walk is interrupted on a slow device).
//!
//! Windows-only: the device stack uses PhysicalDrive (winraw, cfg(windows)).
//! On non-Windows targets this compiles to a stub that exits with an error.

#[cfg(windows)]
mod inner {
    use sha2::{Digest, Sha256};
    use std::io::{self, Write};

    use apfs::FsView;
    use winfsp_host::cache::CachingBlockDevice;
    use winfsp_host::offset_dev::OffsetBlockDevice;
    use winfsp_host::winraw::PhysicalDrive;

    /// Walk state: file budget (`None` = unlimited) and files emitted so far.
    struct State {
        limit: Option<u64>,
        files: u64,
    }

    impl State {
        /// True once the file budget is exhausted (walk should stop).
        fn done(&self) -> bool {
            matches!(self.limit, Some(l) if self.files >= l)
        }
    }

    /// Recursively walk `dir_path`, writing+flushing one manifest line per
    /// node, stopping early once the file budget is reached.
    pub fn walk<D, W>(
        fs: &mut FsView<D>,
        dir_path: &str,
        out: &mut W,
        st: &mut State,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        D: apfs_core::block_device::BlockDevice,
        W: Write,
    {
        let entries = fs.read_dir(dir_path)?;
        for entry in entries {
            if st.done() {
                return Ok(());
            }
            let child_path = if dir_path == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{}/{}", dir_path, entry.name)
            };

            let attr = match fs.getattr(&child_path)? {
                Some(a) => a,
                None => continue,
            };

            if attr.is_dir {
                writeln!(out, "{}\tDIR", child_path)?;
                out.flush()?;
                walk(fs, &child_path, out, st)?;
            } else {
                let content = fs.read(&child_path, 0, usize::MAX)?;
                let mut hasher = Sha256::new();
                hasher.update(&content);
                let digest = hasher.finalize();
                writeln!(out, "{}\t{}\t{:x}", child_path, content.len(), digest)?;
                out.flush()?;
                st.files += 1;
            }
        }
        Ok(())
    }

    fn parse_limit() -> Option<u64> {
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            if a == "--limit" {
                if let Some(v) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    if v > 0 {
                        return Some(v);
                    }
                }
            }
        }
        None
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let stdout = io::stdout();
        let mut out = stdout.lock();

        let (pd, range) =
            PhysicalDrive::autodetect_apfs().map_err(|e| format!("autodetect failed: {e}"))?;
        // Same cached stack as apfs-mount so the oracle reflects production
        // behaviour and stays fast on slow USB.
        let dev = CachingBlockDevice::new(OffsetBlockDevice::new(pd, range.offset, range.length));
        let mut fs = FsView::open(dev).map_err(|e| format!("FsView::open failed: {e}"))?;

        let mut st = State {
            limit: parse_limit(),
            files: 0,
        };
        walk(&mut fs, "/", &mut out, &mut st)?;
        out.flush()?;
        Ok(())
    }
}

fn main() {
    #[cfg(windows)]
    {
        if let Err(e) = inner::run() {
            eprintln!("apfs-dump error: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("apfs-dump is only supported on Windows");
        std::process::exit(1);
    }
}
