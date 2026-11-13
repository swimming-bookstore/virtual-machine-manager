//! Quick start's disk worker. One JSON object per line. The signature is checked
//! before the ISO is downloaded, and the checksum is checked after.

mod verify;

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

const STEPS: [&str; 6] = ["key", "sums", "sign", "check", "download", "verify"];
const TITLES: [&str; 6] = [
    "Download OpenPGP keyring",
    "Download checksums file",
    "Download signature file",
    "Verify signature",
    "Download ISO",
    "Verify checksum",
];
static ORDER: Mutex<[&'static str; 6]> = Mutex::new(["wait"; 6]);

#[derive(Serialize)]
struct Event<'a> {
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    step: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    received: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

fn blank(event: &str) -> Event<'_> {
    Event {
        event,
        step: None,
        state: None,
        detail: None,
        received: None,
        total: None,
        path: None,
        note: None,
    }
}

fn write_event(event: &Event) {
    let mut out = io::stdout().lock();
    if serde_json::to_writer(&mut out, event).is_ok() {
        let _ = writeln!(out);
    }
    let _ = out.flush();
}

macro_rules! emit {
    ($event:literal $(, $f:ident: $v:expr)* $(,)?) => {{
        let mut ev = blank($event);
        $(ev.$f = $v;)*
        write_event(&ev)
    }};
}

fn note(line: &str) {
    let mut err = io::stderr().lock();
    let _ = if io::stderr().is_terminal() {
        write!(err, "\r\x1b[2K{line}")
    } else {
        writeln!(err, "{line}")
    };
    let _ = err.flush();
}

fn note_done() {
    if io::stderr().is_terminal() {
        let _ = writeln!(io::stderr());
    }
}

fn fail(detail: String) -> ! {
    note_done();
    note(&detail);
    note_done();
    let step = ORDER.lock().ok().and_then(|order| {
        order
            .iter()
            .position(|state| *state == "now")
            .map(|i| STEPS[i])
    });
    emit!("error", step: step, state: Some("fail"), detail: Some(detail));
    std::process::exit(1);
}

/// A step may run only after every earlier step has finished, and a finished step cannot start again.
fn check_order(order: &mut [&'static str; 6], id: &str, state: &str) -> Result<(), String> {
    let Some(index) = STEPS.iter().position(|item| *item == id) else {
        return Err(format!("unknown step {id}"));
    };
    let later = (index + 1..STEPS.len()).find(|&i| order[i] == "now" || order[i] == "done");
    match state {
        "now" if order[index] == "now" => Ok(()),
        "now" => {
            if let Some(earlier) = (0..index).find(|&i| order[i] != "done") {
                return Err(format!("{id} started before {} finished", STEPS[earlier]));
            }
            if let Some(later) = later {
                return Err(format!(
                    "{id} started while {} is {}",
                    STEPS[later], order[later]
                ));
            }
            order[index] = "now";
            Ok(())
        }
        "done" if order[index] == "now" => {
            order[index] = "done";
            Ok(())
        }
        "done" => Err(format!("{id} finished without running")),
        "wait" if order[index] != "now" => {
            Err(format!("{id} went back to waiting without running"))
        }
        "wait" => {
            if let Some(later) = later {
                return Err(format!(
                    "cannot reset {id} while {} is {}",
                    STEPS[later], order[later]
                ));
            }
            order[index] = "wait";
            Ok(())
        }
        "fail" => {
            order[index] = "fail";
            Ok(())
        }
        other => Err(format!("unknown state {other}")),
    }
}

fn step(id: &'static str, state: &'static str, detail: &str, extra: Option<String>) {
    if let Some(err) = {
        let mut order = ORDER.lock().unwrap_or_else(|err| err.into_inner());
        check_order(&mut order, id, state).err()
    } {
        fail(err);
    }
    let mark = match state {
        "done" => "done",
        "now" => "now ",
        "fail" => "fail",
        _ => "wait",
    };
    let title = STEPS
        .iter()
        .position(|item| *item == id)
        .map(|i| TITLES[i])
        .unwrap_or(id);
    note_done();
    note(&format!("{mark}  {title}  {detail}"));
    note_done();
    emit!("step", step: Some(id), state: Some(state), detail: Some(detail.to_string()), note: extra);
}

fn support_dir() -> PathBuf {
    if let Some(dir) = env::var_os("VMM_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join("Library/Application Support/virtual-machine-manager");
        }
    }
    if let Some(xdg) = env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("virtual-machine-manager");
    }
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/share/virtual-machine-manager"))
        .unwrap_or_else(|| PathBuf::from("virtual-machine-manager"))
}

fn guest_arch() -> Result<&'static str, String> {
    match env::consts::ARCH {
        "aarch64" => Ok("arm64"),
        "x86_64" => Ok("amd64"),
        other => Err(format!("{other} has no Debian installer image. This Mac needs the arm64 or amd64 installer that matches its processor.")),
    }
}

fn gib(n: u64) -> String {
    let n = n as f64;
    let gib = 1024.0 * 1024.0 * 1024.0;
    let mib = 1024.0 * 1024.0;
    if n >= gib {
        format!("{:.1} GiB", n / gib)
    } else if n >= mib {
        format!("{:.0} MiB", n / mib)
    } else {
        format!("{:.0} KiB", n / 1024.0)
    }
}

struct Progress {
    step: &'static str,
    last: Instant,
}

impl Progress {
    fn new(step: &'static str) -> Self {
        Self {
            step,
            last: Instant::now() - Duration::from_secs(1),
        }
    }

    fn tick(&mut self, received: u64, total: u64, force: bool) {
        if !force && self.last.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.last = Instant::now();
        let detail = if total > 0 {
            format!("{} of {}", gib(received), gib(total))
        } else {
            gib(received)
        };
        note(&format!("{:<10} {detail}", self.step));
        emit!("progress", step: Some(self.step), state: Some("now"), detail: Some(detail), received: Some(received), total: Some(total));
    }
}

fn total_size(resp: &ureq::Response, status: u16, already: u64) -> u64 {
    if let Some(total) = resp
        .header("content-range")
        .and_then(|range| range.rsplit('/').next())
        .and_then(|n| n.trim().parse().ok())
        .filter(|n: &u64| *n > 0)
    {
        return total;
    }
    resp.header("content-length")
        .and_then(|s| s.parse().ok())
        .map(|len: u64| {
            if status == 206 {
                already.saturating_add(len)
            } else {
                len
            }
        })
        .unwrap_or(0)
}

fn download(agent: &ureq::Agent, url: &str, partial: &Path) -> Result<u64, String> {
    let url = verify::locate(url);
    let have = partial.metadata().map(|m| m.len()).unwrap_or(0);
    let mut request = agent.get(&url);
    if have > 0 {
        request = request.set("Range", &format!("bytes={have}-"));
    }
    let resp = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(416, _)) if have > 0 => return Ok(have),
        Err(ureq::Error::Status(code, resp)) => return Err(verify::status_msg(&url, code, resp)),
        Err(err) => return Err(format!("{url}: {err}")),
    };
    let status = resp.status();
    let total = total_size(&resp, status, have);
    let (mut file, mut received) = if status == 206 && have > 0 {
        (
            OpenOptions::new()
                .append(true)
                .open(partial)
                .map_err(|err| err.to_string())?,
            have,
        )
    } else if status == 200 {
        (File::create(partial).map_err(|err| err.to_string())?, 0)
    } else {
        return Err(format!("{url} returned {status}"));
    };
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 256 * 1024];
    let mut progress = Progress::new("download");
    progress.tick(received, total, true);
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|err| format!("download read: {err}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|err| format!("download write: {err}"))?;
        received += n as u64;
        progress.tick(received, total, false);
    }
    file.sync_all().map_err(|err| err.to_string())?;
    progress.tick(received, total.max(received), true);
    if total > 0 && received != total {
        return Err(format!(
            "download stopped at {} of {}",
            gib(received),
            gib(total)
        ));
    }
    Ok(received)
}

fn hold_lock(cache: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(cache.join("download.lock"))
        .map_err(|err| err.to_string())?;
    file.try_lock().map_err(|_| {
        "a download is already running. Close the other Quick start window and try again."
            .to_string()
    })?;
    Ok(file)
}

fn run(dir: &Path) -> Result<(), String> {
    let arch = guest_arch()?;
    let cache = dir.join("cache");
    fs::create_dir_all(&cache)
        .map_err(|err| format!("cannot create {}: {err}", cache.display()))?;
    let _lock = hold_lock(&cache)?;
    let agent = verify::agent();
    let base = verify::current_dir(arch)?;
    let key_name = verify::key_file();

    step("key", "now", key_name, None);
    let key = verify::fetch_key(&agent)?;
    verify::debian_key(&key)?;
    fs::write(cache.join(key_name), &key).map_err(|err| err.to_string())?;
    step(
        "key",
        "done",
        key_name,
        Some(format!("Key id {}.", verify::key_id())),
    );

    step("sums", "now", "SHA512SUMS", None);
    let sums = verify::fetch(&agent, &format!("{base}/SHA512SUMS"))?;
    fs::write(cache.join("SHA512SUMS"), &sums).map_err(|err| err.to_string())?;
    step("sums", "done", "SHA512SUMS", None);

    step("sign", "now", "SHA512SUMS.sign", None);
    let signature = verify::fetch(&agent, &format!("{base}/SHA512SUMS.sign"))?;
    fs::write(cache.join("SHA512SUMS.sign"), &signature).map_err(|err| err.to_string())?;
    step("sign", "done", "SHA512SUMS.sign", None);

    step("check", "now", "SHA512SUMS.sign", None);
    let published = verify::published(arch, &key, &sums, &signature)?;
    step(
        "check",
        "done",
        "Signature is valid",
        Some(format!("Debian {} signed SHA512SUMS.", published.version)),
    );

    emit!("detail", step: Some("download"), detail: Some(published.name.clone()));
    let url = verify::image_url(&published.version, arch, &published.name);
    let dest = cache.join(&published.name);
    let partial = cache.join(format!("{}.partial", published.name));
    step("download", "now", &published.name, None);
    if !dest.is_file() {
        note(&format!("download   {url}"));
        note_done();
        download(&agent, &url, &partial)?;
        fs::rename(&partial, &dest).map_err(|err| err.to_string())?;
    }
    step("download", "done", &published.name, None);

    step("verify", "now", "SHA512SUMS", None);
    let mut progress = Progress::new("verify");
    let got = verify::hash_file(&dest, &mut |done, total| progress.tick(done, total, false))?;
    let size = dest.metadata().map(|meta| meta.len()).unwrap_or(0);
    progress.tick(size, size, true);
    if got != published.sha512 {
        let _ = fs::remove_file(&dest);
        return Err(format!(
            "the download did not match SHA512SUMS for {}",
            published.name
        ));
    }
    finish(&url, &published, &dest)
}

fn finish(url: &str, published: &verify::Published, dest: &Path) -> Result<(), String> {
    let bytes = dest.metadata().map_err(|err| err.to_string())?.len();
    let cache = dest.parent().ok_or("image path has no directory")?;
    let manifest = serde_json::json!({
        "url": url,
        "sha512": published.sha512,
        "arch": if published.name.contains("-arm64-") { "arm64" } else { "amd64" },
        "file": published.name,
        "bytes": bytes,
        "version": published.version,
        "signature": "verified:SHA512SUMS.sign",
    });
    let tmp = cache.join("manifest.json.partial");
    fs::write(
        &tmp,
        serde_json::to_string_pretty(&manifest).map_err(|err| err.to_string())? + "\n",
    )
    .map_err(|err| err.to_string())?;
    fs::rename(&tmp, cache.join("manifest.json")).map_err(|err| err.to_string())?;
    let ready = "The ISO matches SHA512SUMS.";
    step(
        "verify",
        "done",
        "Checksum matches",
        Some(ready.to_string()),
    );
    note(&format!("ready      {}  {}", dest.display(), gib(bytes)));
    note_done();
    emit!("ready", step: Some("verify"), state: Some("done"), detail: Some("Checksum matches".to_string()), received: Some(bytes), total: Some(bytes), path: Some(dest.display().to_string()), note: Some(ready.to_string()));
    Ok(())
}

fn usage() -> &'static str {
    "usage: virtual-machine-worker download [--dir PATH]\n\nDownloads the Debian 13 installer. SHA512SUMS.sign must verify with the downloaded OpenPGP keyring. A missing signature is a failure.\n"
}

fn parse_dir(args: &[String]) -> Result<PathBuf, String> {
    let mut dir = support_dir();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                dir = PathBuf::from(args.get(i + 1).ok_or("download --dir needs a path")?);
                i += 2;
            }
            "-h" | "--help" => return Err(usage().to_string()),
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
    }
    Ok(dir)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let rest = match args.first().map(String::as_str) {
        Some("download") => &args[1..],
        Some("-h" | "--help" | "help") => {
            print!("{}", usage());
            return;
        }
        Some(other) if other.starts_with('-') => args.as_slice(),
        Some(other) => fail(format!("unknown command {other}\n{}", usage())),
        None => &args[..],
    };
    match parse_dir(rest).and_then(|dir| run(&dir)) {
        Ok(()) => {}
        Err(err) => fail(err),
    }
}

#[cfg(test)]
mod tests {
    use super::check_order;

    fn run_seq(seq: &[(&str, &str)]) -> Result<(), String> {
        let mut order = ["wait"; 6];
        for (id, state) in seq {
            check_order(&mut order, id, state)?;
        }
        Ok(())
    }

    fn full() -> Vec<(&'static str, &'static str)> {
        ["key", "sums", "sign", "check", "download", "verify"]
            .into_iter()
            .flat_map(|id| [(id, "now"), (id, "done")])
            .collect()
    }

    #[test]
    fn checks_run_only_after_the_previous_step() {
        run_seq(&full()).unwrap();
        assert!(run_seq(&[("key", "wait")]).is_err());
        assert!(run_seq(&[
            ("key", "now"),
            ("key", "done"),
            ("sums", "now"),
            ("sign", "now")
        ])
        .is_err());
        assert!(run_seq(&[("key", "done")]).is_err());
        assert!(run_seq(&[("key", "now"), ("key", "done"), ("sums", "done")]).is_err());
    }

    #[test]
    fn a_cached_file_is_checked_without_downloading_again() {
        let mut again = full();
        again.push(("download", "now"));
        assert!(run_seq(&again).is_err());
        assert!(run_seq(&[
            ("key", "now"),
            ("key", "done"),
            ("sums", "now"),
            ("sums", "done"),
            ("sign", "now"),
            ("sign", "done"),
            ("check", "now"),
            ("check", "done"),
            ("verify", "now")
        ])
        .is_err());
    }
}
