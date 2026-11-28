//! Guest disk size. The image is created at 16 GiB. Settings lengthens or shortens
//! that sparse file while the virtual machine is stopped. It does not change the
//! filesystem inside the image. A shorter file must still cover every partition.
//! A GPT backup header is moved to the new end so the guest sees the free space.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{anyhow, bail, Result};

use crate::preseed::{DISK, GUEST};

const GIB: u64 = 1024 * 1024 * 1024;
const SECTOR: u64 = 512;
/// Settings offers this much. A larger image is still a hole-filled file.
pub const MAX_GIB: u64 = 512;
/// The install image. A guest disk is never shorter than this.
pub const MIN_GIB: u64 = GUEST / GIB;

#[derive(Debug)]
pub struct DiskSize {
    pub bytes: u64,
    pub gib: u64,
    /// Smallest size Settings can offer. Never below the install image or a partition.
    pub min_gib: u64,
    /// Largest size Settings can offer.
    pub max_gib: u64,
}

/// Length of the image, rounded down to a GiB. A missing file is not a disk.
pub fn load(disk: &Path) -> Result<DiskSize> {
    if !disk.is_file() {
        bail!("guest disk is missing");
    }
    let bytes = disk.metadata()?.len();
    if bytes < GUEST {
        bail!("guest disk is smaller than the install image");
    }
    Ok(DiskSize {
        bytes,
        gib: bytes / GIB,
        min_gib: floor(disk, bytes)?,
        max_gib: MAX_GIB,
    })
}

/// Lengthen or shorten `disk.img`. The new size must cover every partition.
/// A file that is already this size is left as it is.
pub fn resize(disk: &Path, gib: u64) -> Result<DiskSize> {
    let current = load(disk)?;
    if gib > MAX_GIB {
        bail!("disk size must be at most {MAX_GIB} GB");
    }
    if gib < MIN_GIB {
        bail!("disk size must be at least {MIN_GIB} GB");
    }
    if !looks_installed(disk)? {
        bail!("the guest disk has no partition table");
    }
    let bytes = gib * GIB;
    let used = minimum_bytes(disk)?;
    if bytes < used {
        bail!("the partitions use {} GB", used.div_ceil(GIB).max(1));
    }
    if bytes != current.bytes {
        apply_len(disk, current.bytes, bytes)?;
    }
    Ok(DiskSize {
        bytes,
        gib,
        min_gib: current.min_gib,
        max_gib: MAX_GIB,
    })
}

/// Grow first so the new end exists. Shrink only after the backup header is in place.
fn apply_len(disk: &Path, old: u64, bytes: u64) -> Result<()> {
    if bytes > old {
        set_len(disk, bytes)?;
        if let Err(err) = fit_gpt(disk, bytes) {
            let _ = set_len(disk, old);
            return Err(err);
        }
    } else {
        fit_gpt(disk, bytes)?;
        set_len(disk, bytes)?;
    }
    let file = OpenOptions::new().write(true).open(disk)?;
    file.sync_all()?;
    Ok(())
}

fn set_len(disk: &Path, bytes: u64) -> Result<()> {
    let file = OpenOptions::new().write(true).open(disk)?;
    file.set_len(bytes)
        .map_err(|err| anyhow!("cannot resize {}: {err}", disk.display()))?;
    Ok(())
}

fn floor(disk: &Path, bytes: u64) -> Result<u64> {
    let used = if looks_installed(disk)? {
        minimum_bytes(disk)?
    } else {
        GUEST
    };
    Ok(used.div_ceil(GIB).max(MIN_GIB).min(bytes / GIB).min(MAX_GIB))
}

/// First byte the file must still contain. Partitions, plus the GPT backup when there is one.
fn minimum_bytes(disk: &Path) -> Result<u64> {
    let end = partition_end(disk)?;
    Ok(end.saturating_add(gpt_tail(disk)?).max(GUEST))
}

fn partition_end(disk: &Path) -> Result<u64> {
    let table = read_table(disk)?;
    let mut end = 0u64;
    if table.is_gpt() {
        end = end.max(gpt_partition_end(disk)?);
    }
    for entry in table.mbr_entries() {
        end = end.max(entry.end_byte()?);
    }
    Ok(end)
}

/// Sectors the backup header and its entry array need past the last partition.
fn gpt_tail(disk: &Path) -> Result<u64> {
    let table = read_table(disk)?;
    if !table.is_gpt() {
        return Ok(0);
    }
    let header = read_gpt_header(disk)?;
    Ok(header.entry_sectors().saturating_add(1) * SECTOR)
}

fn fit_gpt(disk: &Path, bytes: u64) -> Result<()> {
    let table = read_table(disk)?;
    if !table.is_gpt() {
        return Ok(());
    }
    let header = read_gpt_header(disk)?;
    let last_lba = bytes / SECTOR - 1;
    let entry_sectors = header.entry_sectors();
    if last_lba <= entry_sectors + 1 {
        bail!("partition table is invalid");
    }
    let backup_entries = last_lba - entry_sectors;
    let last_usable = backup_entries - 1;
    if last_usable < header.first_usable {
        bail!("the partitions use {} GB", bytes.div_ceil(GIB).max(1));
    }
    let entries = read_entries(disk, &header)?;
    let part_last = gpt_last_lba(&entries, &header)?;
    if part_last > last_usable {
        bail!(
            "the partitions use {} GB",
            minimum_bytes(disk)?.div_ceil(GIB).max(1)
        );
    }
    let entry_crc = crc32(&entries);
    write_backup(disk, &header, &entries, last_lba, backup_entries, last_usable, entry_crc)?;
    write_primary(disk, &header, last_lba, last_usable, entry_crc)?;
    Ok(())
}

fn write_backup(
    disk: &Path,
    header: &GptHeader,
    entries: &[u8],
    last_lba: u64,
    backup_entries: u64,
    last_usable: u64,
    entry_crc: u32,
) -> Result<()> {
    let mut sector = header.sector;
    put_u64(&mut sector[24..], last_lba);
    put_u64(&mut sector[32..], header.my_lba);
    put_u64(&mut sector[48..], last_usable);
    put_u64(&mut sector[72..], backup_entries);
    put_u32(&mut sector[88..], entry_crc);
    seal_header(&mut sector, header.header_size)?;
    let mut file = OpenOptions::new().write(true).open(disk)?;
    file.seek(SeekFrom::Start(backup_entries * SECTOR))?;
    file.write_all(entries)?;
    file.seek(SeekFrom::Start(last_lba * SECTOR))?;
    file.write_all(&sector)?;
    Ok(())
}

fn write_primary(
    disk: &Path,
    header: &GptHeader,
    last_lba: u64,
    last_usable: u64,
    entry_crc: u32,
) -> Result<()> {
    let mut sector = header.sector;
    put_u64(&mut sector[32..], last_lba);
    put_u64(&mut sector[48..], last_usable);
    put_u32(&mut sector[88..], entry_crc);
    seal_header(&mut sector, header.header_size)?;
    let mut file = OpenOptions::new().write(true).open(disk)?;
    file.seek(SeekFrom::Start(header.my_lba * SECTOR))?;
    file.write_all(&sector)?;
    Ok(())
}

fn seal_header(sector: &mut [u8], header_size: usize) -> Result<()> {
    if header_size > sector.len() || header_size < 92 {
        bail!("partition table is invalid");
    }
    sector[16..20].fill(0);
    let sum = crc32(&sector[..header_size]);
    put_u32(&mut sector[16..], sum);
    Ok(())
}

struct GptHeader {
    sector: [u8; 512],
    header_size: usize,
    my_lba: u64,
    first_usable: u64,
    entry_lba: u64,
    count: u64,
    entry_size: u64,
}

impl GptHeader {
    fn entry_sectors(&self) -> u64 {
        self.count
            .saturating_mul(self.entry_size)
            .div_ceil(SECTOR)
    }
}

fn read_gpt_header(disk: &Path) -> Result<GptHeader> {
    let mut file = File::open(disk)?;
    let mut sector = [0u8; 512];
    file.seek(SeekFrom::Start(SECTOR))?;
    file.read_exact(&mut sector)?;
    if &sector[0..8] != b"EFI PART" {
        bail!("the guest disk has no partition table");
    }
    let header_size = u32::from_le_bytes(sector[12..16].try_into().unwrap()) as usize;
    if !(92..=512).contains(&header_size) {
        bail!("partition table is invalid");
    }
    let count = u32::from_le_bytes(sector[80..84].try_into().unwrap()) as u64;
    let entry_size = u32::from_le_bytes(sector[84..88].try_into().unwrap()) as u64;
    if count == 0 || count > 128 || entry_size < 128 || entry_size > 4096 {
        bail!("partition table is invalid");
    }
    Ok(GptHeader {
        my_lba: u64::from_le_bytes(sector[24..32].try_into().unwrap()),
        first_usable: u64::from_le_bytes(sector[40..48].try_into().unwrap()),
        entry_lba: u64::from_le_bytes(sector[72..80].try_into().unwrap()),
        count,
        entry_size,
        header_size,
        sector,
    })
}

fn read_entries(disk: &Path, header: &GptHeader) -> Result<Vec<u8>> {
    let bytes = header
        .count
        .checked_mul(header.entry_size)
        .ok_or_else(|| anyhow!("partition table is invalid"))?;
    let at = header
        .entry_lba
        .checked_mul(SECTOR)
        .ok_or_else(|| anyhow!("partition table is invalid"))?;
    let mut file = File::open(disk)?;
    let mut entries = vec![0u8; bytes as usize];
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(&mut entries)?;
    Ok(entries)
}

fn gpt_partition_end(disk: &Path) -> Result<u64> {
    let header = read_gpt_header(disk)?;
    let entries = read_entries(disk, &header)?;
    let last = gpt_last_lba(&entries, &header)?;
    Ok(last.saturating_add(1).saturating_mul(SECTOR))
}

fn gpt_last_lba(entries: &[u8], header: &GptHeader) -> Result<u64> {
    let size = header.entry_size as usize;
    let mut last = 0u64;
    for index in 0..header.count as usize {
        let at = index * size;
        let entry = entries
            .get(at..at + size)
            .ok_or_else(|| anyhow!("partition table is invalid"))?;
        if entry[..16].iter().all(|byte| *byte == 0) {
            continue;
        }
        let end = u64::from_le_bytes(entry[40..48].try_into().unwrap());
        last = last.max(end);
    }
    Ok(last)
}

struct Table {
    sector: [u8; 512],
}

impl Table {
    fn is_gpt(&self) -> bool {
        self.mbr_entries().any(|entry| entry.kind == 0xee)
    }

    fn mbr_entries(&self) -> impl Iterator<Item = MbrEntry> + '_ {
        (0..4).map(|index| {
            let at = 446 + index * 16;
            MbrEntry {
                kind: self.sector[at + 4],
                start: u32::from_le_bytes(self.sector[at + 8..at + 12].try_into().unwrap()),
                sectors: u32::from_le_bytes(self.sector[at + 12..at + 16].try_into().unwrap()),
            }
        })
    }
}

struct MbrEntry {
    kind: u8,
    start: u32,
    sectors: u32,
}

impl MbrEntry {
    fn end_byte(&self) -> Result<u64> {
        if self.kind == 0 || self.sectors == 0 || self.kind == 0xee {
            return Ok(0);
        }
        u64::from(self.start)
            .checked_add(u64::from(self.sectors))
            .and_then(|sector| sector.checked_mul(SECTOR))
            .ok_or_else(|| anyhow!("partition table is invalid"))
    }
}

fn read_table(disk: &Path) -> Result<Table> {
    let mut file = File::open(disk)?;
    let mut sector = [0u8; 512];
    file.read_exact(&mut sector)?;
    if sector[510] != 0x55 || sector[511] != 0xaa {
        bail!("the guest disk has no partition table");
    }
    Ok(Table { sector })
}

fn looks_installed(disk: &Path) -> Result<bool> {
    Ok(read_table(disk).is_ok())
}

fn put_u32(buf: &mut [u8], value: u32) {
    buf[..4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(buf: &mut [u8], value: u64) {
    buf[..8].copy_from_slice(&value.to_le_bytes());
}

/// ISO 3309 CRC-32. The GPT header is summed with its CRC field set to zero.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn resize_lengthens_and_shrinks_a_sparse_image() {
        let dir = std::env::temp_dir().join(format!("vmm-disk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let disk = dir.join(DISK);
        assert!(load(&disk).is_err());
        let file = File::create(&disk).unwrap();
        file.set_len(GUEST).unwrap();
        drop(file);
        assert!(resize(&disk, 32).unwrap_err().to_string().contains("partition"));
        mark_installed(&disk);
        let grown = resize(&disk, 32).unwrap();
        assert_eq!(grown.gib, 32);
        assert_eq!(grown.min_gib, 16);
        assert_eq!(grown.max_gib, MAX_GIB);
        assert_eq!(disk.metadata().unwrap().len(), 32 * GIB);
        assert!(disk.metadata().unwrap().blocks() < 256);
        let shrunk = resize(&disk, 16).unwrap();
        assert_eq!(shrunk.gib, 16);
        assert_eq!(disk.metadata().unwrap().len(), 16 * GIB);
        assert!(resize(&disk, 8).is_err());
        assert_eq!(resize(&disk, 16).unwrap().bytes, 16 * GIB);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resize_refuses_to_cut_a_partition() {
        let dir = std::env::temp_dir().join(format!("vmm-disk-cut-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let disk = dir.join(DISK);
        let file = File::create(&disk).unwrap();
        file.set_len(32 * GIB).unwrap();
        drop(file);
        mark_mbr_partition(&disk, 20 * GIB);
        assert!(resize(&disk, 16).unwrap_err().to_string().contains("partitions use"));
        assert_eq!(disk.metadata().unwrap().len(), 32 * GIB);
        assert_eq!(load(&disk).unwrap().min_gib, 20);
        assert_eq!(resize(&disk, 20).unwrap().gib, 20);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resize_moves_the_gpt_backup_to_the_new_end() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let dir = std::env::temp_dir().join(format!("vmm-disk-gpt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let disk = dir.join(DISK);
        let file = File::create(&disk).unwrap();
        file.set_len(16 * GIB).unwrap();
        drop(file);
        write_gpt(&disk, 16 * GIB, 8 * GIB);
        let grown = resize(&disk, 32).unwrap();
        assert_eq!(grown.gib, 32);
        assert_eq!(grown.min_gib, 16);
        assert_gpt(&disk, 32 * GIB, 8 * GIB);
        assert!(disk.metadata().unwrap().blocks() < 256);
        let shrunk = resize(&disk, 16).unwrap();
        assert_eq!(shrunk.gib, 16);
        assert_gpt(&disk, 16 * GIB, 8 * GIB);
        let _ = fs::remove_dir_all(&dir);
    }

    fn mark_installed(disk: &Path) {
        let mut file = OpenOptions::new().write(true).open(disk).unwrap();
        file.seek(SeekFrom::Start(510)).unwrap();
        file.write_all(&[0x55, 0xaa]).unwrap();
    }

    fn mark_mbr_partition(disk: &Path, end: u64) {
        let mut sector = [0u8; 512];
        sector[446 + 4] = 0x83;
        let start = 2048u32;
        let sectors = ((end / SECTOR) - u64::from(start)) as u32;
        sector[446 + 8..446 + 12].copy_from_slice(&start.to_le_bytes());
        sector[446 + 12..446 + 16].copy_from_slice(&sectors.to_le_bytes());
        sector[510] = 0x55;
        sector[511] = 0xaa;
        let mut file = OpenOptions::new().write(true).open(disk).unwrap();
        file.write_all(&sector).unwrap();
    }

    fn write_gpt(disk: &Path, disk_bytes: u64, part_end: u64) {
        let last_lba = disk_bytes / SECTOR - 1;
        let mut entries = vec![0u8; 128 * 128];
        entries[0] = 1;
        put_u64(&mut entries[32..], 2048);
        put_u64(&mut entries[40..], part_end / SECTOR - 1);
        let entry_crc = crc32(&entries);
        let mut header = [0u8; 512];
        header[0..8].copy_from_slice(b"EFI PART");
        put_u32(&mut header[8..], 0x0001_0000);
        put_u32(&mut header[12..], 92);
        put_u64(&mut header[24..], 1);
        put_u64(&mut header[32..], last_lba);
        put_u64(&mut header[40..], 34);
        put_u64(&mut header[48..], last_lba - 33);
        put_u64(&mut header[72..], 2);
        put_u32(&mut header[80..], 128);
        put_u32(&mut header[84..], 128);
        put_u32(&mut header[88..], entry_crc);
        seal_header(&mut header, 92).unwrap();
        let mut mbr = [0u8; 512];
        mbr[446 + 4] = 0xee;
        mbr[510] = 0x55;
        mbr[511] = 0xaa;
        let mut file = OpenOptions::new().write(true).open(disk).unwrap();
        file.write_all(&mbr).unwrap();
        file.seek(SeekFrom::Start(SECTOR)).unwrap();
        file.write_all(&header).unwrap();
        file.seek(SeekFrom::Start(2 * SECTOR)).unwrap();
        file.write_all(&entries).unwrap();
    }

    fn assert_gpt(disk: &Path, disk_bytes: u64, part_end: u64) {
        let last_lba = disk_bytes / SECTOR - 1;
        let header = read_gpt_header(disk).unwrap();
        assert_eq!(header.my_lba, 1);
        let alternate = u64::from_le_bytes(header.sector[32..40].try_into().unwrap());
        let last_usable = u64::from_le_bytes(header.sector[48..56].try_into().unwrap());
        assert_eq!(alternate, last_lba);
        assert_eq!(last_usable, last_lba - 33);
        assert!(part_end / SECTOR - 1 <= last_usable);
        let mut check = header.sector;
        check[16..20].fill(0);
        assert_eq!(
            crc32(&check[..header.header_size]),
            u32::from_le_bytes(header.sector[16..20].try_into().unwrap())
        );
        let mut backup = [0u8; 8];
        let mut file = File::open(disk).unwrap();
        file.seek(SeekFrom::Start(last_lba * SECTOR)).unwrap();
        file.read_exact(&mut backup).unwrap();
        assert_eq!(&backup, b"EFI PART");
    }
}
