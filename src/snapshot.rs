//! Frozen Debian disk. The install is finished when the host can SSH in.
//! The snapshot is a sparse copy of that stopped disk, so later machines skip the installer.

#[cfg(any(target_os = "linux", test))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};

use crate::preseed::{DISK, GUEST, MACHINE};

#[cfg(test)]
use crate::preseed;

#[path = "ssh.rs"]
mod ssh;

const READY: &str = "ready";
const NVRAM: &str = "nvram.bin";
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
    let guest = support.join("machines").join(MACHINE);
    let disk = guest.join(DISK);
    if !ssh::logged_in(&support.join("ssh"))? && !ssh::logged_in_beside(&disk)? {
        bail!("install is not finished: SSH has not logged in");
    }
    installed(&disk)?;
    // The next Quick start reads this file beside the disk, not only the app-wide proof.
    ssh::record_login_beside(&disk)?;
    let dest = support.join("snapshots").join(MACHINE);
    if snapshot_ready(&dest)? {
        return Ok(saved(&dest));
    }
    fs::create_dir_all(&dest).map_err(|err| anyhow!("cannot create {}: {err}", dest.display()))?;
    place(&disk, &dest.join(DISK))?;
    let nvram = guest.join(NVRAM);
    if nvram.is_file() {
        place(&nvram, &dest.join(NVRAM))?;
    }
    write_ready(&dest.join(READY))?;
    Ok(saved(&dest))
}

/// Clone the snapshot into `machines/NAME`. The copy shares blocks when the disk allows it.
pub fn clone_machine(support: &Path, name: Option<&str>) -> Result<Saved> {
    let src = support.join("snapshots").join(MACHINE);
    if !snapshot_ready(&src)? {
        bail!("no Debian snapshot yet. Finish the installer first.");
    }
    let machines = support.join("machines");
    fs::create_dir_all(&machines)
        .map_err(|err| anyhow!("cannot create {}: {err}", machines.display()))?;
    let name = match name {
        Some(name) => {
            machine_name(name)?;
            name.to_string()
        }
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
    place(&src.join(DISK), &stage.join(DISK))?;
    let nvram = src.join(NVRAM);
    if nvram.is_file() {
        place(&nvram, &stage.join(NVRAM))?;
    }
    fs::rename(&stage, &dest)?;
    Ok(saved(&dest))
}

fn saved(dir: &Path) -> Saved {
    Saved {
        dir: dir.to_path_buf(),
        disk: dir.join(DISK),
    }
}

/// Both files, and a real installed disk. A removed or broken copy is not ready, so save replaces it.
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
    if len != GUEST {
        bail!("install is not finished: guest disk is {len} bytes");
    }
    Ok(())
}

fn write_ready(dest: &Path) -> Result<()> {
    let tmp = dest.with_extension("partial");
    let mut file = File::create(&tmp)?;
    file.write_all(b"ssh-ok\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn place(src: &Path, dest: &Path) -> Result<()> {
    if dest.is_file() {
        fs::remove_file(dest)?;
    }
    let tmp = dest.with_extension("partial");
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    copy_sparse(src, &tmp)
        .map_err(|err| anyhow!("cannot copy {} to {}: {err}", src.display(), dest.display()))?;
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn machine_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        bail!("machine name {name} is not usable");
    }
    if name == MACHINE {
        bail!("{name} is the installer disk, not a clone");
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
        assert!(save(&dir)
            .unwrap_err()
            .to_string()
            .contains("SSH has not logged in"));
        ssh::record_login(&dir.join("ssh")).unwrap();

        let saved = save(&dir).unwrap();
        assert_eq!(saved.disk.metadata().unwrap().len(), GUEST);
        assert!(saved.disk.metadata().unwrap().blocks() < 256);
        assert_eq!(fs::read(saved.dir.join(NVRAM)).unwrap(), b"efi-vars");
        assert!(fs::read_to_string(saved.dir.join(READY))
            .unwrap()
            .starts_with("ssh-ok"));

        let mut snap = OpenOptions::new().write(true).open(&saved.disk).unwrap();
        snap.seek(SeekFrom::Start(600)).unwrap();
        snap.write_all(b"CANARY").unwrap();
        drop(snap);
        let mut src = OpenOptions::new().write(true).open(&prepared.disk).unwrap();
        src.seek(SeekFrom::Start(600)).unwrap();
        src.write_all(b"CHANGED").unwrap();
        fs::write(prepared.disk.parent().unwrap().join(NVRAM), b"mutated").unwrap();
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

        // Deleting the snapshot, or leaving a broken copy, must not keep the old files.
        fs::remove_dir_all(dir.join("snapshots").join(MACHINE)).unwrap();
        let recopied = save(&dir).unwrap();
        let mut recopied_buf = [0u8; 7];
        let mut recopied_disk = File::open(&recopied.disk).unwrap();
        recopied_disk.seek(SeekFrom::Start(600)).unwrap();
        recopied_disk.read_exact(&mut recopied_buf).unwrap();
        assert_eq!(&recopied_buf, b"CHANGED");
        assert_eq!(fs::read(recopied.dir.join(NVRAM)).unwrap(), b"mutated");
        assert!(fs::read_to_string(recopied.dir.join(READY))
            .unwrap()
            .starts_with("ssh-ok"));

        fs::write(recopied.dir.join(READY), b"nope").unwrap();
        fs::write(&recopied.disk, b"short").unwrap();
        let replaced = save(&dir).unwrap();
        assert_eq!(replaced.disk.metadata().unwrap().len(), GUEST);
        let mut replaced_buf = [0u8; 7];
        let mut replaced_disk = File::open(&replaced.disk).unwrap();
        replaced_disk.seek(SeekFrom::Start(600)).unwrap();
        replaced_disk.read_exact(&mut replaced_buf).unwrap();
        assert_eq!(&replaced_buf, b"CHANGED");
        assert!(fs::read_to_string(replaced.dir.join(READY))
            .unwrap()
            .starts_with("ssh-ok"));
        let _ = fs::remove_dir_all(&dir);
    }

    fn mark_installed(disk: &Path) {
        let mut file = OpenOptions::new().write(true).open(disk).unwrap();
        file.seek(SeekFrom::Start(510)).unwrap();
        file.write_all(&[0x55, 0xaa]).unwrap();
        file.sync_all().unwrap();
    }
}
