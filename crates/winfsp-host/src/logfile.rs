//! Size bound for the mount log, which every run appends to.

use std::path::Path;

/// A log past this size is moved aside when a mount starts.
pub const LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// Move `path` to `<path>.1` (replacing an older one) when it holds more than
/// `limit` bytes, so the log keeps the last run or two and stays bounded.
/// Returns whether the file was moved. A missing log is not an error.
pub fn rotate_if_large(path: &Path, limit: u64) -> std::io::Result<bool> {
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if len <= limit {
        return Ok(false);
    }
    let mut old = path.as_os_str().to_owned();
    old.push(".1");
    std::fs::rename(path, &old)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_over_the_limit_moves_aside_and_replaces_the_older_one() {
        let dir = std::env::temp_dir().join(format!("apfs-logfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("mount.log");
        let old = dir.join("mount.log.1");
        std::fs::write(&old, b"older").unwrap();

        std::fs::write(&log, vec![b'x'; 10]).unwrap();
        assert!(!rotate_if_large(&log, 10).unwrap(), "at the limit it stays");
        assert!(log.exists());

        std::fs::write(&log, vec![b'y'; 11]).unwrap();
        assert!(
            rotate_if_large(&log, 10).unwrap(),
            "past the limit it moves"
        );
        assert!(!log.exists());
        assert_eq!(std::fs::read(&old).unwrap(), vec![b'y'; 11]);

        assert!(!rotate_if_large(&log, 10).unwrap(), "no log, nothing to do");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
