//! Pause files beside a guest disk. Swift writes the Virtualization save;
//! this records that it is there and removes it on Stop.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};

pub const MARKER: &str = "paused";
pub const STATE: &str = "paused.vzvmsave";
pub const PREVIEW: &str = "paused.png";

pub fn marker(disk: &Path) -> PathBuf {
    beside(disk).join(MARKER)
}

pub fn state(disk: &Path) -> PathBuf {
    beside(disk).join(STATE)
}

pub fn preview(disk: &Path) -> PathBuf {
    beside(disk).join(PREVIEW)
}

/// Both files. A marker alone is not a paused machine.
pub fn is_paused(disk: &Path) -> bool {
    marker(disk).is_file() && state(disk).is_file()
}

/// Record that Swift has written the save. Missing state is not paused.
pub fn mark(disk: &Path) -> Result<()> {
    if !disk.is_file() {
        bail!("guest disk is missing");
    }
    let state = state(disk);
    if !state.is_file() {
        bail!("pause state file is missing");
    }
    let dest = marker(disk);
    let tmp = dest.with_extension("partial");
    fs::write(&tmp, b"paused\n")?;
    fs::rename(&tmp, dest)?;
    if !is_paused(disk) {
        bail!("pause files are missing");
    }
    Ok(())
}

pub fn clear(disk: &Path) -> Result<()> {
    for path in [marker(disk), state(disk), preview(disk)] {
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|err| anyhow!("cannot remove {}: {err}", path.display()))?;
        }
    }
    Ok(())
}

fn beside(disk: &Path) -> PathBuf {
    disk.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_needs_the_save_file_and_clear_removes_both() {
        let dir = std::env::temp_dir().join(format!("vmm-pause-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.img");
        fs::write(&disk, b"disk").unwrap();
        assert!(!is_paused(&disk));
        assert!(mark(&disk).unwrap_err().to_string().contains("pause state"));
        fs::write(state(&disk), b"vz-state").unwrap();
        mark(&disk).unwrap();
        assert!(is_paused(&disk));
        assert!(fs::read_to_string(marker(&disk))
            .unwrap()
            .starts_with("paused"));
        fs::write(preview(&disk), b"png").unwrap();
        clear(&disk).unwrap();
        assert!(!is_paused(&disk));
        assert!(!marker(&disk).exists());
        assert!(!state(&disk).exists());
        assert!(!preview(&disk).exists());
        clear(&disk).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
