//! Data directory resolution. REELVAULT_DATA_DIR wins, otherwise the
//! platform default. Windows parity later reuses this module as is;
//! the LOCALAPPDATA branch already compiles on every platform.

use std::path::PathBuf;

/// Highest precedence: REELVAULT_DATA_DIR (empty values fall through).
pub fn resolve() -> PathBuf {
    if let Ok(dir) = std::env::var("REELVAULT_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    default_dir()
}

pub fn default_dir() -> PathBuf {
    let home = home_dir();

    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/ReelVault")
    } else if cfg!(target_os = "windows") {
        std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or(home)
            .join("ReelVault")
    } else {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".local/share"))
            .join("reelvault")
    }
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
