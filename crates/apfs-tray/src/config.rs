//! Persistent tray configuration stored at
//! `%APPDATA%\APFS Explorer\config.toml`.
//!
//! The file is a minimal hand-parsed TOML subset (no extra dependency):
//! one `key = value` per line, boolean values only.
//!
//! Cross-platform for unit tests; the `appdata_dir` function is
//! `#[cfg(windows)]`-gated.

/// Runtime tray settings loaded from / saved to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayConfig {
    /// Auto-mount APFS drives when plugged in (default: true).
    pub auto_mount: bool,
    /// Mount drives read-only by default (default: false).
    pub default_read_only: bool,
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            auto_mount: true,
            default_read_only: false,
        }
    }
}

impl TrayConfig {
    /// Serialise to a minimal TOML string.
    pub fn to_toml(&self) -> String {
        format!(
            "auto_mount = {}\ndefault_read_only = {}\n",
            self.auto_mount, self.default_read_only
        )
    }

    /// Parse from a minimal TOML string (key = true|false lines only).
    pub fn from_toml(s: &str) -> Self {
        let mut cfg = Self::default();
        for line in s.lines() {
            let line = line.trim();
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim();
                let val = v.trim();
                match key {
                    "auto_mount" => cfg.auto_mount = val == "true",
                    "default_read_only" => cfg.default_read_only = val == "true",
                    _ => {}
                }
            }
        }
        cfg
    }

    /// Load from disk; returns `Default` on any error (first run, corrupt
    /// file, etc.).
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => Self::from_toml(&s),
            Err(_) => Self::default(),
        }
    }

    /// Persist to disk. Errors are silently ignored — config persistence
    /// is best-effort and must never crash the tray.
    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, self.to_toml());
    }
}

/// Returns `%APPDATA%\APFS Explorer\config.toml` on Windows;
/// `None` when the env var is unset (non-Windows CI).
pub fn config_path() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var("APPDATA").ok()?;
        Some(std::path::PathBuf::from(base).join("APFS Explorer").join("config.toml"))
    }
    #[cfg(not(windows))]
    {
        // Non-Windows (CI / macOS unit tests): use a tmp path so tests can
        // exercise save/load without touching real user dirs.
        Some(std::env::temp_dir().join("apfs-explorer-test-config.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_defaults() {
        let cfg = TrayConfig::default();
        let toml = cfg.to_toml();
        let loaded = TrayConfig::from_toml(&toml);
        assert_eq!(cfg, loaded);
    }

    #[test]
    fn round_trip_non_defaults() {
        let cfg = TrayConfig {
            auto_mount: false,
            default_read_only: true,
        };
        let toml = cfg.to_toml();
        let loaded = TrayConfig::from_toml(&toml);
        assert_eq!(cfg, loaded);
    }

    #[test]
    fn unknown_keys_ignored() {
        let s = "unknown_key = true\nauto_mount = false\n";
        let cfg = TrayConfig::from_toml(s);
        assert!(!cfg.auto_mount);
        assert!(!cfg.default_read_only);
    }

    #[test]
    fn empty_string_gives_defaults() {
        assert_eq!(TrayConfig::from_toml(""), TrayConfig::default());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let cfg = TrayConfig {
            auto_mount: false,
            default_read_only: true,
        };
        cfg.save();
        let loaded = TrayConfig::load();
        assert_eq!(cfg, loaded);
    }
}
