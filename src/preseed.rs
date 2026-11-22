//! Preseed for the Debian netinst, applied the way the installation guide describes.
//! <https://www.debian.org/releases/stable/amd64/apbs02.en.html>
//! The signed ISO is kept. A boot copy gets two in-place edits. The GRUB menu firmware
//! actually reads is `/boot/grub/grub.cfg` (the EFI stub only sources it). That menu
//! starts the text installer with the guide's auto parameters and does not wait for a
//! key. `preseed.cfg` is appended to that initrd. The graphical initrd does not see it.
//! Virtualization.framework has no CD drive, so the boot copy is a USB disk.
//!
//! `preseed.cfg` is the official example, with the local answers appended.
//! <https://www.debian.org/releases/trixie/example-preseed.txt>
//!
//! When the target has a Debian version and a GRUB config, late_command copies
//! `/authorized_keys` from the text initrd into the debian account and enables
//! sshd. The host treats the install as finished only after it can log in.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use flate2::write::GzEncoder;
use flate2::Compression;

pub const FILE: &str = "preseed.img";
pub const LABEL: &str = "PRESEED";
pub const BOOT: &str = "boot.iso";
/// Not the download cache. The signed ISO stays in cache/.
pub const DIR: &str = "installer";

const SECTOR: u64 = 2048;

pub const MACHINE: &str = "debian";
pub const DISK: &str = "disk.img";
/// The wizard is building this snapshot. It is not a machine yet, so it does not live under machines/.
pub const SNAPSHOT: &str = "snapshots/debian";
pub const GUEST: u64 = 16 * 1024 * 1024 * 1024;

pub const USER: &str = "debian";
/// Written into the guest so the host can log in with this key.
pub const AUTHORIZED: &str = "authorized_keys";

/// Official example. Downloaded, not kept in the binary.
/// <https://www.debian.org/releases/trixie/example-preseed.txt>
pub const EXAMPLE: &str = "example-preseed.txt";
pub const EXAMPLE_URL: &str = "https://www.debian.org/releases/trixie/example-preseed.txt";

/// The downloaded example, then the local answers. The installer reads this as `/preseed.cfg`.
pub fn preseed_cfg(example: &str) -> Result<String> {
    if !example.contains("#_preseed_V1") || !example.contains("d-i ") {
        bail!("{EXAMPLE} is not a Debian preseed");
    }
    let mut text = example.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&answers());
    Ok(text)
}

/// Overrides loaded after the official example. The example already sets the mirror
/// layout, partitioning confirms, and GRUB. Locale and keymap are also kernel
/// parameters, because the installer asks them before this file is read.
///
/// `priority=critical` does not hide this question. The guide's `keymap=skip-config`
/// is not one of the choices in trixie, so the installer still asks. xfce-desktop
/// then installs keyboard-configuration in the target, and that package asks
/// "Please select the layout matching the keyboard for this machine" at critical
/// priority. It skips the question when the variant is already seen, or when
/// `/etc/default/keyboard` names a layout it does not know. The installer's own
/// keyboard setup overwrites that file, so the hook writes it again in the target
/// and marks the question seen before the package configures.
/// The guest disk is virtio, and the installer is a USB disk, so the disk has to be
/// named or partman may pick the USB stick.
pub fn answers() -> String {
    format!(
        r#"d-i debian-installer/locale string en_US.UTF-8
d-i keyboard-configuration/xkb-keymap select us
d-i debian-installer/keymap string us
keyboard-configuration keyboard-configuration/layout select English (US)
keyboard-configuration keyboard-configuration/variant select English (US)
keyboard-configuration keyboard-configuration/model select Generic 105-key PC
keyboard-configuration keyboard-configuration/layoutcode string us
keyboard-configuration keyboard-configuration/variantcode string
keyboard-configuration keyboard-configuration/modelcode string pc105
keyboard-configuration keyboard-configuration/optionscode string
keyboard-configuration keyboard-configuration/layout seen true
keyboard-configuration keyboard-configuration/variant seen true
keyboard-configuration keyboard-configuration/model seen true
d-i console-setup/ask_detect boolean false
d-i preseed/early_command string {early}
d-i mirror/http/hostname string deb.debian.org
d-i passwd/root-login boolean false
d-i passwd/user-fullname string debian
d-i passwd/user-password password debian
d-i passwd/user-password-again password debian
d-i passwd/username string debian
d-i partman-auto/disk string /dev/vda
d-i grub-installer/bootdev string /dev/vda
d-i preseed/late_command string {late}

tasksel tasksel/first multiselect \
  ssh-server, \
  standard, \
  xfce-desktop, \
  ,

popularity-contest popularity-contest/participate boolean false
"#,
        early = early_command(),
        late = late_command(),
    )
}

/// Writes the target keyboard file and marks the layout question seen. The
/// installer's keyboard setup replaces `/etc/default/keyboard` after early_command,
/// so the hook writes the target file itself. `skip` is not a real variant: the
/// package treats that layout as unsupported and does not ask. The seen flags
/// cover the case where that file is replaced by a normal US layout before the
/// package configures. The hook is one shell line. A `#` in this line is a
/// debconf comment, so the script has no shebang.
fn early_command() -> String {
    "mkdir -p /etc/default /usr/lib/pre-pkgsel.d /usr/lib/base-installer.d && printf '%s\\n' XKBMODEL=pc105 XKBLAYOUT=us XKBVARIANT=skip XKBOPTIONS= BACKSPACE=guess > /etc/default/keyboard && printf '%s\\n' 'mkdir -p /target/etc/default' 'printf '\\''%s\\n'\\'' XKBMODEL=pc105 XKBLAYOUT=us XKBVARIANT=skip XKBOPTIONS= BACKSPACE=guess > /target/etc/default/keyboard' 'printf '\\''%s\\n'\\'' '\\''keyboard-configuration keyboard-configuration/layout select English (US)'\\'' '\\''keyboard-configuration keyboard-configuration/variant select English (US)'\\'' '\\''keyboard-configuration keyboard-configuration/model select Generic 105-key PC'\\'' '\\''keyboard-configuration keyboard-configuration/layoutcode string us'\\'' '\\''keyboard-configuration keyboard-configuration/variantcode string '\\'' '\\''keyboard-configuration keyboard-configuration/modelcode string pc105'\\'' '\\''keyboard-configuration keyboard-configuration/optionscode string '\\'' '\\''keyboard-configuration keyboard-configuration/layout seen true'\\'' '\\''keyboard-configuration keyboard-configuration/variant seen true'\\'' '\\''keyboard-configuration keyboard-configuration/model seen true'\\'' | chroot /target debconf-set-selections' > /usr/lib/pre-pkgsel.d/10keyboard && cp /usr/lib/pre-pkgsel.d/10keyboard /usr/lib/base-installer.d/15keyboard && chmod 755 /usr/lib/pre-pkgsel.d/10keyboard /usr/lib/base-installer.d/15keyboard".to_string()
}

/// Runs in the installer, with `/target` still mounted. A non-zero exit does not
/// install the host key, so the installer stays up and the host does not boot the disk.
/// Success powers off. The host logs in only after that disk boots.
/// No `#`: debconf treats that as a comment.
fn late_command() -> String {
    let home = format!("/target/home/{USER}");
    format!(
        "mkdir -p /target/etc/default && printf '%s\\n' XKBMODEL=pc105 XKBLAYOUT=us XKBVARIANT= XKBOPTIONS= BACKSPACE=guess > /target/etc/default/keyboard && test -s /target/etc/debian_version && test -e /target/boot/grub/grub.cfg && mkdir -p {home}/.ssh && cp /authorized_keys {home}/.ssh/authorized_keys && chmod 700 {home}/.ssh && chmod 600 {home}/.ssh/authorized_keys && chroot /target chown -R {USER}:{USER} /home/{USER}/.ssh && chroot /target systemctl enable ssh && sync && poweroff"
    )
}

/// OpenSSH public key line. The matching private key stays on the host.
pub fn authorized_keys(public_key: &str) -> String {
    let key = public_key.trim();
    if key.is_empty() {
        String::new()
    } else {
        format!("{key}\n")
    }
}

/// Boot parameters from the guide's auto mode. They belong before `---`.
/// Locale and keymap are kernel parameters. `priority=critical` still asks
/// critical questions, so it does not answer the target layout prompt.
pub fn boot_args() -> String {
    "auto=true priority=critical locale=en_US.UTF-8 keymap=us".to_string()
}

pub struct Prepared {
    pub disk: PathBuf,
    pub key: PathBuf,
}

/// Writes the answer disk. The empty guest disk and the installer copy are later steps.
/// Those files are not downloads, so they stay out of the cache, in installer/.
pub fn write_preseed(support: &Path) -> Result<()> {
    let key = read_key(&support.join("ssh").join(AUTHORIZED))?;
    let _ = write_image(
        &installer_dir(support)?.join(FILE),
        &image(&key, &read_example(support)?)?,
    )?;
    Ok(())
}

fn installer_dir(support: &Path) -> Result<PathBuf> {
    let dir = support.join(DIR);
    fs::create_dir_all(&dir).map_err(|err| anyhow!("cannot create {}: {err}", dir.display()))?;
    Ok(dir)
}

/// Creates the empty guest disk when it is missing. An existing file is left as it is.
/// The bool is true when this call wrote the file.
/// A missing disk is a new install. The previous login does not apply to the empty file written here.
/// The file is snapshots/debian/disk.img. machines/ is only for a clone made after that snapshot exists.
pub fn write_guest_disk(support: &Path) -> Result<(PathBuf, bool)> {
    let guest = support.join(SNAPSHOT);
    fs::create_dir_all(&guest)
        .map_err(|err| anyhow!("cannot create {}: {err}", guest.display()))?;
    let disk = guest.join(DISK);
    if disk.is_file() {
        return Ok((disk, false));
    }
    forget_login(support, &guest);
    sparse(&disk, GUEST)?;
    Ok((disk, true))
}

/// Copies the installer and applies the boot edit. The signed file is not rewritten.
/// The bool is true when this call rewrote the copy. The copy is not a download cache file.
pub fn prepare_installer(iso: &Path, support: &Path) -> Result<bool> {
    if !iso.is_file() {
        bail!("installer image is missing: {}", iso.display());
    }
    let key = read_key(&support.join("ssh").join(AUTHORIZED))?;
    let example = read_example(support)?;
    write_boot(iso, &installer_dir(support)?.join(BOOT), &key, &example)
}

fn read_example(support: &Path) -> Result<String> {
    let path = support.join("cache").join(EXAMPLE);
    let text = fs::read_to_string(&path)
        .map_err(|err| anyhow!("cannot read {}: {err}", path.display()))?;
    preseed_cfg(&text)?;
    Ok(text)
}

/// Writes the answer disk, the empty guest disk, and the boot ISO.
/// The bool is true when this call rewrote the installer copy. A copy that is already patched is false.
#[cfg(test)]
pub fn prepare(iso: &Path, support: &Path) -> Result<(Prepared, bool)> {
    if !iso.is_file() {
        bail!("installer image is missing: {}", iso.display());
    }
    write_preseed(support)?;
    let (disk, _) = write_guest_disk(support)?;
    let patched = prepare_installer(iso, support)?;
    Ok((
        Prepared {
            disk,
            key: support.join("ssh").join(AUTHORIZED),
        },
        patched,
    ))
}

/// `ssh/ssh-ok` is one file for the app. Deleting `disk.img` does not remove it.
fn forget_login(support: &Path, guest: &Path) {
    let _ = fs::remove_file(support.join("ssh").join("ssh-ok"));
    let _ = fs::remove_file(guest.join("ssh-ok"));
}

fn read_key(path: &Path) -> Result<String> {
    if !path.is_file() {
        bail!("SSH key is missing: {}", path.display());
    }
    let text =
        fs::read_to_string(path).map_err(|err| anyhow!("cannot read {}: {err}", path.display()))?;
    if text.trim().is_empty() {
        bail!("SSH key is empty: {}", path.display());
    }
    Ok(text)
}

fn write_image(dest: &Path, body: &[u8]) -> Result<PathBuf> {
    let tmp = dest.with_extension("partial");
    let mut file = File::create(&tmp)?;
    file.write_all(body)?;
    file.sync_all()?;
    fs::rename(&tmp, dest)?;
    Ok(dest.to_path_buf())
}

fn write_boot(iso: &Path, dest: &Path, public_key: &str, example: &str) -> Result<bool> {
    if already_patched(dest, public_key, example) {
        return Ok(false);
    }
    let tmp = dest.with_extension("partial");
    fs::copy(iso, &tmp).map_err(|err| anyhow!("cannot copy installer: {err}"))?;
    if is_eltorito(&tmp)? {
        patch_installer(&tmp, public_key, example)?;
    }
    fs::rename(&tmp, dest)?;
    Ok(true)
}

fn already_patched(dest: &Path, public_key: &str, example: &str) -> bool {
    let Ok(mut file) = File::open(dest) else {
        return false;
    };
    let Ok(true) = is_eltorito(dest) else {
        return dest.is_file();
    };
    let Ok(install) = install_dir(&mut file) else {
        return false;
    };
    let Ok(hits) = grub_cfgs(&mut file) else {
        return false;
    };
    let Some(hit) = hits.iter().max_by_key(|hit| hit.size) else {
        return false;
    };
    let mut body = vec![0u8; hit.size as usize];
    // A menu-only copy is stale once the answers or the SSH key change.
    read_at(&mut file, hit.extent as u64 * SECTOR, &mut body).is_ok()
        && menu_ready(&body, &install)
        && initrd_has_preseed(&mut file, &install, public_key, example)
}

fn initrd_has_preseed(file: &mut File, install: &str, public_key: &str, example: &str) -> bool {
    let Ok(extra) = preseed_gzip(public_key, example) else {
        return false;
    };
    let Ok(hits) = walk_install(file, install) else {
        return false;
    };
    let Some(initrd) = hits
        .iter()
        .find(|hit| hit.name.eq_ignore_ascii_case("initrd.gz"))
    else {
        return false;
    };
    if initrd.size > 256 * 1024 * 1024 {
        return false;
    }
    let mut body = vec![0u8; initrd.size as usize];
    read_at(file, initrd.extent as u64 * SECTOR, &mut body).is_ok()
        && body
            .windows(extra.len())
            .any(|word| word == extra.as_slice())
}

fn is_eltorito(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() < 18 * SECTOR {
        return Ok(false);
    }
    let mut boot = [0u8; 32];
    read_at(&mut file, 17 * SECTOR, &mut boot)?;
    Ok(&boot[7..30] == b"EL TORITO SPECIFICATION")
}

/// The GRUB menu is `/boot/grub/grub.cfg`. The EFI stub only sources that file.
/// Appending the preseed to the text initrd is the initrd method in the guide.
fn patch_installer(path: &Path, public_key: &str, example: &str) -> Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let install = install_dir(&mut file)?;
    append_preseed(&mut file, &install, public_key, example)?;
    patch_menu(&mut file, &install)?;
    file.sync_all()?;
    Ok(())
}

fn install_dir(file: &mut File) -> Result<String> {
    let mut found = Vec::new();
    for hit in root_entries(file)? {
        let name = hit.name.to_ascii_lowercase();
        if hit.is_dir && (name == "install.amd" || name == "install.a64") {
            found.push(name);
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => bail!("installer has no install.amd or install.a64 directory"),
        _ => bail!("installer has more than one architecture"),
    }
}

fn patch_menu(file: &mut File, install: &str) -> Result<()> {
    let hits = grub_cfgs(file)?;
    let Some(hit) = hits.iter().max_by_key(|hit| hit.size) else {
        bail!("installer has no GRUB menu to start automatically");
    };
    if hit.size > 256 * 1024 {
        bail!("GRUB menu is unexpectedly large");
    }
    let mut body = vec![0u8; hit.size as usize];
    read_at(file, hit.extent as u64 * SECTOR, &mut body)?;
    if menu_ready(&body, install) {
        return Ok(());
    }
    let next = fit(
        menu_body(&body, install)?,
        hit.size as usize + sector_slack(hit.size) as usize,
    )?;
    write_at(file, hit.extent as u64 * SECTOR, &next)?;
    if next.len() < body.len() {
        write_at(
            file,
            hit.extent as u64 * SECTOR + next.len() as u64,
            &vec![0u8; body.len() - next.len()],
        )?;
    }
    write_size(file, &hits, hit.extent, hit.size, next.len() as u32, None)
}

fn named(file: &mut File, extent: u32, size: u32, joliet: bool, want: &str, dir: bool) -> Result<Option<Hit>> {
    Ok(read_dir(file, extent, size, joliet)?.into_iter().find(|hit| {
        hit.is_dir == dir && hit.name.eq_ignore_ascii_case(want)
    }))
}

fn grub_cfgs(file: &mut File) -> Result<Vec<Hit>> {
    let mut hits = Vec::new();
    for joliet in [false, true] {
        let Ok((extent, size, is_joliet)) = root_of(file, joliet) else {
            continue;
        };
        let Some(boot) = named(file, extent, size, is_joliet, "boot", true)? else {
            continue;
        };
        let Some(grub) = named(file, boot.extent, boot.size, is_joliet, "grub", true)? else {
            continue;
        };
        if let Some(cfg) = named(file, grub.extent, grub.size, is_joliet, "grub.cfg", false)? {
            hits.push(cfg);
        }
    }
    Ok(hits)
}

fn menu_ready(body: &[u8], install: &str) -> bool {
    let Some(entry) = text_entry(body, install) else {
        return false;
    };
    let default = format!("set default={}\n", entry.index);
    entry
        .line
        .windows(boot_args().len())
        .any(|w| w == boot_args().as_bytes())
        && body.windows(default.len()).any(|w| w == default.as_bytes())
        && body
            .windows(b"set timeout=0\n".len())
            .any(|w| w == b"set timeout=0\n")
}

fn menu_body(body: &[u8], install: &str) -> Result<Vec<u8>> {
    let Some(entry) = text_entry(body, install) else {
        bail!("installer GRUB menu has no text install entry");
    };
    let mut out = format!("set default={}\nset timeout=0\n", entry.index).into_bytes();
    out.extend(&body[..entry.at]);
    out.extend(insert_args(entry.line, &boot_args()));
    out.extend(&body[entry.at + entry.line.len()..]);
    Ok(out)
}

struct TextEntry<'a> {
    index: usize,
    at: usize,
    line: &'a [u8],
}

/// Top-level text install entry. A submenu counts as one entry, so the index is GRUB's default.
fn text_entry<'a>(body: &'a [u8], install: &str) -> Option<TextEntry<'a>> {
    let text = format!("/{install}/initrd.gz");
    let gtk = format!("/{install}/gtk/");
    let mut index = 0;
    let mut i = 0;
    while i < body.len() {
        let (rel, submenu) = next_entry(&body[i..])?;
        let at = i + rel;
        let end = at + block_end(&body[at..]).unwrap_or(body.len() - at);
        if !submenu {
            let block = &body[at..end];
            let text_initrd = block.windows(text.len()).any(|w| w == text.as_bytes())
                && !block.windows(gtk.len()).any(|w| w == gtk.as_bytes());
            if text_initrd {
                let (off, line) = linux_line(block)?;
                return Some(TextEntry {
                    index,
                    at: at + off,
                    line,
                });
            }
        }
        index += 1;
        i = end;
    }
    None
}

fn next_entry(buf: &[u8]) -> Option<(usize, bool)> {
    let menu = buf.windows(9).position(|w| w == b"menuentry");
    let sub = buf.windows(7).position(|w| w == b"submenu");
    match (menu, sub) {
        (Some(menu), Some(sub)) if sub < menu => Some((sub, true)),
        (Some(menu), _) => Some((menu, false)),
        (None, Some(sub)) => Some((sub, true)),
        _ => None,
    }
}

fn block_end(block: &[u8]) -> Option<usize> {
    let mut depth = 0;
    let mut started = false;
    for (i, byte) in block.iter().enumerate() {
        if *byte == b'{' {
            depth += 1;
            started = true;
        } else if *byte == b'}' {
            depth -= 1;
            if started && depth == 0 {
                return Some(i + 1);
            }
        }
    }
    None
}

fn linux_line(block: &[u8]) -> Option<(usize, &[u8])> {
    let mut i = 0;
    while i < block.len() {
        let end = block[i..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|n| i + n + 1)
            .unwrap_or(block.len());
        let line = &block[i..end];
        let start = line
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .unwrap_or(0);
        if line[start..].starts_with(b"linux ") || line[start..].starts_with(b"linux\t") {
            return Some((i, line));
        }
        i = end;
    }
    None
}

fn insert_args(line: &[u8], args: &str) -> Vec<u8> {
    if line.windows(args.len()).any(|word| word == args.as_bytes()) {
        return line.to_vec();
    }
    if let Some(at) = line.windows(3).position(|word| word == b"---") {
        let mut out = line[..at].to_vec();
        if !out.ends_with(b" ") {
            out.push(b' ');
        }
        out.extend(args.as_bytes());
        out.push(b' ');
        out.extend(&line[at..]);
        return out;
    }
    let mut out = line.to_vec();
    let nl = out.ends_with(b"\n");
    if nl {
        out.pop();
    }
    if !out.ends_with(b" ") {
        out.push(b' ');
    }
    out.extend(args.as_bytes());
    if nl {
        out.push(b'\n');
    }
    out
}

/// The menu must stay inside its sector. Indent spaces are padding, so they can go.
fn fit(mut body: Vec<u8>, limit: usize) -> Result<Vec<u8>> {
    while body.len() > limit {
        let Some(at) = body.windows(2).position(|word| word == b"\n ") else {
            bail!("boot line does not fit in the GRUB menu");
        };
        body.remove(at + 1);
    }
    Ok(body)
}

struct Hit {
    at: u64,
    extent: u32,
    size: u32,
    name: String,
    is_dir: bool,
}

/// The official example does not fit in the sector slack, so the initrd is copied to the
/// end of the image and both directory records are pointed at that copy.
fn append_preseed(file: &mut File, install: &str, public_key: &str, example: &str) -> Result<()> {
    let hits = walk_install(file, install)?;
    let Some(initrd) = hits
        .iter()
        .find(|hit| hit.name.eq_ignore_ascii_case("initrd.gz"))
    else {
        bail!("installer has no text initrd to preseed");
    };
    let old = initrd.size;
    let extent = initrd.extent;
    if old > 256 * 1024 * 1024 {
        bail!("installer initrd is unexpectedly large");
    }
    let extra = preseed_gzip(public_key, example)?;
    if (extra.len() as u32) <= sector_slack(old) {
        write_at(file, extent as u64 * SECTOR + old as u64, &extra)?;
        return write_size(
            file,
            &hits,
            extent,
            old,
            old + extra.len() as u32,
            Some("initrd.gz"),
        );
    }
    let mut body = vec![0u8; old as usize];
    read_at(file, extent as u64 * SECTOR, &mut body)?;
    body.extend_from_slice(&extra);
    let new_extent = append_extent(file, body.len() as u64)?;
    write_at(file, new_extent as u64 * SECTOR, &body)?;
    let new_size = u32::try_from(body.len()).map_err(|_| anyhow!("preseed initrd is too large"))?;
    for hit in &hits {
        if hit.extent == extent && hit.size == old && hit.name.eq_ignore_ascii_case("initrd.gz") {
            write_u32_both(file, hit.at + 2, new_extent)?;
            write_u32_both(file, hit.at + 10, new_size)?;
        }
    }
    Ok(())
}

fn append_extent(file: &mut File, bytes: u64) -> Result<u32> {
    let old_len = file.metadata()?.len();
    let start = old_len.div_ceil(SECTOR) * SECTOR;
    let extent = u32::try_from(start / SECTOR)
        .map_err(|_| anyhow!("installer image is too large to extend"))?;
    let end = start + bytes;
    file.set_len(end.div_ceil(SECTOR) * SECTOR)
        .map_err(|err| anyhow!("cannot extend installer image: {err}"))?;
    extend_hybrid_mbr(file, old_len)?;
    let mut sector = [0u8; SECTOR as usize];
    let mut n = 16u64;
    loop {
        if read_at(file, n * SECTOR, &mut sector).is_err() || &sector[1..6] != b"CD001" {
            break;
        }
        if sector[0] == 1 || sector[0] == 2 {
            let old = u32::from_le_bytes(sector[80..84].try_into().unwrap());
            let sectors = u32::try_from(file.metadata()?.len() / SECTOR)
                .map_err(|_| anyhow!("installer image is too large to extend"))?;
            if sectors > old {
                write_u32_both(file, n * SECTOR + 80, sectors)?;
            }
        }
        if sector[0] == 255 || n > 32 {
            break;
        }
        n += 1;
    }
    Ok(extent)
}

/// The boot copy is a USB disk. Grow the hybrid partition that covers the whole file.
fn extend_hybrid_mbr(file: &mut File, old_len: u64) -> Result<()> {
    let new_len = file.metadata()?.len();
    if new_len % 512 != 0 || new_len < 512 {
        return Ok(());
    }
    let mut mbr = [0u8; 512];
    read_at(file, 0, &mut mbr)?;
    if mbr[510] != 0x55 || mbr[511] != 0xaa {
        return Ok(());
    }
    let lba = u32::from_le_bytes(mbr[454..458].try_into().unwrap());
    let count = u32::from_le_bytes(mbr[458..462].try_into().unwrap());
    if lba != 0 || count as u64 * 512 != old_len {
        return Ok(());
    }
    let new_count =
        u32::try_from(new_len / 512).map_err(|_| anyhow!("installer image is too large"))?;
    mbr[458..462].copy_from_slice(&new_count.to_le_bytes());
    write_at(file, 0, &mbr)
}

fn sector_slack(size: u32) -> u32 {
    let used = size % SECTOR as u32;
    if used == 0 {
        0
    } else {
        SECTOR as u32 - used
    }
}

fn preseed_gzip(public_key: &str, example: &str) -> Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::new(9));
    enc.write_all(&cpio_preseed(public_key, example)?)?;
    enc.finish()
        .map_err(|err| anyhow!("cannot compress preseed: {err}"))
}

fn cpio_preseed(public_key: &str, example: &str) -> Result<Vec<u8>> {
    let body = preseed_cfg(example)?;
    let mut archive = newc("preseed.cfg", body.as_bytes());
    archive.extend(newc(AUTHORIZED, authorized_keys(public_key).as_bytes()));
    archive.extend(newc("TRAILER!!!", b""));
    Ok(archive)
}

fn newc(name: &str, body: &[u8]) -> Vec<u8> {
    let name = name.as_bytes();
    let namesize = name.len() + 1;
    let header = format!(
        "070701{ino:08X}{mode:08X}{uid:08X}{gid:08X}{nlink:08X}{mtime:08X}{size:08X}{dev:08X}{dev:08X}{rdev:08X}{rdev:08X}{namesize:08X}{check:08X}",
        ino = 1,
        mode = 0o100644,
        uid = 0,
        gid = 0,
        nlink = 1,
        mtime = 0,
        size = body.len(),
        dev = 0,
        rdev = 0,
        check = 0,
    );
    let mut out = header.into_bytes();
    out.extend(name);
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out.extend(body);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

fn root_entries(file: &mut File) -> Result<Vec<Hit>> {
    let (extent, size, joliet) = root_of(file, false)?;
    read_dir(file, extent, size, joliet)
}

fn walk_install(file: &mut File, install: &str) -> Result<Vec<Hit>> {
    let mut hits = Vec::new();
    for joliet in [false, true] {
        let Ok((extent, size, is_joliet)) = root_of(file, joliet) else {
            continue;
        };
        if let Some(entry) = named(file, extent, size, is_joliet, install, true)? {
            hits.extend(read_dir(file, entry.extent, entry.size, is_joliet)?);
        }
    }
    Ok(hits)
}

fn root_of(file: &mut File, joliet: bool) -> Result<(u32, u32, bool)> {
    let mut sector = [0u8; SECTOR as usize];
    let mut n = 16u64;
    loop {
        read_at(file, n * SECTOR, &mut sector)?;
        if &sector[1..6] != b"CD001" {
            bail!("installer image has no ISO directory");
        }
        if sector[0] == 255 {
            bail!(
                "installer image has no {} directory",
                if joliet { "Joliet" } else { "ISO" }
            );
        }
        let is_joliet = sector[0] == 2 && &sector[88..91] == b"%/E";
        if (!joliet && sector[0] == 1) || (joliet && is_joliet) {
            let extent = u32_at(&sector, 156 + 2);
            let size = u32_at(&sector, 156 + 10);
            return Ok((extent, size, joliet));
        }
        n += 1;
        if n > 32 {
            bail!("installer image has no volume descriptor");
        }
    }
}

fn read_dir(file: &mut File, extent: u32, size: u32, joliet: bool) -> Result<Vec<Hit>> {
    if size > 1024 * 1024 {
        bail!("installer directory is unexpectedly large");
    }
    let mut data = vec![0u8; size as usize];
    let base = extent as u64 * SECTOR;
    read_at(file, base, &mut data)?;
    let mut hits = Vec::new();
    let mut i = 0;
    while i + 33 < data.len() {
        let len = data[i] as usize;
        if len == 0 {
            i += 1;
            continue;
        }
        if i + len > data.len() {
            break;
        }
        let nlen = data[i + 32] as usize;
        if i + 33 + nlen > data.len() {
            break;
        }
        let name = decode_name(&data[i + 33..i + 33 + nlen], joliet);
        if name != "\u{0}" && name != "\u{1}" {
            hits.push(Hit {
                at: base + i as u64,
                extent: u32_at(&data, i + 2),
                size: u32_at(&data, i + 10),
                is_dir: data[i + 25] & 0x02 != 0,
                name,
            });
        }
        i += len;
    }
    Ok(hits)
}

fn decode_name(raw: &[u8], joliet: bool) -> String {
    if joliet && raw.len() >= 2 && raw.len().is_multiple_of(2) {
        let units: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
            .collect();
        return String::from_utf16_lossy(&units)
            .split(';')
            .next()
            .unwrap_or("")
            .to_string();
    }
    let text = raw.iter().map(|b| char::from(*b)).collect::<String>();
    text.split(';').next().unwrap_or("").to_string()
}

fn read_at(file: &mut File, at: u64, buf: &mut [u8]) -> Result<()> {
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(buf).map_err(|err| anyhow!("installer image is truncated: {err}"))
}

fn write_at(file: &mut File, at: u64, buf: &[u8]) -> Result<()> {
    file.seek(SeekFrom::Start(at))?;
    file.write_all(buf)?;
    Ok(())
}

fn write_size(file: &mut File, hits: &[Hit], extent: u32, old: u32, new_size: u32, name: Option<&str>) -> Result<()> {
    for hit in hits {
        if hit.extent == extent && hit.size == old && name.is_none_or(|want| hit.name.eq_ignore_ascii_case(want)) {
            write_u32_both(file, hit.at + 10, new_size)?;
        }
    }
    Ok(())
}

fn write_u32_both(file: &mut File, at: u64, n: u32) -> Result<()> {
    let mut size = [0u8; 8];
    size[..4].copy_from_slice(&n.to_le_bytes());
    size[4..].copy_from_slice(&n.to_be_bytes());
    write_at(file, at, &size)
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
}

/// A hole-filled file. The installer grows it; the host does not write 16 GiB of zeros.
fn sparse(dest: &Path, bytes: u64) -> Result<()> {
    let tmp = dest.with_extension("partial");
    let file = File::create(&tmp)?;
    file.set_len(bytes)
        .map_err(|err| anyhow!("cannot size {}: {err}", tmp.display()))?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn image(public_key: &str, example: &str) -> Result<Vec<u8>> {
    let _ = public_key;
    let text = preseed_cfg(example)?;
    let clusters = text.len().div_ceil(512).max(1);
    let mut disk = vec![0u8; 512 * (3 + clusters)];
    write_fat12(&mut disk, LABEL, "preseed.cfg", text.as_bytes())?;
    Ok(disk)
}

/// One-sector FAT12. Directory entries are 8.3 names, so nested paths are short directories.
fn write_fat12(disk: &mut [u8], label: &str, path: &str, body: &[u8]) -> Result<()> {
    let bps = 512usize;
    disk[0] = 0xeb;
    disk[1] = 0x3c;
    disk[2] = 0x90;
    disk[3..11].copy_from_slice(b"MSWIN4.1");
    put_u16(&mut disk[11..], bps as u16);
    disk[13] = 1;
    put_u16(&mut disk[14..], 1);
    disk[16] = 1;
    put_u16(&mut disk[17..], 16);
    let sectors = (disk.len() / bps) as u16;
    put_u16(&mut disk[19..], sectors);
    disk[21] = 0xf8;
    put_u16(&mut disk[22..], 1);
    put_u16(&mut disk[24..], 32);
    put_u16(&mut disk[26..], 2);
    disk[36] = 0x80;
    disk[38] = 0x29;
    disk[39..43].copy_from_slice(&0x5630_4d4du32.to_le_bytes());
    let mut vol = [b' '; 11];
    for (i, b) in label.bytes().take(11).enumerate() {
        vol[i] = b.to_ascii_uppercase();
    }
    disk[43..54].copy_from_slice(&vol);
    disk[54..62].copy_from_slice(b"FAT12   ");
    disk[510] = 0x55;
    disk[511] = 0xaa;

    let fat = bps;
    disk[fat] = 0xf8;
    disk[fat + 1] = 0xff;
    disk[fat + 2] = 0xff;
    let root = fat + bps;
    let data = root + 512;

    let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let file = parts
        .pop()
        .ok_or_else(|| anyhow!("preseed path is empty"))?;
    let mut dir = root;
    let mut cluster = 2u16;
    for name in parts {
        let slot = free_slot(disk, dir)?;
        write_name(&mut disk[slot..slot + 11], name, true)?;
        disk[slot + 11] = 0x10;
        put_u16(&mut disk[slot + 26..], cluster);
        chain(disk, fat, cluster, 0x0fff)?;
        dir = data + (cluster as usize - 2) * bps;
        disk[dir] = b'.';
        disk[dir + 11] = 0x10;
        put_u16(&mut disk[dir + 26..], cluster);
        disk[dir + 32] = b'.';
        disk[dir + 33] = b'.';
        disk[dir + 32 + 11] = 0x10;
        put_u16(
            &mut disk[dir + 32 + 26..],
            if cluster == 2 { 0 } else { cluster },
        );
        cluster += 1;
    }
    let slot = free_slot(disk, dir)?;
    write_name(&mut disk[slot..slot + 11], file, false)?;
    disk[slot + 11] = 0x20;
    put_u16(&mut disk[slot + 26..], cluster);
    put_u32(&mut disk[slot + 28..], body.len() as u32);
    let mut left = body;
    if left.is_empty() {
        chain(disk, fat, cluster, 0x0fff)?;
        return Ok(());
    }
    while !left.is_empty() {
        let file_at = data + (cluster as usize - 2) * bps;
        if file_at >= disk.len() {
            bail!("preseed does not fit");
        }
        let n = left.len().min(bps).min(disk.len() - file_at);
        disk[file_at..file_at + n].copy_from_slice(&left[..n]);
        left = &left[n..];
        let current = cluster;
        cluster += 1;
        if left.is_empty() {
            chain(disk, fat, current, 0x0fff)?;
        } else {
            chain(disk, fat, current, cluster)?;
        }
    }
    Ok(())
}

fn chain(disk: &mut [u8], fat: usize, cluster: u16, value: u16) -> Result<()> {
    let offset = fat + (cluster as usize * 3) / 2;
    if offset + 2 > fat + 512 {
        bail!("FAT entry does not fit");
    }
    let raw = u16::from_le_bytes([disk[offset], disk[offset + 1]]);
    let next = if cluster.is_multiple_of(2) {
        (raw & 0xf000) | (value & 0x0fff)
    } else {
        (raw & 0x000f) | ((value & 0x0fff) << 4)
    };
    disk[offset..offset + 2].copy_from_slice(&next.to_le_bytes());
    Ok(())
}

fn free_slot(disk: &[u8], dir: usize) -> Result<usize> {
    (0..16)
        .map(|i| dir + i * 32)
        .find(|at| disk[*at] == 0)
        .ok_or_else(|| anyhow!("preseed directory is full"))
}

fn write_name(out: &mut [u8], name: &str, dir: bool) -> Result<()> {
    out.fill(b' ');
    let (base, ext) = name.split_once('.').unwrap_or((name, ""));
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || name.contains([' ', '/']) {
        bail!("preseed name {name} is not 8.3");
    }
    if dir && name.contains('.') {
        bail!("preseed directory {name} is not 8.3");
    }
    for (i, b) in base.bytes().enumerate() {
        out[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.bytes().enumerate() {
        out[8 + i] = b.to_ascii_uppercase();
    }
    Ok(())
}

fn put_u16(out: &mut [u8], n: u16) {
    out[..2].copy_from_slice(&n.to_le_bytes());
}

fn put_u32(out: &mut [u8], n: u32) {
    out[..4].copy_from_slice(&n.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    fn example() -> String {
        let mut text = String::from("#_preseed_V1\nd-i debian-installer/locale string en_US.UTF-8\n# ");
        let mut n = 0x12345678u32;
        for _ in 0..4000 {
            n = n.wrapping_mul(1664525).wrapping_add(1013904223);
            text.push(char::from(b'a' + (n % 26) as u8));
        }
        text.push('\n');
        text
    }

    #[test]
    fn preseed_disk_contains_the_installer_answers() {
        let disk = image(PUB, &example()).unwrap();
        let text = String::from_utf8_lossy(&disk);
        let answers = answers();
        let preseed = preseed_cfg(&example()).unwrap();
        assert!(text.contains("PRESEED"));
        assert!(disk.windows(preseed.len()).any(|w| w == preseed.as_bytes()));
        assert!(preseed.contains("#_preseed_V1"));
        assert!(preseed_cfg("not a preseed").is_err());
        assert_eq!(
            EXAMPLE_URL,
            "https://www.debian.org/releases/trixie/example-preseed.txt"
        );
        assert!(boot_args().contains("auto=true"));
        assert!(boot_args().contains("locale=en_US.UTF-8"));
        assert!(boot_args().contains("keymap=us"));
        assert!(answers.contains("keyboard-configuration/xkb-keymap select us"));
        assert!(answers.contains("keyboard-configuration/variant select English (US)"));
        assert!(answers.contains("keyboard-configuration/variant seen true"));
        assert!(answers.contains("XKBVARIANT=skip"));
        assert!(answers.contains("debconf-set-selections"));
        assert!(answers.contains("preseed/early_command"));
        assert!(answers.contains("pre-pkgsel.d/10keyboard"));
        assert!(answers.contains("base-installer.d/15keyboard"));
        assert!(!answers.contains('#'));
        assert!(answers.contains("debian-installer/locale string en_US.UTF-8"));
        assert!(answers.contains("deb.debian.org"));
        assert!(answers.contains("partman-auto/disk string /dev/vda"));
        assert!(answers.contains("grub-installer/bootdev string /dev/vda"));
        assert!(answers.contains("ssh-server"));
        assert!(answers.contains("xfce-desktop"));
        assert!(answers.contains("user-password password debian"));
        assert!(answers.contains("user-password-again password debian"));
        assert!(answers.contains("popularity-contest/participate boolean false"));
        assert!(answers.contains("cp /authorized_keys"));
        assert!(answers.contains("systemctl enable ssh && sync && poweroff"));
        assert!(!answers.contains('#'));
        assert_eq!(
            answers
                .lines()
                .filter(|line| line.contains("late_command"))
                .count(),
            1
        );
        assert!(!disk.windows(b"PRESEED/".len()).any(|w| w == b"PRESEED/"));
        let gzip = preseed_gzip(PUB, &example()).unwrap();
        assert!(gzip.len() < 16 * 1024, "preseed gzip is {} bytes", gzip.len());
        let mut plain = Vec::new();
        GzDecoder::new(gzip.as_slice())
            .read_to_end(&mut plain)
            .unwrap();
        assert!(plain.windows(PUB.len()).any(|word| word == PUB.as_bytes()));
    }

    #[test]
    fn the_boot_iso_starts_the_text_installer() {
        let dir = std::env::temp_dir().join(format!("vmm-grub-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("cache")).unwrap();
        fs::write(dir.join("cache").join(EXAMPLE), example()).unwrap();
        let iso = dir.join("cache").join("debian-13.7.0-arm64-netinst.iso");
        fs::write(&iso, installer_iso()).unwrap();
        fs::create_dir_all(dir.join("ssh")).unwrap();
        fs::write(dir.join("ssh").join(AUTHORIZED), PUB).unwrap();
        let (_, patched) = prepare(&iso, &dir).unwrap();
        assert!(patched);
        let boot = fs::read(dir.join(DIR).join(BOOT)).unwrap();
        assert_eq!(fs::read(&iso).unwrap(), installer_iso());
        let menu = &boot[29 * 2048..30 * 2048];
        let text = String::from_utf8_lossy(menu);
        assert!(text.contains("set default=1\n"), "{text}");
        assert!(text.contains("set timeout=0\n"), "{text}");
        assert!(text.contains("linux /install.a64/vmlinuz auto=true priority=critical locale=en_US.UTF-8 keymap=us --- quiet"));
        assert!(text.contains("linux /install.a64/vmlinuz --- quiet"));
        let gtk = text.find("gtk/initrd.gz").unwrap();
        assert!(!text[..gtk].contains("auto=true"));
        assert!(boot[30 * 2048..31 * 2048].iter().all(|byte| *byte == 0));
        let size = initrd_size(&boot);
        let at = u32_at(&boot, 22 * 2048 + 34 + 2) as usize * 2048;
        assert!(size > 100);
        assert!(at >= 48 * 2048, "the example does not fit in the initrd slack");
        let extra = &boot[at + 100..at + size as usize];
        let mut dec = GzDecoder::new(extra);
        let mut cpio = Vec::new();
        dec.read_to_end(&mut cpio).unwrap();
        assert!(cpio
            .windows(preseed_cfg(&example()).unwrap().len())
            .any(|w| w == preseed_cfg(&example()).unwrap().as_bytes()));
        assert!(cpio.windows(PUB.len()).any(|w| w == PUB.as_bytes()));
        let (_, again) = prepare(&iso, &dir).unwrap();
        assert!(!again);
        assert_eq!(fs::read(dir.join(DIR).join(BOOT)).unwrap(), boot);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_menu_picks_the_text_entry_on_either_arch() {
        let amd64 = b"\
menuentry 'Graphical install' {
    linux    /install.amd/vmlinuz vga=788 --- quiet
    initrd   /install.amd/gtk/initrd.gz
}
menuentry 'Install' {
    linux    /install.amd/vmlinuz vga=788 --- quiet
    initrd   /install.amd/initrd.gz
}
submenu 'Advanced options' {
    menuentry '... Automated install' {
        linux    /install.amd/vmlinuz auto=true priority=critical locale=en_US.UTF-8 keymap=us vga=788 --- quiet
        initrd   /install.amd/initrd.gz
    }
}
";
        let patched = menu_body(amd64, "install.amd").unwrap();
        assert!(patched.starts_with(b"set default=1\nset timeout=0\n"));
        assert!(patched
            .windows(b"vmlinuz vga=788 auto=true priority=critical locale=en_US.UTF-8 keymap=us --- quiet".len())
            .any(|line| line == b"vmlinuz vga=788 auto=true priority=critical locale=en_US.UTF-8 keymap=us --- quiet"));
        let arm64 = b"\
menuentry 'Install' {
    linux /install.a64/vmlinuz --- quiet
    initrd /install.a64/initrd.gz
}
menuentry 'Graphical install' {
    linux /install.a64/vmlinuz --- quiet
    initrd /install.a64/gtk/initrd.gz
}
";
        assert!(menu_body(arm64, "install.a64")
            .unwrap()
            .starts_with(b"set default=0\nset timeout=0\n"));
    }

    #[test]
    fn the_guest_disk_is_sparse() {
        let dir = std::env::temp_dir().join(format!("vmm-preseed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("cache")).unwrap();
        fs::write(dir.join("cache").join(EXAMPLE), example()).unwrap();
        let iso = dir.join("cache").join("debian.iso");
        fs::write(&iso, b"iso").unwrap();
        fs::create_dir_all(dir.join("ssh")).unwrap();
        fs::write(dir.join("ssh").join(AUTHORIZED), PUB).unwrap();
        let (prepared, patched) = prepare(&iso, &dir).unwrap();
        assert!(patched);
        assert_eq!(prepared.disk, dir.join(SNAPSHOT).join(DISK));
        assert!(
            !dir.join("machines").exists(),
            "the install disk is not a machine"
        );
        assert_eq!(prepared.disk.metadata().unwrap().len(), GUEST);
        assert_eq!(prepared.key, dir.join("ssh").join(AUTHORIZED));
        assert!(dir.join(DIR).join(FILE).is_file());
        assert_eq!(fs::read(dir.join(DIR).join(BOOT)).unwrap(), b"iso");
        assert!(!dir.join(FILE).exists());
        assert!(!dir.join(BOOT).exists());
        assert!(!dir.join("cache").join(FILE).exists());
        assert!(!dir.join("cache").join(BOOT).exists());
        let (_, again) = prepare(&iso, &dir).unwrap();
        assert!(!again);
        assert_eq!(prepared.disk.metadata().unwrap().len(), GUEST);
        let _ = fs::remove_dir_all(&dir);
    }

    const PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKeyForTheInstaller debian@host";

    fn initrd_size(iso: &[u8]) -> u32 {
        u32_at(iso, 22 * 2048 + 34 + 10)
    }

    /// A tiny El Torito image. Firmware reads `/boot/grub/grub.cfg`; the first entry is graphical.
    fn installer_iso() -> Vec<u8> {
        let mut iso = vec![0u8; 48 * 2048];
        iso[24 * 2048..24 * 2048 + 100].fill(0x11);
        let menu = grub_menu();
        iso[29 * 2048..29 * 2048 + menu.len()].copy_from_slice(&menu);

        iso[16 * 2048] = 1;
        iso[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
        put_both(&mut iso[16 * 2048 + 156 + 2..], 20);
        put_both(&mut iso[16 * 2048 + 156 + 10..], 2048);

        iso[17 * 2048] = 0;
        iso[17 * 2048 + 1..17 * 2048 + 6].copy_from_slice(b"CD001");
        iso[17 * 2048 + 7..17 * 2048 + 30].copy_from_slice(b"EL TORITO SPECIFICATION");

        iso[18 * 2048] = 2;
        iso[18 * 2048 + 1..18 * 2048 + 6].copy_from_slice(b"CD001");
        iso[18 * 2048 + 88..18 * 2048 + 91].copy_from_slice(b"%/E");
        put_both(&mut iso[18 * 2048 + 156 + 2..], 21);
        put_both(&mut iso[18 * 2048 + 156 + 10..], 2048);

        let mut root = Vec::new();
        root.extend(dir_record(b"\x00", 20, 2048, true));
        root.extend(dir_record(b"INSTALL.A64", 22, 2048, true));
        root.extend(dir_record(b"BOOT", 25, 2048, true));
        iso[20 * 2048..20 * 2048 + root.len()].copy_from_slice(&root);

        let mut joliet = Vec::new();
        joliet.extend(dir_record(b"\x00", 21, 2048, true));
        joliet.extend(dir_record(&ucs2("install.a64"), 23, 2048, true));
        joliet.extend(dir_record(&ucs2("boot"), 26, 2048, true));
        iso[21 * 2048..21 * 2048 + joliet.len()].copy_from_slice(&joliet);

        let mut install = Vec::new();
        install.extend(dir_record(b"\x00", 22, 2048, true));
        install.extend(dir_record(b"INITRD.GZ;1", 24, 100, false));
        iso[22 * 2048..22 * 2048 + install.len()].copy_from_slice(&install);

        let mut jinstall = Vec::new();
        jinstall.extend(dir_record(b"\x00", 23, 2048, true));
        jinstall.extend(dir_record(&ucs2("initrd.gz"), 24, 100, false));
        iso[23 * 2048..23 * 2048 + jinstall.len()].copy_from_slice(&jinstall);

        let mut boot = Vec::new();
        boot.extend(dir_record(b"\x00", 25, 2048, true));
        boot.extend(dir_record(b"GRUB", 27, 2048, true));
        iso[25 * 2048..25 * 2048 + boot.len()].copy_from_slice(&boot);

        let mut jboot = Vec::new();
        jboot.extend(dir_record(b"\x00", 26, 2048, true));
        jboot.extend(dir_record(&ucs2("grub"), 28, 2048, true));
        iso[26 * 2048..26 * 2048 + jboot.len()].copy_from_slice(&jboot);

        let mut grub = Vec::new();
        grub.extend(dir_record(b"\x00", 27, 2048, true));
        grub.extend(dir_record(b"GRUB.CFG;1", 29, menu.len() as u32, false));
        iso[27 * 2048..27 * 2048 + grub.len()].copy_from_slice(&grub);

        let mut jgrub = Vec::new();
        jgrub.extend(dir_record(b"\x00", 28, 2048, true));
        jgrub.extend(dir_record(&ucs2("grub.cfg"), 29, menu.len() as u32, false));
        iso[28 * 2048..28 * 2048 + jgrub.len()].copy_from_slice(&jgrub);
        iso
    }

    /// Graphical is first, as on the amd64 image, and the file fills its sector down to 20 bytes.
    fn grub_menu() -> Vec<u8> {
        let mut menu = b"\
menuentry 'Graphical install' {
    linux /install.a64/vmlinuz --- quiet
    initrd /install.a64/gtk/initrd.gz
}
menuentry 'Install' {
    linux /install.a64/vmlinuz --- quiet
    initrd /install.a64/initrd.gz
}
submenu 'Advanced options' {
    menuentry '... Automated install' {
        linux /install.a64/vmlinuz auto=true priority=critical locale=en_US.UTF-8 keymap=us --- quiet
        initrd /install.a64/initrd.gz
    }
}
"
        .to_vec();
        while menu.len() < 2028 {
            menu.extend(b"\n    ");
        }
        menu.truncate(2028);
        menu
    }

    fn dir_record(name: &[u8], extent: u32, size: u32, dir: bool) -> Vec<u8> {
        let len = 33 + name.len();
        let mut rec = vec![0u8; len];
        rec[0] = len as u8;
        put_both(&mut rec[2..], extent);
        put_both(&mut rec[10..], size);
        rec[25] = if dir { 2 } else { 0 };
        rec[32] = name.len() as u8;
        rec[33..].copy_from_slice(name);
        rec
    }

    fn put_both(out: &mut [u8], n: u32) {
        out[..4].copy_from_slice(&n.to_le_bytes());
        out[4..8].copy_from_slice(&n.to_be_bytes());
    }

    fn ucs2(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
    }
}
