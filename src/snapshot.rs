//! Frozen Debian disk. The install is finished when the host can SSH in.
//! The snapshot is the disk the installer wrote. Later machines clone it and skip the installer.

#[cfg(any(target_os = "linux", test))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};

use crate::preseed::{DISK, GUEST, MACHINE, SNAPSHOT};
use crate::ssh;

#[cfg(test)]
use crate::preseed;

const READY: &str = "ready";
const NVRAM: &str = "nvram.bin";
const MACHINE_ID: &str = "machine-id";
const SEEK_DATA: i32 = 3;
const SEEK_HOLE: i32 = 4;
const ENXIO: i32 = 6;
const EINVAL: i32 = 22;

#[derive(Debug)]
pub struct Saved {
    pub dir: PathBuf,
    pub disk: PathBuf,
}

/// Copy the finished guest once. A second call keeps the first snapshot.
pub fn save(support: &Path) -> Result<Saved> {
    let guest = support.join(SNAPSHOT);
    let disk = guest.join(DISK);
    if !ssh::logged_in(&support.join("ssh"))? && !ssh::logged_in_beside(&disk)? {
        bail!("install is not finished: SSH has not logged in");
    }
    installed(&disk)?;
    // The next Quick start reads this file beside the disk, not only the app-wide proof.
    ssh::record_login_beside(&disk)?;
    let dest = support.join(SNAPSHOT);
    if snapshot_ready(&dest)? {
        return Ok(saved(&dest));
    }
    fs::create_dir_all(&dest).map_err(|err| anyhow!("cannot create {}: {err}", dest.display()))?;
    // The installer writes snapshots/debian/disk.img. That file is the snapshot, so it is not copied onto itself.
    copy_beside(&guest, &dest, &[DISK, NVRAM, MACHINE_ID])?;
    write_ready(&dest.join(READY))?;
    let prev = support.join(format!("{SNAPSHOT}.prev"));
    if prev.exists() {
        let _ = fs::remove_dir_all(&prev);
    }
    Ok(saved(&dest))
}

/// Clone the snapshot into `machines/NAME`. The copy shares blocks when the disk allows it.
pub fn clone_machine(support: &Path, name: Option<&str>) -> Result<Saved> {
    let current = support.join("snapshots").join(MACHINE);
    let prev = support.join(format!("{SNAPSHOT}.prev"));
    let src = if snapshot_ready(&current)? {
        current
    } else if snapshot_ready(&prev)? {
        prev
    } else {
        bail!("no Debian snapshot yet. Finish the installer first.");
    };
    let machines = support.join("machines");
    fs::create_dir_all(&machines)
        .map_err(|err| anyhow!("cannot create {}: {err}", machines.display()))?;
    let name = match name {
        Some(name) => folder_name(name)?,
        None => next_name(&machines)?,
    };
    let dest = machines.join(&name);
    if dest.exists() {
        bail!("{name} already exists");
    }
    let stage = machines.join(format!(".{name}.partial"));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage)
        .map_err(|err| anyhow!("cannot create {}: {err}", stage.display()))?;
    copy_beside(&src, &stage, &[DISK, NVRAM, MACHINE_ID])?;
    fs::rename(&stage, &dest)?;
    Ok(saved(&dest))
}

/// Rename a clone. The folder under machines/ is the name, so the directory moves with it.
pub fn rename_machine(support: &Path, disk: &Path, name: &str) -> Result<Saved> {
    let name = folder_name(name)?;
    let machines = support.join("machines");
    let src = machine_dir(support, disk)?;
    let dest = machines.join(&name);
    if src == dest {
        return Ok(saved(&src));
    }
    if dest.exists() {
        bail!("{name} already exists");
    }
    fs::rename(&src, &dest).map_err(|err| {
        anyhow!(
            "cannot rename {} to {}: {err}",
            src.display(),
            dest.display()
        )
    })?;
    Ok(saved(&dest))
}

/// Delete a clone. The folder under machines/ goes away. The snapshot is not a clone.
pub fn delete_machine(support: &Path, disk: &Path) -> Result<Saved> {
    let src = machine_dir(support, disk)?;
    let saved = saved(&src);
    fs::remove_dir_all(&src).map_err(|err| anyhow!("cannot delete {}: {err}", src.display()))?;
    Ok(saved)
}

/// The clone directory that owns this disk. Only machines/<name>/disk.img can move.
fn machine_dir(support: &Path, disk: &Path) -> Result<PathBuf> {
    let file = disk
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("that disk is not a virtual machine"))?;
    let parent = disk
        .parent()
        .ok_or_else(|| anyhow!("that disk is not a virtual machine"))?;
    let name = parent
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("that disk is not a virtual machine"))?;
    let machines = parent
        .parent()
        .ok_or_else(|| anyhow!("that disk is not a virtual machine"))?;
    // Compare the path the app stored. canonicalize follows the support-dir symlink and would miss it.
    if file != DISK || machines != support.join("machines") {
        bail!("that disk is not a virtual machine");
    }
    machine_name(name)?;
    if !disk.is_file() {
        bail!("that virtual machine is missing");
    }
    Ok(parent.to_path_buf())
}

fn copy_beside(src: &Path, dest: &Path, names: &[&str]) -> Result<()> {
    for name in names {
        let from = src.join(name);
        let to = dest.join(name);
        if from.is_file() && from != to {
            place(&from, &to)?;
        }
    }
    Ok(())
}

fn saved(dir: &Path) -> Saved {
    Saved {
        dir: dir.to_path_buf(),
        disk: dir.join(DISK),
    }
}

/// Both files, and a real installed disk. A removed or broken copy is not ready, so save replaces it.
pub fn is_ready(dir: &Path) -> bool {
    snapshot_ready(dir).unwrap_or(false)
}

fn snapshot_ready(dir: &Path) -> Result<bool> {
    let ready = dir.join(READY);
    let disk = dir.join(DISK);
    if !ready.is_file() || !disk.is_file() {
        return Ok(false);
    }
    let Ok(text) = fs::read_to_string(&ready) else {
        return Ok(false);
    };
    if !text.starts_with("ssh-ok") {
        return Ok(false);
    }
    Ok(installed(&disk).is_ok())
}

fn installed(disk: &Path) -> Result<()> {
    if !disk.is_file() {
        bail!("install is not finished: guest disk is missing");
    }
    let mut file = File::open(disk)?;
    let mut sector = [0u8; 512];
    file.read_exact(&mut sector)
        .map_err(|_| anyhow!("install is not finished: guest disk is too small"))?;
    if sector[510] != 0x55 || sector[511] != 0xaa {
        bail!("install is not finished: the guest disk has no partition table");
    }
    let len = file.metadata()?.len();
    if len < GUEST {
        bail!("install is not finished: guest disk is {len} bytes");
    }
    Ok(())
}

fn atomic(dest: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let tmp = dest.with_extension("partial");
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    write(&tmp)?;
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn write_ready(dest: &Path) -> Result<()> {
    atomic(dest, |tmp| {
        let mut file = File::create(tmp)?;
        file.write_all(b"ssh-ok\n")?;
        file.sync_all()?;
        Ok(())
    })
}

fn place(src: &Path, dest: &Path) -> Result<()> {
    if dest.is_file() {
        fs::remove_file(dest)?;
    }
    atomic(dest, |tmp| {
        copy_sparse(src, tmp)
            .map_err(|err| anyhow!("cannot copy {} to {}: {err}", src.display(), dest.display()))
    })
}

const FIRST: &str = "My Debian";

/// The popup's starting text. The typed name is still what Create uses.
pub fn suggest_name() -> String {
    format!("{FIRST} {}", random_code(4))
}

fn random_code(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    let got = File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok();
    if !got {
        let tick = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = (tick >> (i * 8)) as u8;
        }
    }
    const ABC: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    bytes
        .into_iter()
        .map(|byte| ABC[byte as usize % ABC.len()] as char)
        .collect()
}

/// "My Debian" is the folder My Debian. The popup sends the typed text; it does not invent the folder.
fn folder_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("machine name is empty");
    }
    machine_name(name)?;
    Ok(name.to_string())
}

fn machine_name(name: &str) -> Result<()> {
    // The folder is the typed name. The only reserved folder is the snapshot name, debian.
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 64
        && !name.starts_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'));
    if !ok {
        bail!("that name cannot be used");
    }
    if name == MACHINE {
        bail!("{name} is the snapshot name, not a clone");
    }
    Ok(())
}

fn next_name(machines: &Path) -> Result<String> {
    for n in 2..1000 {
        let name = format!("{MACHINE}-{n}");
        if !machines.join(&name).exists() {
            return Ok(name);
        }
    }
    bail!("no free machine name")
}

fn copy_sparse(src: &Path, dest: &Path) -> Result<()> {
    if try_clone(src, dest).is_ok() {
        return Ok(());
    }
    let _ = fs::remove_file(dest);
    copy_holes(src, dest)
}

fn try_clone(src: &Path, dest: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        ficlone(src, dest)
    }
    #[cfg(target_os = "macos")]
    {
        clonefile_copy(src, dest)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (src, dest);
        bail!("this system has no clone")
    }
}

#[cfg(target_os = "linux")]
fn ficlone(src: &Path, dest: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let input = File::open(src)?;
    let output = OpenOptions::new().write(true).create_new(true).open(dest)?;
    // FICLONE = _IOW(0x94, 9, int)
    const FICLONE: u64 = 0x4004_9409;
    extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }
    let rc = unsafe { ioctl(output.as_raw_fd(), FICLONE, input.as_raw_fd()) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        drop(output);
        let _ = fs::remove_file(dest);
        bail!("reflink: {err}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clonefile_copy(src: &Path, dest: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::raw::c_char;
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn clonefile(src: *const c_char, dest: *const c_char, flags: u32) -> i32;
    }
    let src =
        CString::new(src.as_os_str().as_bytes()).map_err(|_| anyhow!("source path has a null"))?;
    let dest = CString::new(dest.as_os_str().as_bytes())
        .map_err(|_| anyhow!("destination path has a null"))?;
    let rc = unsafe { clonefile(src.as_ptr(), dest.as_ptr(), 0) };
    if rc != 0 {
        bail!("clonefile: {}", io::Error::last_os_error());
    }
    Ok(())
}

/// Copy only allocated extents. A 16 GiB guest disk is mostly holes.
fn copy_holes(src: &Path, dest: &Path) -> Result<()> {
    let mut input = File::open(src)?;
    let len = input.metadata()?.len();
    if !holes_supported(&input)? {
        return copy_skip_zeros(&mut input, dest, len);
    }
    let mut output = File::create(dest)?;
    output.set_len(len)?;
    let mut offset = 0u64;
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let data = match seek_kind(&input, offset, SEEK_DATA) {
            Ok(at) if at < len => at,
            Ok(_) => break,
            Err(err) if err.raw_os_error() == Some(ENXIO) => break,
            Err(err) => return Err(err.into()),
        };
        let hole = match seek_kind(&input, data, SEEK_HOLE) {
            Ok(at) => at.min(len),
            Err(err) if err.raw_os_error() == Some(ENXIO) => len,
            Err(err) => return Err(err.into()),
        };
        input.seek(SeekFrom::Start(data))?;
        output.seek(SeekFrom::Start(data))?;
        let mut left = hole - data;
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            let n = input.read(&mut buf[..want])?;
            if n == 0 {
                bail!("short read copying {}", src.display());
            }
            output.write_all(&buf[..n])?;
            left -= n as u64;
        }
        offset = hole;
        if offset >= len {
            break;
        }
    }
    output.sync_all()?;
    Ok(())
}

fn holes_supported(file: &File) -> Result<bool> {
    match seek_kind(file, 0, SEEK_DATA) {
        Ok(_) => Ok(true),
        Err(err) if err.raw_os_error() == Some(ENXIO) => Ok(true),
        Err(err) if err.raw_os_error() == Some(EINVAL) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn copy_skip_zeros(input: &mut File, dest: &Path, len: u64) -> Result<()> {
    input.seek(SeekFrom::Start(0))?;
    let mut output = File::create(dest)?;
    output.set_len(len)?;
    let mut buf = vec![0u8; 1024 * 1024];
    let mut offset = 0u64;
    while offset < len {
        let want = ((len - offset) as usize).min(buf.len());
        let n = input.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().any(|byte| *byte != 0) {
            output.seek(SeekFrom::Start(offset))?;
            output.write_all(&buf[..n])?;
        }
        offset += n as u64;
    }
    output.sync_all()?;
    Ok(())
}

fn seek_kind(file: &File, offset: u64, kind: i32) -> io::Result<u64> {
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn lseek(fd: i32, offset: i64, whence: i32) -> i64;
    }
    let rc = unsafe { lseek(file.as_raw_fd(), offset as i64, kind) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, Instant};

    #[test]
    fn a_finished_install_snapshots_once_and_clones_fast() {
        let dir = std::env::temp_dir().join(format!(
            "vmm-snap-{}-{}",
            std::process::id(),
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("cache")).unwrap();
        fs::write(
            dir.join("cache").join(preseed::EXAMPLE),
            "#_preseed_V1\nd-i debian-installer/locale string en_US.UTF-8\n",
        )
        .unwrap();
        fs::create_dir_all(dir.join("ssh")).unwrap();
        fs::write(
            dir.join("ssh").join("authorized_keys"),
            b"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKeyForTheInstaller\n",
        )
        .unwrap();
        let iso = dir.join("cache").join("debian.iso");
        fs::write(&iso, b"iso").unwrap();
        let prepared = preseed::prepare(&iso, &dir).unwrap().0;
        assert!(save(&dir)
            .unwrap_err()
            .to_string()
            .contains("install is not finished"));
        assert!(clone_machine(&dir, None)
            .unwrap_err()
            .to_string()
            .contains("no Debian snapshot"));

        mark_installed(&prepared.disk);
        fs::write(prepared.disk.parent().unwrap().join(NVRAM), b"efi-vars").unwrap();
        fs::write(prepared.disk.parent().unwrap().join(MACHINE_ID), b"machine").unwrap();
        assert!(save(&dir)
            .unwrap_err()
            .to_string()
            .contains("SSH has not logged in"));
        ssh::record_login(&dir.join("ssh")).unwrap();

        let saved = save(&dir).unwrap();
        assert_eq!(saved.disk, prepared.disk);
        assert_eq!(saved.disk.metadata().unwrap().len(), GUEST);
        assert!(saved.disk.metadata().unwrap().blocks() < 256);
        assert_eq!(fs::read(saved.dir.join(NVRAM)).unwrap(), b"efi-vars");
        assert_eq!(fs::read(saved.dir.join(MACHINE_ID)).unwrap(), b"machine");
        assert!(fs::read_to_string(saved.dir.join(READY))
            .unwrap()
            .starts_with("ssh-ok"));
        // A second save keeps the disk the installer wrote. It is not a second copy.
        let mut live = OpenOptions::new().write(true).open(&saved.disk).unwrap();
        live.seek(SeekFrom::Start(600)).unwrap();
        live.write_all(b"CANARY").unwrap();
        drop(live);
        let again = save(&dir).unwrap();
        let mut buf = [0u8; 6];
        let mut frozen = File::open(&again.disk).unwrap();
        frozen.seek(SeekFrom::Start(600)).unwrap();
        frozen.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"CANARY");
        assert_eq!(fs::read(again.dir.join(NVRAM)).unwrap(), b"efi-vars");

        let started = Instant::now();
        let cloned = clone_machine(&dir, None).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "clone took {:?}",
            started.elapsed()
        );
        assert_eq!(cloned.disk, dir.join("machines/debian-2").join(DISK));
        assert_eq!(cloned.disk.metadata().unwrap().len(), GUEST);
        assert!(cloned.disk.metadata().unwrap().blocks() < 256);
        assert_eq!(fs::read(cloned.dir.join(NVRAM)).unwrap(), b"efi-vars");
        assert_eq!(fs::read(cloned.dir.join(MACHINE_ID)).unwrap(), b"machine");
        let mut cloned_buf = [0u8; 6];
        let mut cloned_disk = File::open(&cloned.disk).unwrap();
        cloned_disk.seek(SeekFrom::Start(600)).unwrap();
        cloned_disk.read_exact(&mut cloned_buf).unwrap();
        assert_eq!(&cloned_buf, b"CANARY");

        let named = clone_machine(&dir, Some("lab")).unwrap();
        assert_eq!(named.dir, dir.join("machines/lab"));
        assert!(clone_machine(&dir, Some("lab")).is_err());
        assert!(clone_machine(&dir, Some("../x")).is_err());
        assert!(clone_machine(&dir, Some("")).is_err());
        assert!(clone_machine(&dir, Some("debian")).is_err());
        let pretty = clone_machine(&dir, Some("My Debian")).unwrap();
        assert_eq!(pretty.dir, dir.join("machines/My Debian"));
        assert!(clone_machine(&dir, Some("My Debian"))
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        assert!(clone_machine(&dir, Some("a/b")).is_err());
        assert!(clone_machine(&dir, Some(".hidden")).is_err());

        let renamed = rename_machine(&dir, &pretty.disk, "Other Debian").unwrap();
        assert_eq!(renamed.dir, dir.join("machines/Other Debian"));
        assert_eq!(renamed.disk, dir.join("machines/Other Debian").join(DISK));
        assert!(!pretty.dir.exists());
        assert_eq!(fs::read(renamed.dir.join(NVRAM)).unwrap(), b"efi-vars");
        let same = rename_machine(&dir, &renamed.disk, "Other Debian").unwrap();
        assert_eq!(same.dir, renamed.dir);
        assert!(rename_machine(&dir, &renamed.disk, "lab").is_err());
        assert!(rename_machine(&dir, &renamed.disk, "debian").is_err());
        assert!(rename_machine(&dir, &renamed.disk, "../x").is_err());
        assert!(rename_machine(&dir, &dir.join("snapshots/debian").join(DISK), "nope").is_err());

        let removed = delete_machine(&dir, &renamed.disk).unwrap();
        assert_eq!(removed.dir, renamed.dir);
        assert!(!renamed.dir.exists());
        assert!(delete_machine(&dir, &renamed.disk).is_err());
        assert!(delete_machine(&dir, &dir.join("snapshots/debian").join(DISK)).is_err());
        assert!(dir.join("machines/lab").exists());

        // Deleting the snapshot removes the disk the installer wrote. There is no second copy to restore.
        fs::remove_dir_all(dir.join(SNAPSHOT)).unwrap();
        assert!(save(&dir)
            .unwrap_err()
            .to_string()
            .contains("guest disk is missing"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_display_name_keeps_its_spaces() {
        assert_eq!(folder_name("  Other   Debian ").unwrap(), "Other   Debian");
        assert_eq!(folder_name("My-Debian").unwrap(), "My-Debian");
        assert_eq!(folder_name("My Debian").unwrap(), "My Debian");
        assert!(folder_name("debian")
            .unwrap_err()
            .to_string()
            .contains("snapshot name"));
        assert_eq!(folder_name("Debian").unwrap(), "Debian");
        assert!(folder_name("a/b").is_err());
        assert!(folder_name(" ")
            .unwrap_err()
            .to_string()
            .contains("empty"));
        assert!(folder_name(".hidden").is_err());
        assert!(folder_name(&"n".repeat(65)).is_err());
        assert_eq!(folder_name(&"n".repeat(64)).unwrap().len(), 64);
        let suggested = suggest_name();
        let code = suggested.strip_prefix("My Debian ").unwrap();
        assert_eq!(code.len(), 4);
        assert!(code.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
        assert!(folder_name(&suggested).is_ok());
    }

    fn mark_installed(disk: &Path) {
        let mut file = OpenOptions::new().write(true).open(disk).unwrap();
        file.seek(SeekFrom::Start(510)).unwrap();
        file.write_all(&[0x55, 0xaa]).unwrap();
        file.sync_all().unwrap();
    }
}
