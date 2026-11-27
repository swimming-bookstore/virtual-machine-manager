//! CPU and memory beside a guest disk. Swift draws the sliders and boots with these numbers.
//! A missing file is 2 CPUs and 2 GB, the previous boot.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

pub const FILE: &str = "hardware.json";
pub const DEFAULT_CPU: u32 = 2;
pub const DEFAULT_MEMORY_GB: u32 = 2;

#[derive(Serialize)]
pub struct Hardware {
    pub cpu: u32,
    #[serde(rename = "memoryGB")]
    pub memory_gb: u32,
}

pub fn path(disk: &Path) -> PathBuf {
    disk.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(FILE)
}

/// Missing or broken file is the default. Swift clamps to what this Mac will boot.
pub fn load(disk: &Path) -> Hardware {
    let Ok(text) = fs::read_to_string(path(disk)) else {
        return defaults();
    };
    serde_json::from_str::<Stored>(&text)
        .ok()
        .filter(|stored| stored.cpu >= 1 && stored.memory_gb >= 1)
        .map(|stored| Hardware {
            cpu: stored.cpu,
            memory_gb: stored.memory_gb,
        })
        .unwrap_or_else(defaults)
}

pub fn save(disk: &Path, cpu: u32, memory_gb: u32) -> Result<Hardware> {
    if !disk.is_file() {
        return Err(anyhow!("guest disk is missing"));
    }
    if cpu < 1 || memory_gb < 1 {
        return Err(anyhow!("cpu and memory must be at least 1"));
    }
    let hardware = Hardware { cpu, memory_gb };
    let dest = path(disk);
    let tmp = dest.with_extension("partial");
    fs::write(&tmp, serde_json::to_vec(&hardware)?)?;
    fs::rename(&tmp, &dest).map_err(|err| anyhow!("cannot write {}: {err}", dest.display()))?;
    Ok(hardware)
}

fn defaults() -> Hardware {
    Hardware {
        cpu: DEFAULT_CPU,
        memory_gb: DEFAULT_MEMORY_GB,
    }
}

#[derive(Deserialize)]
struct Stored {
    cpu: u32,
    #[serde(rename = "memoryGB")]
    memory_gb: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_the_default_and_save_round_trips() {
        let dir = std::env::temp_dir().join(format!("vmm-hw-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.img");
        assert_eq!(load(&disk).cpu, DEFAULT_CPU);
        assert!(save(&disk, 4, 8).is_err());
        fs::write(&disk, b"disk").unwrap();
        save(&disk, 4, 8).unwrap();
        let hardware = load(&disk);
        assert_eq!(hardware.cpu, 4);
        assert_eq!(hardware.memory_gb, 8);
        fs::write(path(&disk), b"nope").unwrap();
        assert_eq!(load(&disk).memory_gb, DEFAULT_MEMORY_GB);
        let _ = fs::remove_dir_all(&dir);
    }
}
