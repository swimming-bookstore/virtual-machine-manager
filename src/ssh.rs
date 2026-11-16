//! Host key for the guest. The install is finished when this key can log in.

use std::collections::HashSet;
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Result};

use crate::preseed::{AUTHORIZED, USER};

const PRIVATE: &str = "id_ed25519";
const PUBLIC: &str = "id_ed25519.pub";
const PROOF: &str = "ssh-ok";
const SCAN: &str = "scan";
const SSH_PORT: u16 = 22;

/// Create the host key once. The public half is what the installer copies.
pub fn ensure(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|err| anyhow!("cannot create {}: {err}", dir.display()))?;
    let private = dir.join(PRIVATE);
    let public = dir.join(PUBLIC);
    if !private.is_file() || !public.is_file() {
        generate(&private)?;
    }
    let public_line = fs::read_to_string(&public)
        .map_err(|err| anyhow!("cannot read {}: {err}", public.display()))?;
    if public_line.trim().is_empty() {
        bail!("SSH public key is empty");
    }
    let authorized = dir.join(AUTHORIZED);
    if fs::read_to_string(&authorized).ok().as_deref() != Some(public_line.as_str()) {
        write_new(&authorized, public_line.as_bytes())?;
    }
    Ok(())
}

/// True only after a login with the host key has been recorded.
/// The proof may sit in `ssh/` or beside the disk that was reached.
pub fn logged_in(dir: &Path) -> Result<bool> {
    proof_ok(&dir.join(PROOF))
}

pub fn logged_in_beside(disk: &Path) -> Result<bool> {
    let Some(guest) = disk.parent() else {
        return Ok(false);
    };
    if proof_ok(&guest.join(PROOF))? {
        return Ok(true);
    }
    let Some(support) = guest.parent().and_then(|dir| dir.parent()) else {
        return Ok(false);
    };
    proof_ok(&support.join("ssh").join(PROOF))
}

fn proof_ok(proof: &Path) -> Result<bool> {
    if !proof.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(proof).map_err(|err| anyhow!("{}: {err}", proof.display()))?;
    Ok(text.starts_with(USER) && text.contains("ok"))
}

/// Same login, stored next to the disk so a later open can tell this disk from a new empty one.
pub fn record_login_beside(disk: &Path) -> Result<()> {
    let Some(guest) = disk.parent() else {
        bail!("guest disk has no directory");
    };
    fs::create_dir_all(guest)?;
    write_new(&guest.join(PROOF), format!("{USER} ok\n").as_bytes())?;
    Ok(())
}

/// Own process group so closing Quick start can kill this scan and the ssh it started.
pub fn own_group() {
    #[cfg(unix)]
    {
        extern "C" {
            fn setpgid(pid: i32, pgid: i32) -> i32;
        }
        unsafe {
            let _ = setpgid(0, 0);
        }
    }
}

/// One attempt. A closed port or a refused key is not a finished install.
/// ARP first, then the next slice of 192.168.64.2–254. 192.168.64.1 is the host gateway, not the guest.
/// DHCP is not limited to .2–.8, so one probe must not stop there, and it must not walk the whole subnet.
pub fn probe_disk(disk: &Path) -> Result<bool> {
    if logged_in_beside(disk)? {
        return Ok(true);
    }
    let Some(guest) = disk.parent() else {
        return Ok(false);
    };
    let Some(support) = guest.parent().and_then(|dir| dir.parent()) else {
        return Ok(false);
    };
    let dir = support.join("ssh");
    let private = dir.join(PRIVATE);
    if !private.is_file() {
        return Ok(false);
    }
    let mac = mac_string(disk);
    for host in next_hosts(&dir, &mac, &arp_guests()) {
        if login(&private, &host)? {
            write_new(&guest.join(PROOF), format!("{USER} ok\n").as_bytes())?;
            write_new(&dir.join(PROOF), format!("{USER} ok\n").as_bytes())?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Locally administered MAC, stable for this disk, so the DHCP lease can be found again.
/// Same bytes the guest attachment uses.
pub fn mac_string(disk: &Path) -> String {
    let mut sum: u16 = 0x564d;
    for byte in disk.as_os_str().as_bytes() {
        sum = sum.wrapping_mul(33).wrapping_add(u16::from(*byte));
    }
    format!("02:56:4d:4d:{:02x}:{:02x}", (sum >> 8) as u8, sum as u8)
}

fn next_hosts(dir: &Path, mac: &str, arp: &[(String, String)]) -> Vec<String> {
    let want = normalize_mac(mac);
    let mut batch = Vec::new();
    for (host, seen) in arp {
        if seen == &want {
            batch.push(host.clone());
        }
    }
    let start = read_octet(dir);
    let next = 2 + (start - 2 + 12) % 253;
    let _ = fs::create_dir_all(dir);
    let _ = fs::write(dir.join(SCAN), format!("{next}\n"));
    for offset in 0..12 {
        let n = 2 + (start - 2 + offset) % 253;
        batch.push(format!("192.168.64.{n}"));
    }
    let mut seen = HashSet::new();
    batch
        .into_iter()
        .filter(|host| seen.insert(host.clone()))
        .collect()
}

fn read_octet(dir: &Path) -> u32 {
    fs::read_to_string(dir.join(SCAN))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .filter(|n| (2..=254).contains(n))
        .unwrap_or(2)
}

fn arp_guests() -> Vec<(String, String)> {
    parse_arp(&arp_output())
}

/// The NAT address for this disk's MAC. ARP first, then `/var/db/dhcpd_leases`.
/// Nothing, if the guest is off or still booting.
pub fn guest_address(disk: &Path) -> Option<String> {
    let want = assigned_mac(disk).unwrap_or_else(|| normalize_mac(&mac_string(disk)));
    if want.is_empty() {
        return None;
    }
    address_for_mac(&want, &arp_guests()).or_else(|| lease_address(&want, &lease_text()))
}

fn assigned_mac(disk: &Path) -> Option<String> {
    let path = disk.parent()?.join("mac");
    let text = fs::read_to_string(path).ok()?;
    let mac = normalize_mac(text.trim());
    if mac.is_empty() { None } else { Some(mac) }
}

fn lease_text() -> String {
    for path in [
        "/var/db/dhcpd_leases",
        "/private/var/db/dhcpd_leases",
        "/var/db/vmnet/dhcpd_leases",
    ] {
        if let Ok(text) = fs::read_to_string(path) {
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

fn lease_address(want: &str, text: &str) -> Option<String> {
    parse_leases(text)
        .into_iter()
        .rev()
        .find(|(_, mac)| mac == want)
        .map(|(host, _)| host)
}

/// bootpd writes `key=value` blocks. `hw_address` is `1,aa:bb:cc:dd:ee:ff`, not two words.
fn parse_leases(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut ip = None;
    let mut mac = None;
    let finish = |ip: &mut Option<String>, mac: &mut Option<String>, found: &mut Vec<(String, String)>| {
        if let (Some(host), Some(raw)) = (ip.take(), mac.take()) {
            let normalized = normalize_mac(&raw);
            if !normalized.is_empty() && is_guest_host(&host) {
                found.push((host, normalized));
            }
        } else {
            ip.take();
            mac.take();
        }
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line == "{" {
            ip = None;
            mac = None;
            continue;
        }
        if line == "}" {
            finish(&mut ip, &mut mac, &mut found);
            continue;
        }
        let Some((key, value)) = lease_field(line) else {
            continue;
        };
        if key == "ip_address" || key == "ip-address" {
            ip = Some(value);
        } else if key == "hw_address"
            || key == "hw-address"
            || key == "mac_address"
            || key == "hardware_address"
        {
            mac = Some(value);
        } else if key == "identifier" && mac.is_none() {
            mac = Some(value);
        }
    }
    finish(&mut ip, &mut mac, &mut found);
    found
}

fn lease_field(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_matches(|c| c == '"' || c == ';');
    if let Some((key, value)) = trimmed.split_once('=') {
        let key = key.trim().trim_matches('"');
        let value = value.trim().trim_matches(|c| c == '"' || c == ';');
        if key.is_empty() || value.is_empty() {
            return None;
        }
        return Some((key.to_string(), value.to_string()));
    }
    let mut parts = trimmed.split_whitespace();
    let key = parts.next()?.trim_matches('"');
    let value = parts.collect::<Vec<_>>().join(" ");
    let value = value.trim_matches(|c| c == '"' || c == ';');
    if key.is_empty() || value.is_empty() {
        return None;
    }
    Some((key.to_string(), value.to_string()))
}

fn is_guest_host(host: &str) -> bool {
    host.starts_with("192.168.64.") && host != "192.168.64.1" && host.split('.').count() == 4
}

fn address_for(disk: &Path, arp: &[(String, String)]) -> Option<String> {
    let want = assigned_mac(disk).unwrap_or_else(|| normalize_mac(&mac_string(disk)));
    address_for_mac(&want, arp)
}

fn address_for_mac(want: &str, arp: &[(String, String)]) -> Option<String> {
    if want.is_empty() {
        return None;
    }
    arp.iter()
        .find(|(_, seen)| seen == want)
        .map(|(host, _)| host.clone())
}

fn arp_output() -> String {
    for bin in ["/usr/sbin/arp", "/usr/bin/arp", "arp"] {
        if let Ok(out) = Command::new(bin)
            .arg("-an")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        {
            return String::from_utf8_lossy(&out.stdout).into_owned();
        }
    }
    String::new()
}

fn parse_arp(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for line in text.lines() {
        let Some(open) = line.find('(') else { continue };
        let Some(close) = line[open..].find(')').map(|i| open + i) else {
            continue;
        };
        if open >= close {
            continue;
        }
        let host = &line[open + 1..close];
        if !host.starts_with("192.168.64.") || host == "192.168.64.1" {
            continue;
        }
        let mac = line
            .split_whitespace()
            .find(|part| part.split(':').count() == 6)
            .map(normalize_mac)
            .unwrap_or_default();
        found.push((host.to_string(), mac));
    }
    found
}

fn normalize_mac(text: &str) -> String {
    let raw = text.trim();
    let body = raw.rsplit(',').next().unwrap_or(raw).trim();
    let token = body.split_whitespace().next().unwrap_or(body);
    let parts: Vec<&str> = token.split(|c| c == ':' || c == '-').collect();
    if parts.len() != 6 {
        return String::new();
    }
    let mut bytes = Vec::with_capacity(6);
    for part in parts {
        let Ok(value) = u8::from_str_radix(part, 16) else {
            return String::new();
        };
        bytes.push(format!("{value:02x}"));
    }
    bytes.join(":")
}

fn generate(private: &Path) -> Result<()> {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(private)
        .stdin(Stdio::null())
        .status()
        .map_err(|err| anyhow!("ssh-keygen: {err}"))?;
    if !status.success() {
        bail!("ssh-keygen failed");
    }
    Ok(())
}

fn login(private: &Path, host: &str) -> Result<bool> {
    let port = SSH_PORT.to_string();
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "ConnectTimeout=1",
        "-p",
        &port,
        "-i",
    ])
    .arg(private)
    .args(["-l", USER, host, "true"])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    match cmd.status() {
        Ok(status) => Ok(status.success()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn write_new(dest: &Path, body: &[u8]) -> Result<()> {
    let tmp = dest.with_extension("partial");
    fs::write(&tmp, body)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, dest)?;
    Ok(())
}

/// Test double. A real guest is not required to prove the snapshot waits for a login.
#[cfg(test)]
pub fn record_login(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    write_new(&dir.join(PROOF), format!("{USER} ok\n").as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn mac_matches_the_guest_attachment() {
        let disk = Path::new("/tmp/vmm/snapshots/debian/disk.img");
        assert_eq!(mac_string(disk), "02:56:4d:4d:09:35");
    }

    #[test]
    fn arp_keeps_guest_addresses() {
        let text = "\
? (192.168.64.1) at aa:bb:cc:dd:ee:ff on bridge100
? (192.168.64.7) at 2:56:4d:4d:9:35 on bridge100
? (10.0.0.2) at 02:56:4d:4d:09:35 on en0
";
        assert_eq!(
            parse_arp(text),
            vec![("192.168.64.7".into(), "02:56:4d:4d:09:35".into())]
        );
        let disk = Path::new("/tmp/vmm/snapshots/debian/disk.img");
        assert_eq!(
            address_for(disk, &parse_arp(text)),
            Some("192.168.64.7".into())
        );
        assert_eq!(address_for(disk, &[]), None);
    }

    #[test]
    fn lease_file_is_key_equals_value() {
        let text = "{\n\tname=debian\n\tip_address=192.168.64.5\n\thw_address=1,2:56:4d:4d:9:35\n\tidentifier=1,2:56:4d:4d:9:35\n\tlease=0x1\n}\n{\n\tip_address=192.168.64.1\n\thw_address=1,aa:bb:cc:dd:ee:ff\n}\n";
        let mac = "02:56:4d:4d:09:35";
        assert_eq!(lease_address(mac, text), Some("192.168.64.5".into()));
        assert_eq!(lease_address("02:00:00:00:00:01", text), None);
        assert_eq!(
            parse_leases("ip_address=10.0.0.2\nhw_address=1,02:56:4d:4d:09:35\n"),
            vec![]
        );
    }

    #[test]
    fn a_probe_walks_twelve_addresses_and_continues() {
        let dir = PathBuf::from(format!("/tmp/vmm-ssh-{}-{}", std::process::id(), "scan"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mac = "02:56:4d:4d:09:35";
        let arp = vec![("192.168.64.7".into(), mac.into())];
        let first = next_hosts(&dir, mac, &arp);
        assert_eq!(first[0], "192.168.64.7");
        let mut want = vec!["192.168.64.7".into()];
        want.extend(
            (2..14)
                .filter(|n| *n != 7)
                .map(|n| format!("192.168.64.{n}")),
        );
        assert_eq!(first, want);
        let second = next_hosts(&dir, mac, &[]);
        assert_eq!(second[0], "192.168.64.14");
        assert_eq!(second.len(), 12);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_is_true_only_after_a_recorded_login() {
        let root = PathBuf::from(format!("/tmp/vmm-ssh-probe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let disk = root.join("snapshots/debian/disk.img");
        fs::create_dir_all(disk.parent().unwrap()).unwrap();
        fs::write(&disk, b"disk").unwrap();
        assert!(!probe_disk(&disk).unwrap());
        record_login(&root.join("ssh")).unwrap();
        assert!(probe_disk(&disk).unwrap());
        let _ = fs::remove_dir_all(&root);
    }
}
