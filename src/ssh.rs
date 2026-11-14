//! Host key for the guest. The install is finished when this key can log in.

#![allow(dead_code)]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

use crate::preseed::{AUTHORIZED, SSH_PORT, USER};

const PRIVATE: &str = "id_ed25519";
const PUBLIC: &str = "id_ed25519.pub";
const PROOF: &str = "logged-in";

pub struct Keys {
    pub private: PathBuf,
    pub public_line: String,
}

/// Create the host key once. The public half is what the installer copies.
pub fn ensure(dir: &Path) -> Result<Keys> {
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
    Ok(Keys { private, public_line })
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

/// One attempt. A closed port or a refused key is not a finished install.
pub fn probe(dir: &Path, host: &str) -> Result<bool> {
    if logged_in(dir)? {
        return Ok(true);
    }
    let keys = ensure(dir)?;
    if !login(&keys.private, host)? {
        return Ok(false);
    }
    write_new(&dir.join(PROOF), format!("{USER} ok\n").as_bytes())?;
    Ok(true)
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

/// Keep trying until the guest answers or the wait runs out.
pub fn wait_login(dir: &Path, host: &str, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if probe(dir, host)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
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
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=2",
            "-p",
            &port,
            "-i",
        ])
        .arg(private)
        .args(["-l", USER, host, "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match out {
        Ok(status) => Ok(status.success()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn write_new(dest: &Path, body: &[u8]) -> Result<()> {
    let tmp = dest.with_extension("partial");
    let mut file = File::create(&tmp)?;
    file.write_all(body)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, dest)?;
    Ok(())
}

/// Test double. A real guest is not required to prove the snapshot waits for a login.
pub fn record_login(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    write_new(&dir.join(PROOF), format!("{USER} ok\n").as_bytes())?;
    Ok(())
}

pub fn read_proof(dir: &Path) -> Result<String> {
    let mut file = File::open(dir.join(PROOF))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}
