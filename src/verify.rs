//! Signed checksum for the Debian installer. Key, then SHA512SUMS, then
//! SHA512SUMS.sign. The signature is checked before the ISO.

use std::time::Duration;

use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey};
use pgp::types::KeyDetails;
use sha2::{Digest, Sha512};

const KEY_FP: &str = "df9b9c49eaa9298432589d76da87e80d6294be9b";
const KEY_FILE: &str = "key-DA87E80D6294BE9B.txt";
const KEY_URL: &str = "https://www.debian.org/CD/key-DA87E80D6294BE9B.txt";

pub struct Published {
    pub name: String,
    pub sha512: String,
    pub version: String,
}

pub fn key_file() -> &'static str {
    KEY_FILE
}

fn key_url() -> &'static str {
    KEY_URL
}

pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(120))
        .redirects(8)
        .user_agent("virtual-machine-manager")
        .build()
}

pub fn current_dir(arch: &str) -> Result<String, String> {
    arch_ok(arch)?;
    Ok(format!(
        "https://cdimage.debian.org/debian-cd/current/{arch}/iso-cd"
    ))
}

pub fn image_url(version: &str, arch: &str, name: &str) -> String {
    format!("https://cdimage.debian.org/debian-cd/{version}/{arch}/iso-cd/{name}")
}

pub fn fetch_key(agent: &ureq::Agent) -> Result<Vec<u8>, String> {
    let url = key_url();
    let body = fetch(agent, url)?;
    if std::str::from_utf8(&body).is_ok_and(|text| text.contains("BEGIN PGP PUBLIC KEY BLOCK")) {
        Ok(body)
    } else {
        Err(format!("OpenPGP key was not found at {url}"))
    }
}

/// `VMM_BASE` points a test at a local server. It is ignored when unset.
pub fn locate(url: &str) -> String {
    let Ok(base) = std::env::var("VMM_BASE") else {
        return url.to_string();
    };
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return url.to_string();
    }
    format!("{base}/{}", url.splitn(4, '/').nth(3).unwrap_or(""))
}

fn fingerprint() -> String {
    std::env::var("VMM_BASE")
        .ok()
        .filter(|base| !base.is_empty())
        .and_then(|_| std::env::var("VMM_KEY_FP").ok())
        .filter(|fp| !fp.is_empty())
        .map(|fp| fp.to_ascii_lowercase())
        .unwrap_or_else(|| KEY_FP.to_string())
}

pub fn status_msg(url: &str, code: u16, resp: ureq::Response) -> String {
    let body: String = resp
        .into_string()
        .unwrap_or_default()
        .chars()
        .take(160)
        .collect();
    format!("{url} returned {code} {body}")
}

pub fn fetch(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    let url = locate(url);
    let resp = match agent.get(&url).call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(404, _)) => return Err(format!("{url} was not found")),
        Err(ureq::Error::Status(code, resp)) => return Err(status_msg(&url, code, resp)),
        Err(err) => return Err(format!("{url}: {err}")),
    };
    if resp.status() != 200 {
        return Err(format!("{url} returned {}", resp.status()));
    }
    let mut body = Vec::new();
    let mut reader = resp.into_reader();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n =
            std::io::Read::read(&mut reader, &mut buf).map_err(|err| format!("{url}: {err}"))?;
        if n == 0 {
            break;
        }
        if body.len() + n > 2_000_000 {
            return Err(format!("{url} is unexpectedly large"));
        }
        body.extend_from_slice(&buf[..n]);
    }
    if body.is_empty() {
        return Err(format!("{url} was empty"));
    }
    Ok(body)
}

/// Checks the downloaded key, then the signature, then reads the installer name.
pub fn published(
    arch: &str,
    key: &[u8],
    sums: &[u8],
    signature: &[u8],
) -> Result<Published, String> {
    arch_ok(arch)?;
    verify(&debian_key(key)?, signature, sums)?;
    let text = std::str::from_utf8(sums).map_err(|err| err.to_string())?;
    let name = installer_name(text, arch)?;
    Ok(Published {
        sha512: digest_for(text, &name)?,
        version: version_of(&name, arch)?,
        name,
    })
}

pub fn debian_key(armor: &[u8]) -> Result<SignedPublicKey, String> {
    let text = std::str::from_utf8(armor).map_err(|_| format!("{KEY_FILE} is not a text key"))?;
    if !text.contains("BEGIN PGP PUBLIC KEY BLOCK") {
        return Err(format!("{KEY_FILE} is not a PGP public key"));
    }
    let (key, _) =
        SignedPublicKey::from_string(text).map_err(|err| format!("{KEY_FILE}: {err}"))?;
    let got = key.fingerprint().to_string().to_ascii_lowercase();
    let want = fingerprint();
    if got != want {
        return Err(format!(
            "{KEY_FILE} fingerprint {got} does not match {want}"
        ));
    }
    Ok(key)
}

fn verify(key: &SignedPublicKey, signature: &[u8], content: &[u8]) -> Result<(), String> {
    parse_signature(signature)?
        .verify(key, content)
        .map_err(|err| format!("SHA512SUMS was not signed by the Debian CD key ({err})"))
}

fn parse_signature(bytes: &[u8]) -> Result<DetachedSignature, String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        if text.contains("BEGIN PGP") {
            return DetachedSignature::from_string(text)
                .map(|(signature, _)| signature)
                .map_err(|err| format!("signature: {err}"));
        }
    }
    DetachedSignature::from_bytes(bytes).map_err(|err| format!("signature: {err}"))
}

fn arch_ok(arch: &str) -> Result<(), String> {
    if arch == "amd64" || arch == "arm64" {
        Ok(())
    } else {
        Err(format!("{arch} is not a Debian installer architecture"))
    }
}

fn installer_name(sums: &str, arch: &str) -> Result<String, String> {
    let suffix = format!("-{arch}-netinst.iso");
    let mut found = None;
    for (_, file) in rows(sums) {
        let Some(rest) = file.strip_prefix("debian-") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_digit()) || !file.ends_with(&suffix) {
            continue;
        }
        if found.is_some() {
            return Err(format!(
                "more than one Debian {arch} installer in the checksum file"
            ));
        }
        found = Some(file.to_string());
    }
    found.ok_or_else(|| format!("the checksum file has no Debian {arch} installer image"))
}

fn version_of(name: &str, arch: &str) -> Result<String, String> {
    let rest = name
        .strip_prefix("debian-")
        .ok_or_else(|| format!("unexpected image name {name}"))?;
    let version = rest.split('-').next().unwrap_or("");
    if version.is_empty()
        || !version.chars().all(|c| c.is_ascii_digit() || c == '.')
        || version.starts_with('.')
        || version.ends_with('.')
    {
        return Err(format!("unexpected Debian version in {name}"));
    }
    if name != format!("debian-{version}-{arch}-netinst.iso") {
        return Err(format!("unexpected Debian image name {name}"));
    }
    Ok(version.to_string())
}

fn digest_for(sums: &str, name: &str) -> Result<String, String> {
    let mut found = None;
    for (hash, file) in rows(sums) {
        if file != name {
            continue;
        }
        if hash.len() != 128 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("checksum file has a bad digest for {name}"));
        }
        if found.is_some() {
            return Err(format!("{name} is listed twice in the checksum file"));
        }
        found = Some(hash.to_ascii_lowercase());
    }
    found.ok_or_else(|| format!("{name} is not in the checksum file"))
}

fn rows(sums: &str) -> impl Iterator<Item = (&str, &str)> {
    sums.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut parts = line.split_whitespace();
        Some((
            parts.next().unwrap_or(""),
            parts.next().unwrap_or("").trim_start_matches('*'),
        ))
    })
}

pub fn hash_file(
    path: &std::path::Path,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|err| err.to_string())?;
    let total = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let mut hasher = Sha512::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut done = 0u64;
    on_progress(0, total);
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)
            .map_err(|err| format!("verify read: {err}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        on_progress(done, total);
    }
    on_progress(done, total.max(done));
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_file_matches_the_published_key_id() {
        assert_eq!(key_file(), "key-DA87E80D6294BE9B.txt");
        assert_eq!(
            key_url(),
            "https://www.debian.org/CD/key-DA87E80D6294BE9B.txt"
        );
        assert_eq!(&fingerprint()[fingerprint().len() - 16..], "da87e80d6294be9b");
    }

    #[test]
    fn picks_the_official_installer() {
        let sums = include_str!("../tests/fixtures/SHA512SUMS");
        assert_eq!(
            installer_name(sums, "amd64").unwrap(),
            "debian-13.7.0-amd64-netinst.iso"
        );
        assert_eq!(
            version_of("debian-13.7.0-arm64-netinst.iso", "arm64").unwrap(),
            "13.7.0"
        );
        assert!(installer_name("aaa  debian-edu-13.7.0-amd64-netinst.iso\n", "amd64").is_err());
        assert!(digest_for(
            "abcd  debian-13.7.0-amd64-netinst.iso",
            "debian-13.7.0-amd64-netinst.iso"
        )
        .is_err());
    }

    #[test]
    fn published_rejects_a_bad_key() {
        let sums = include_bytes!("../tests/fixtures/SHA512SUMS");
        assert!(published("amd64", b"not a key", sums, b"not a signature").is_err());
        assert!(debian_key(b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nnot a key\n").is_err());
    }

    #[test]
    #[ignore = "needs network"]
    fn current_installer_signature_verifies() {
        let agent = agent();
        let base = current_dir("amd64").unwrap();
        let got = published(
            "amd64",
            &fetch_key(&agent).unwrap(),
            &fetch(&agent, &format!("{base}/SHA512SUMS")).unwrap(),
            &fetch(&agent, &format!("{base}/SHA512SUMS.sign")).unwrap(),
        )
        .unwrap();
        assert_eq!(got.sha512.len(), 128);
        assert!(got.name.ends_with("-amd64-netinst.iso"));
    }
}
