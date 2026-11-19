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
/// ARP first, then the next slice of the vmnet subnet. `.1` is the host gateway, not the guest.
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
    let text = ifconfig_text();
    let prefixes = nat_prefixes(&text);
    for host in next_on_bridge(&prefixes, &host_gateways(&text), &dir, &mac, &arp_guests()) {
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

#[cfg(test)]
fn next_hosts(dir: &Path, mac: &str, arp: &[(String, String)]) -> Vec<String> {
    next_on_bridge(&[[192, 168, 64]], &HashSet::new(), dir, mac, arp)
}

fn next_on_bridge(
    prefixes: &[[u8; 3]],
    gateways: &HashSet<String>,
    dir: &Path,
    mac: &str,
    arp: &[(String, String)],
) -> Vec<String> {
    let want = normalize_mac(mac);
    let mut batch = Vec::new();
    for (host, seen) in arp {
        if seen == &want && !gateways.contains(host) && !host_side(host) {
            batch.push(host.clone());
        }
    }
    let start = read_octet(dir);
    let next = 2 + (start - 2 + 12) % 253;
    let _ = fs::create_dir_all(dir);
    let _ = fs::write(dir.join(SCAN), format!("{next}\n"));
    let prefix = prefixes.first().copied().unwrap_or([192, 168, 64]);
    for offset in 0..12 {
        let n = 2 + (start - 2 + offset) % 253;
        let host = format!("{}.{}.{}.{n}", prefix[0], prefix[1], prefix[2]);
        if gateways.contains(&host) || host_side(&host) {
            continue;
        }
        batch.push(host);
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

/// Guest IP for this disk. The caller polls. Match the short MAC `arp -an` prints.
/// `02:56:4d:4d:a0:00` is `2:56:4d:4d:a0:0` in `? (192.168.64.51) at 2:56:4d:4d:a0:0 on bridge100`.
pub fn guest_address(disk: &Path, mac: Option<&str>) -> Option<String> {
    let wants = macs_for(disk, mac);
    if wants.is_empty() {
        return None;
    }
    ip_for_short_mac(&wants, &lookup_text())
}

/// Tests pass `VMM_ARP`. Live lookup reads the kernel table, `arp -an`, and bootpd leases.
fn lookup_text() -> String {
    if let Ok(text) = std::env::var("VMM_ARP") {
        return text;
    }
    let mut text = String::new();
    #[cfg(target_os = "macos")]
    {
        text.push_str(&sysctl_arp_text());
    }
    push_table(&mut text, &arp_output());
    push_table(&mut text, &lease_text_to_arp(&dhcp_leases_file()));
    text
}

fn push_table(text: &mut String, extra: &str) {
    if extra.is_empty() {
        return;
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(extra);
}

/// The IP whose short MAC equals this disk. The interface name is not part of the match.
fn ip_for_short_mac(wants: &[String], text: &str) -> Option<String> {
    let shorts: Vec<String> = wants.iter().map(|mac| short_mac(mac)).filter(|mac| !mac.is_empty()).collect();
    if shorts.is_empty() {
        return None;
    }
    let mut found = None;
    for line in text.lines() {
        let Some((host, mac, _)) = arp_row(line) else {
            continue;
        };
        if mac.is_empty() || host_side(&host) || !usable_ipv4(&host) {
            continue;
        }
        if shorts.iter().any(|want| want == &short_mac(&mac)) {
            found = Some(host.clone());
            if host.starts_with("192.168.") {
                return found;
            }
        }
    }
    found
}

/// `arp -an` drops leading zeros. `02:56:4d:4d:a0:00` and `2:56:4d:4d:a0:0` are the same NIC.
fn short_mac(mac: &str) -> String {
    normalize_mac(mac)
        .split(':')
        .filter(|part| !part.is_empty())
        .map(|octet| {
            let trimmed = octet.trim_start_matches('0');
            if trimmed.is_empty() {
                "0".to_string()
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(":")
}

fn macs_for(disk: &Path, mac: Option<&str>) -> Vec<String> {
    let mut wants = Vec::new();
    if let Some(mac) = mac.map(normalize_mac).filter(|text| !text.is_empty()) {
        wants.push(mac);
    }
    if let Some(mac) = assigned_mac(disk) {
        if !wants.contains(&mac) {
            wants.push(mac);
        }
    }
    let computed = normalize_mac(&mac_string(disk));
    if !computed.is_empty() && !wants.contains(&computed) {
        wants.push(computed);
    }
    wants
}

/// Addresses on vmnet bridges, plus the other host address on that subnet.
/// A bridge at 192.168.64.1 also owns .2. A bridge at 192.168.65.2 also owns .1.
fn host_gateways(text: &str) -> HashSet<String> {
    let mut found = HashSet::new();
    for host in bridge_addresses(text) {
        found.insert(host.clone());
        found.extend(host_pair(&host));
    }
    found
}

fn bridge_addresses(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut name = "";
    for line in text.lines() {
        if !line.starts_with('\t') && !line.starts_with(' ') {
            name = line.split(':').next().unwrap_or("");
        }
        let bridge =
            name.starts_with("bridge") || name.starts_with("vmnet") || name.starts_with("vmenet");
        if !bridge {
            continue;
        }
        let Some(inet) = line
            .split_whitespace()
            .skip_while(|word| *word != "inet")
            .nth(1)
        else {
            continue;
        };
        if (usable_ipv4(inet) || inet.split('.').count() == 4) && !found.contains(&inet.to_string())
        {
            found.push(inet.to_string());
        }
    }
    found
}

/// 192.168.64.1 and 192.168.64.2 are the same Mac. Whichever ifconfig shows, skip both.
fn host_pair(host: &str) -> Vec<String> {
    let mut parts = host.split('.');
    let Some(a) = parts.next() else {
        return Vec::new();
    };
    let Some(b) = parts.next() else {
        return Vec::new();
    };
    let Some(c) = parts.next().and_then(|part| part.parse::<u8>().ok()) else {
        return Vec::new();
    };
    let Some(d) = parts.next() else {
        return Vec::new();
    };
    if a != "192" || b != "168" || c < 64 || parts.next().is_some() || (d != "1" && d != "2") {
        return Vec::new();
    }
    vec![format!("{a}.{b}.{c}.1"), format!("{a}.{b}.{c}.2")]
}

/// vmnet shared networks are bridge interfaces. The host address on that bridge is `.1` or `.2`.
/// A miss falls back to 192.168.64, the first subnet Apple hands out.
fn nat_prefixes(text: &str) -> Vec<[u8; 3]> {
    let mut found = Vec::new();
    for inet in bridge_addresses(text) {
        let mut parts = inet.split('.');
        let Some(a) = parts.next().and_then(|part| part.parse::<u8>().ok()) else {
            continue;
        };
        let Some(b) = parts.next().and_then(|part| part.parse::<u8>().ok()) else {
            continue;
        };
        let Some(c) = parts.next().and_then(|part| part.parse::<u8>().ok()) else {
            continue;
        };
        let Some(d) = parts.next().and_then(|part| part.parse::<u8>().ok()) else {
            continue;
        };
        if a != 192 || b != 168 || c < 64 || (d != 1 && d != 2) || parts.next().is_some() {
            continue;
        }
        let prefix = [a, b, c];
        if !found.contains(&prefix) {
            found.push(prefix);
        }
    }
    if found.is_empty() {
        found.push([192, 168, 64]);
    }
    found
}

fn ifconfig_text() -> String {
    for bin in ["/sbin/ifconfig", "/usr/sbin/ifconfig", "ifconfig"] {
        if let Ok(out) = Command::new(bin)
            .arg("-a")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

fn assigned_mac(disk: &Path) -> Option<String> {
    let path = disk.parent()?.join("mac");
    let text = fs::read_to_string(path).ok()?;
    let mac = normalize_mac(text.trim());
    if mac.is_empty() {
        None
    } else {
        Some(mac)
    }
}

/// Not this Mac, loopback, or a broadcast. The subnet is whichever bridge vmnet chose.
/// `.1` is the gateway. `.2` on a vmnet shared network is this Mac too, including 192.168.64.2.
fn is_guest_host(host: &str) -> bool {
    usable_ipv4(host) && !host_side(host)
}

/// The host side of a vmnet shared network. ifconfig may show only `.1`, so `.2` is excluded here.
fn host_side(host: &str) -> bool {
    let mut parts = host.split('.');
    let Some(a) = parts.next() else {
        return false;
    };
    let Some(b) = parts.next() else {
        return false;
    };
    let Some(c) = parts.next().and_then(|part| part.parse::<u8>().ok()) else {
        return false;
    };
    let Some(d) = parts.next().and_then(|part| part.parse::<u8>().ok()) else {
        return false;
    };
    parts.next().is_none() && a == "192" && b == "168" && c >= 64 && (d == 1 || d == 2)
}

fn usable_ipv4(host: &str) -> bool {
    let mut parts = host.split('.');
    let Some(a) = parts.next().and_then(|part| part.parse::<u8>().ok()) else {
        return false;
    };
    let Some(_) = parts.next().and_then(|part| part.parse::<u8>().ok()) else {
        return false;
    };
    let Some(_) = parts.next().and_then(|part| part.parse::<u8>().ok()) else {
        return false;
    };
    let Some(d) = parts.next().and_then(|part| part.parse::<u8>().ok()) else {
        return false;
    };
    parts.next().is_none() && a != 0 && a != 127 && a < 224 && d != 0 && d != 1 && d != 255
}

fn arp_output() -> String {
    for bin in ["/usr/sbin/arp", "/usr/bin/arp", "/sbin/arp", "arp"] {
        if let Ok(out) = Command::new(bin)
            .arg("-an")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

fn dhcp_leases_file() -> String {
    for path in ["/var/db/dhcpd_leases", "/private/var/db/dhcpd_leases"] {
        if let Some(text) = fs::read_to_string(path).ok().filter(|text| !text.is_empty()) {
            return text;
        }
    }
    String::new()
}

/// bootpd writes `{ ip_address=... hw_address=1,2:56:4d:4d:a0:0 }`. Same rows as ARP.
fn lease_text_to_arp(text: &str) -> String {
    let mut out = String::new();
    let mut ip = None;
    let mut mac = None;
    for line in text.lines() {
        let line = line.trim();
        if line == "{" {
            ip = None;
            mac = None;
            continue;
        }
        if line == "}" {
            if let (Some(host), Some(hw)) = (ip.take(), mac.take()) {
                out.push_str(&format!("? ({host}) at {hw} on bridge100\n"));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("ip_address=") {
            ip = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("hw_address=") {
            mac = Some(rest.trim().to_string());
        }
    }
    out
}

/// Kernel ARP table. Spawning `arp` from the app can fail while `arp -an` in a shell still works.
#[cfg(target_os = "macos")]
fn sysctl_arp_text() -> String {
    const NET_RT_FLAGS: i32 = 2;
    const RTF_LLINFO: i32 = 0x400;
    let mut mib = [
        libc::CTL_NET,
        libc::PF_ROUTE,
        0,
        libc::AF_INET,
        NET_RT_FLAGS,
        RTF_LLINFO,
    ];
    let mut len = 0usize;
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 || len == 0 {
        return String::new();
    }
    let mut buf = vec![0u8; len];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 {
        return String::new();
    }
    buf.truncate(len);
    parse_route_arp(&buf)
}

#[cfg(target_os = "macos")]
fn parse_route_arp(buf: &[u8]) -> String {
    let hdr = std::mem::size_of::<libc::rt_msghdr>();
    let mut text = String::new();
    let mut off = 0;
    while off + hdr <= buf.len() {
        let msglen = u16::from_ne_bytes(buf[off..off + 2].try_into().unwrap()) as usize;
        if msglen < hdr || off + msglen > buf.len() {
            break;
        }
        let addrs = i32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap());
        let end = off + msglen;
        let mut p = off + hdr;
        let mut ip = None;
        let mut mac = None;
        for bit in 0..8 {
            if addrs & (1 << bit) == 0 {
                continue;
            }
            if p >= end {
                break;
            }
            let sa_len = buf[p] as usize;
            let family = if p + 1 < end { buf[p + 1] } else { 0 };
            let take = if sa_len == 0 { 8 } else { (sa_len + 7) & !7 };
            if bit == 0 && family == libc::AF_INET as u8 && p + 8 <= end {
                let addr = u32::from_be_bytes(buf[p + 4..p + 8].try_into().unwrap());
                ip = Some(format!(
                    "{}.{}.{}.{}",
                    (addr >> 24) & 0xff,
                    (addr >> 16) & 0xff,
                    (addr >> 8) & 0xff,
                    addr & 0xff
                ));
            }
            if bit == 1 && family == libc::AF_LINK as u8 && p + 8 <= end {
                let nlen = buf[p + 5] as usize;
                let alen = buf[p + 6] as usize;
                let data = p + 8 + nlen;
                if alen == 6 && data + 6 <= end {
                    let hw = &buf[data..data + 6];
                    mac = Some(format!(
                        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        hw[0], hw[1], hw[2], hw[3], hw[4], hw[5]
                    ));
                }
            }
            p = end.min(p + take);
        }
        if let (Some(host), Some(hw)) = (ip, mac) {
            text.push_str(&format!("? ({host}) at {hw} on bridge100\n"));
        }
        off += msglen;
    }
    text
}

fn parse_arp(text: &str) -> Vec<(String, String)> {
    arp_rows(text)
        .into_iter()
        .filter(|(host, _, _)| is_guest_host(host))
        .map(|(host, mac, _)| (host, mac))
        .collect()
}

/// One ARP row: IP, normalized MAC, interface. `bridge100` is the vmnet shared network.
fn arp_rows(text: &str) -> Vec<(String, String, String)> {
    let mut found = Vec::new();
    for line in text.lines() {
        let Some((host, mac, iface)) = arp_row(line) else {
            continue;
        };
        found.push((host, mac, iface));
    }
    found
}

fn arp_row(line: &str) -> Option<(String, String, String)> {
    let open = line.find('(')?;
    let close = open + line[open..].find(')')?;
    if open >= close {
        return None;
    }
    let host = line[open + 1..close].to_string();
    let mac = line
        .split_whitespace()
        .find(|part| {
            let n = part.split(|c| c == ':' || c == '-').count();
            n == 6 || n == 7
        })
        .map(normalize_mac)
        .unwrap_or_default();
    let iface = line
        .split_whitespace()
        .skip_while(|part| *part != "on")
        .nth(1)
        .unwrap_or("")
        .trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string();
    Some((host, mac, iface))
}

fn normalize_mac(text: &str) -> String {
    let raw = text.trim();
    let body = raw.rsplit(',').next().unwrap_or(raw).trim();
    let token = body.split_whitespace().next().unwrap_or(body);
    let mut parts: Vec<&str> = token.split(|c| c == ':' || c == '-').collect();
    // bootpd sometimes writes the hardware type in front of the six MAC bytes.
    if parts.len() == 7 {
        let Ok(kind) = u8::from_str_radix(parts[0], 16) else {
            return String::new();
        };
        if kind > 1 {
            return String::new();
        }
        parts.remove(0);
    }
    if parts.len() != 6 {
        return String::new();
    }
    let mut bytes = Vec::with_capacity(6);
    for part in parts {
        let Ok(value) = u8::from_str_radix(hex_octet(part), 16) else {
            return String::new();
        };
        bytes.push(format!("{value:02x}"));
    }
    bytes.join(":")
}

/// `arp -an` prints `2:56:4d:4d:9:0` for `02:56:4d:4d:09:00`, sometimes with a comma on the last octet.
fn hex_octet(part: &str) -> &str {
    part.trim_end_matches(|c: char| !c.is_ascii_hexdigit())
        .trim_start_matches(|c: char| !c.is_ascii_hexdigit())
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
            vec![
                ("192.168.64.7".into(), "02:56:4d:4d:09:35".into()),
                ("10.0.0.2".into(), "02:56:4d:4d:09:35".into()),
            ]
        );
        let mac = "02:56:4d:4d:09:35";
        assert_eq!(
            ip_for_short_mac(&[mac.into()], text),
            Some("192.168.64.7".into())
        );
        assert_eq!(ip_for_short_mac(&[mac.into()], ""), None);
        let guest = "\
? (192.168.64.1) at aa:bb:cc:dd:ee:ff on bridge100
? (192.168.64.2) at bb:bb:cc:dd:ee:ff on bridge100
? (192.168.64.51) at 02:56:4d:4d:09:35 on bridge100
";
        assert_eq!(
            ip_for_short_mac(&["02:56:4d:4d:09:35".into()], guest),
            Some("192.168.64.51".into())
        );
        assert_eq!(normalize_mac("1,01:02:56:4d:4d:09:35"), "02:56:4d:4d:09:35");
        assert_eq!(short_mac("2:56:4d:4d:a0:0"), "2:56:4d:4d:a0:0");
        assert_eq!(short_mac("02:56:4d:4d:a0:00"), "2:56:4d:4d:a0:0");
        assert_eq!(
            ip_for_short_mac(
                &["02:56:4d:4d:a0:00".into()],
                "? (192.168.64.51) at 2:56:4d:4d:a0:0 on bridge100 ifscope [bridge]\n"
            ),
            Some("192.168.64.51".into())
        );
        assert_eq!(
            ip_for_short_mac(
                &["02:56:4d:4d:09:00".into()],
                "? (192.168.64.8) at 2:56:4d:4d:9:0 on bridge100\n"
            ),
            Some("192.168.64.8".into())
        );
        let bridge100 = "? (192.168.64.51) at 2:56:4d:4d:9:35 on bridge100 ifscope [bridge]\n";
        assert_eq!(
            ip_for_short_mac(&["02:56:4d:4d:09:35".into()], bridge100),
            Some("192.168.64.51".into())
        );
        let en0 = "? (192.168.64.51) at 2:56:4d:4d:9:35 on en0 ifscope [ethernet]\n";
        assert_eq!(
            ip_for_short_mac(&["02:56:4d:4d:09:35".into()], en0),
            Some("192.168.64.51".into())
        );
        assert_eq!(
            ip_for_short_mac(
                &["02:56:4d:4d:a0:00".into()],
                "? (192.168.64.51) at 2:56:4d:4d:a0:0 on bridge100 ifscope [bridge]\n"
            ),
            Some("192.168.64.51".into())
        );
        let row = arp_row(bridge100.trim()).expect("bridge100 row");
        assert_eq!(row.0, "192.168.64.51");
        assert_eq!(row.1, "02:56:4d:4d:09:35");
        assert_eq!(row.2, "bridge100");
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
        want.extend((2..14).filter(|n| *n != 2 && *n != 7).map(|n| format!("192.168.64.{n}")));
        assert_eq!(first, want);
        assert!(!first.iter().any(|host| host.ends_with(".2")));
        let second = next_hosts(&dir, mac, &[]);
        assert_eq!(second[0], "192.168.64.14");
        assert_eq!(second.len(), 12);
        let gateways = HashSet::from(["192.168.65.2".to_string()]);
        let other = dir.join("second-subnet");
        fs::create_dir_all(&other).unwrap();
        let skipped = next_on_bridge(&[[192, 168, 65]], &gateways, &other, mac, &[]);
        assert!(!skipped.contains(&"192.168.65.2".to_string()));
        assert_eq!(skipped[0], "192.168.65.3");
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

    #[test]
    fn nat_prefixes_are_the_bridge_gateways() {
        let text = "\
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
	inet 192.168.1.23 netmask 0xffffff00 broadcast 192.168.1.255
bridge0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
	inet 192.168.1.1 netmask 0xffffff00 broadcast 192.168.1.255
bridge100: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
	inet 192.168.65.1 netmask 0xffffff00 broadcast 192.168.65.255
vmnet1: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
	inet 192.168.66.1 netmask 0xffffff00 broadcast 192.168.66.255
";
        assert_eq!(nat_prefixes(text), vec![[192, 168, 65], [192, 168, 66]]);
        assert_eq!(nat_prefixes(""), vec![[192, 168, 64]]);
        assert!(host_gateways(text).contains("192.168.65.1"));
        assert!(host_gateways(text).contains("192.168.65.2"));
        assert!(!host_gateways(text).contains("192.168.1.23"));
        assert!(!host_gateways(text).contains("192.168.1.2"));
        let second = "\
bridge100: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
	inet 192.168.65.2 netmask 0xffffff00 broadcast 192.168.65.255
";
        assert_eq!(nat_prefixes(second), vec![[192, 168, 65]]);
        assert!(host_gateways(second).contains("192.168.65.1"));
        assert!(host_gateways(second).contains("192.168.65.2"));
        assert!(!host_gateways(second).contains("192.168.65.3"));
        assert!(host_side("192.168.64.2"));
        assert!(!host_side("192.168.64.51"));
        assert!(is_guest_host("192.168.64.51"));
        assert!(!is_guest_host("192.168.64.2"));
        assert!(!host_side("192.168.64.3"));
        assert!(!host_side("10.0.0.2"));
    }

    #[test]
    fn bootpd_leases_match_the_guest_mac() {
        let leases = "\
{
	name=debian
	ip_address=192.168.64.51
	hw_address=1,2:56:4d:4d:a0:0
}
";
        let arp = lease_text_to_arp(leases);
        assert_eq!(
            ip_for_short_mac(&["02:56:4d:4d:a0:00".into()], &arp),
            Some("192.168.64.51".into())
        );
        assert!(arp.contains("192.168.64.51"));
        assert!(arp.contains("2:56:4d:4d:a0:0"));
    }
}
