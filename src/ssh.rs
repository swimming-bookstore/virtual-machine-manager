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

/// Guest IP for this disk. The caller polls, so each pass matches this disk's MAC.
/// `.1` and `.2` are this Mac. A saved `.3` is not shown when this MAC is already at `.51`.
/// ARP stays empty until this Mac asks, so a miss pings `.51` and the newest lease, then asks the guest.
/// That is one address, not a subnet scan.
pub fn guest_address(disk: &Path, mac: Option<&str>) -> Option<String> {
    let wants = macs_for(disk, mac);
    if wants.is_empty() {
        return None;
    }
    let gateways = host_gateways(&ifconfig_text());
    let leases = guest_rows(parse_leases(&lease_text()), &gateways);
    let asks = ask_hosts(&nat_prefixes(&ifconfig_text()), &leases);
    if let Some(host) = live_match(&wants, &guest_rows(arp_guests(), &gateways)) {
        return Some(host);
    }
    for host in &asks {
        nudge(host);
        let arp = guest_rows(arp_guests(), &gateways);
        if let Some(found) = live_match(&wants, &arp) {
            return Some(found);
        }
    }
    // The guest's own `ip addr` is the address for this MAC, even when the lease still says `.3`.
    for host in asks.iter().chain(saved_ip(disk).iter()) {
        for want in &wants {
            if let Some(ip) = guest_reported(disk, want, host) {
                return Some(ip);
            }
        }
    }
    // Lease only after `.51` did not answer. The last grant for this MAC wins.
    lease_match(&wants, &leases)
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

fn guest_rows(rows: Vec<(String, String)>, gateways: &HashSet<String>) -> Vec<(String, String)> {
    rows.into_iter()
        .filter(|(host, _)| !gateways.contains(host) && !host_side(host))
        .collect()
}

/// `.51` first. On the first shared network that is the guest, and `.2` is this Mac.
/// The newest lease is next, so a stale `.3` is not asked before `.51`.
fn ask_hosts(prefixes: &[[u8; 3]], leases: &[(String, String)]) -> Vec<String> {
    let mut hosts = Vec::new();
    for prefix in prefixes {
        let host = format!("{}.{}.{}.51", prefix[0], prefix[1], prefix[2]);
        if is_guest_host(&host) && !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    if let Some((host, _)) = leases.last() {
        if is_guest_host(host) && !hosts.contains(host) {
            hosts.push(host.clone());
        }
    }
    hosts
}

/// ARP is the live address. A lease for the same MAC is not used while ARP already has it.
fn live_match(wants: &[String], arp: &[(String, String)]) -> Option<String> {
    for want in wants {
        if let Some(host) = address_for_mac(want, arp) {
            return Some(host);
        }
    }
    None
}

fn lease_match(wants: &[String], leases: &[(String, String)]) -> Option<String> {
    for want in wants {
        if let Some(host) = address_for_mac(want, leases) {
            return Some(host);
        }
    }
    None
}

/// One ARP request. Not a subnet scan, so bridge100 does not fill with incomplete rows.
fn nudge(host: &str) {
    let wait = if cfg!(target_os = "macos") { "1000" } else { "1" };
    for bin in ["/sbin/ping", "/usr/sbin/ping", "ping"] {
        if Command::new(bin)
            .args(["-c", "1", "-W", wait, host])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            return;
        }
    }
}

fn saved_ip(disk: &Path) -> Option<String> {
    let text = fs::read_to_string(disk.parent()?.join("ip")).ok()?;
    let host = text.trim();
    if is_guest_host(host) {
        Some(host.to_string())
    } else {
        None
    }
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

/// Prefer the address the guest reports for this MAC. ARP is the same address once the host has asked.
fn guest_reported(disk: &Path, want: &str, host: &str) -> Option<String> {
    let guest = disk.parent()?;
    let support = guest.parent()?.parent()?;
    let private = support.join("ssh").join(PRIVATE);
    if !private.is_file() {
        return None;
    }
    let text = ssh_capture(&private, host, "ip addr")?;
    guest_ip(want, &text)
}

/// UTM keeps an IPv4 that does not start with 127, else an IPv6 that is not loopback.
/// The first IPv4 is the one to log in to.
fn guest_ip(want: &str, text: &str) -> Option<String> {
    let want = normalize_mac(want);
    if want.is_empty() {
        return None;
    }
    let mut current = String::new();
    let mut macs = Vec::new();
    let mut addrs: Vec<(String, String, bool)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim().trim_end_matches('\\').trim();
        if line.is_empty() || line == "---" {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() >= 2 {
            let index = tokens[0].trim_end_matches(':');
            if index.parse::<u32>().is_ok() {
                current = tokens[1].trim_end_matches(':').to_string();
            }
        }
        if let Some(i) = tokens.iter().position(|token| *token == "link/ether") {
            if let Some(mac) = tokens.get(i + 1) {
                if !current.is_empty() {
                    macs.push((current.clone(), normalize_mac(mac)));
                }
            }
        }
        if let Some(i) = tokens
            .iter()
            .position(|token| *token == "inet" || *token == "inet6")
        {
            let v6 = tokens[i] == "inet6";
            if let Some(raw_ip) = tokens.get(i + 1) {
                let ip = raw_ip.split('/').next().unwrap_or(raw_ip);
                if !current.is_empty() && usable_guest_ip(ip, v6) {
                    addrs.push((current.clone(), ip.to_string(), v6));
                }
            }
        }
    }
    let iface = macs.into_iter().find(|(_, mac)| mac == &want)?.0;
    addrs
        .iter()
        .find(|(name, _, v6)| name == &iface && !*v6)
        .or_else(|| addrs.iter().find(|(name, _, v6)| name == &iface && *v6))
        .map(|(_, ip, _)| ip.clone())
}

fn usable_guest_ip(ip: &str, v6: bool) -> bool {
    if v6 {
        return ip != "::1" && ip != "0:0:0:0:0:0:0:1";
    }
    usable_ipv4(ip)
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

fn ssh_capture(private: &Path, host: &str, command: &str) -> Option<String> {
    let port = SSH_PORT.to_string();
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "ConnectTimeout=1",
            "-p",
            &port,
            "-i",
        ])
        .arg(private)
        .args(["-l", USER, host, command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
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

/// bootpd writes `key=value` blocks. `hw_address` is `1,aa:bb:cc:dd:ee:ff`, not two words.
fn parse_leases(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut ip = None;
    let mut mac = None;
    let finish =
        |ip: &mut Option<String>, mac: &mut Option<String>, found: &mut Vec<(String, String)>| {
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

fn address_for_mac(want: &str, arp: &[(String, String)]) -> Option<String> {
    if want.is_empty() {
        return None;
    }
    let matched: Vec<&(String, String)> = arp.iter().filter(|(_, seen)| seen == want).collect();
    // The current grant is last. A later en0 row is not this guest when a vmnet address is present.
    matched
        .iter()
        .rev()
        .find(|(host, _)| nat_guest(host))
        .or_else(|| matched.last())
        .map(|(host, _)| (*host).clone())
}

/// A vmnet shared-network address. `192.168.64.51` is one. `10.0.0.2` on en0 is not.
fn nat_guest(host: &str) -> bool {
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
    parts.next().is_none() && a == "192" && b == "168" && c >= 64 && d != 1 && d != 2
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
        if !is_guest_host(host) {
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
            vec![
                ("192.168.64.7".into(), "02:56:4d:4d:09:35".into()),
                ("10.0.0.2".into(), "02:56:4d:4d:09:35".into()),
            ]
        );
        let mac = "02:56:4d:4d:09:35";
        assert_eq!(
            address_for_mac(mac, &parse_arp(text)),
            Some("192.168.64.7".into())
        );
        assert_eq!(address_for_mac(mac, &[]), None);
        let guest = "\
? (192.168.64.1) at aa:bb:cc:dd:ee:ff on bridge100
? (192.168.64.2) at bb:bb:cc:dd:ee:ff on bridge100
? (192.168.64.51) at 02:56:4d:4d:09:35 on bridge100
";
        assert_eq!(
            address_for_mac("02:56:4d:4d:09:35", &parse_arp(guest)),
            Some("192.168.64.51".into())
        );
        let moved = vec![
            ("192.168.64.3".into(), "02:56:4d:4d:09:35".into()),
            ("192.168.64.51".into(), "02:56:4d:4d:09:35".into()),
        ];
        assert_eq!(
            address_for_mac("02:56:4d:4d:09:35", &moved),
            Some("192.168.64.51".into())
        );
        assert_eq!(
            ask_hosts(&[[192, 168, 64]], &moved),
            vec![String::from("192.168.64.51")]
        );
        assert_eq!(
            ask_hosts(
                &[[192, 168, 64]],
                &[("192.168.64.3".into(), "aa:bb:cc:dd:ee:ff".into())]
            ),
            vec![String::from("192.168.64.51"), String::from("192.168.64.3")]
        );
        assert_eq!(normalize_mac("1,01:02:56:4d:4d:09:35"), "02:56:4d:4d:09:35");
    }

    #[test]
    fn lease_file_is_key_equals_value() {
        let text = "{\n\tname=debian\n\tip_address=192.168.64.5\n\thw_address=1,2:56:4d:4d:9:35\n\tidentifier=1,2:56:4d:4d:9:35\n\tlease=0x1\n}\n{\n\tip_address=192.168.64.1\n\thw_address=1,aa:bb:cc:dd:ee:ff\n}\n";
        let mac = "02:56:4d:4d:09:35";
        assert_eq!(
            address_for_mac(mac, &parse_leases(text)),
            Some("192.168.64.5".into())
        );
        assert_eq!(
            address_for_mac("02:00:00:00:00:01", &parse_leases(text)),
            None
        );
        assert_eq!(
            parse_leases("ip_address=10.0.0.2\nhw_address=1,02:56:4d:4d:09:35\n"),
            vec![("10.0.0.2".into(), "02:56:4d:4d:09:35".into())]
        );
        assert_eq!(
            parse_leases("ip_address=127.0.0.1\nhw_address=1,02:56:4d:4d:09:35\n"),
            vec![]
        );
        assert_eq!(
            address_for_mac(
                mac,
                &parse_leases("ip_address=192.168.65.4\nhw_address=1,02:56:4d:4d:09:35\n")
            ),
            Some("192.168.65.4".into())
        );
        assert!(parse_leases("ip_address=192.168.64.1\nhw_address=1,aa:bb:cc:dd:ee:ff\n").is_empty());
        assert!(parse_leases("ip_address=192.168.64.2\nhw_address=1,aa:bb:cc:dd:ee:ff\n").is_empty());
        let second = "bridge100: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\tinet 192.168.65.2 netmask 0xffffff00 broadcast 192.168.65.255\n";
        let gateways = host_gateways(second);
        assert!(gateways.contains("192.168.65.1"));
        assert!(gateways.contains("192.168.65.2"));
        let first = "bridge100: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\tinet 192.168.64.1 netmask 0xffffff00 broadcast 192.168.64.255\n";
        let paired = host_gateways(first);
        assert!(paired.contains("192.168.64.1"));
        assert!(paired.contains("192.168.64.2"));
        let leased = parse_leases("ip_address=192.168.65.2\nhw_address=1,aa:bb:cc:dd:ee:ff\nip_address=192.168.65.3\nhw_address=1,02:56:4d:4d:09:35\n");
        let guests: Vec<_> = leased
            .into_iter()
            .filter(|(host, _)| !gateways.contains(host) && !host_side(host))
            .collect();
        assert_eq!(guests, vec![("192.168.65.3".into(), "02:56:4d:4d:09:35".into())]);
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
    fn guest_ip_matches_the_nic_and_skips_loopback() {
        let text = "\
1: lo: <LOOPBACK,UP> mtu 65536 qdisc noqueue state UNKNOWN mode DEFAULT group default qlen 1000
    link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00
    inet 127.0.0.1/8 scope host lo
    inet6 ::1/128 scope host
2: enp0s1: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc fq_codel state UP mode DEFAULT group default qlen 1000
    link/ether 02:56:4D:4D:09:35 brd ff:ff:ff:ff:ff:ff
    inet 192.168.65.4/24 brd 192.168.65.255 scope global dynamic enp0s1
    inet6 fe80::1/64 scope link
    inet6 ::1/128 scope host
";
        assert_eq!(
            guest_ip("02:56:4d:4d:09:35", text),
            Some("192.168.65.4".into())
        );
        assert_eq!(guest_ip("02:00:00:00:00:01", text), None);
        assert_eq!(
            guest_ip("02:56:4d:4d:09:35", "2: enp0s1\n    inet 127.0.0.1/8\n"),
            None
        );
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
}
