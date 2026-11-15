//! Quick start's disk worker. One JSON object per line. The signature is checked
//! before the ISO is downloaded, and the checksum is checked after.

mod preseed;
mod snapshot;
mod ssh;
mod verify;

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

const STEPS: [&str; 10] = [
    "key", "sums", "sign", "check", "download", "verify", "sshkey", "preseed", "disk", "patch",
];

enum OrderMsg {
    Set(&'static str, &'static str, Sender<Result<()>>),
    Current(Sender<Option<&'static str>>),
}

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
    disk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
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
        disk: None,
        key: None,
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

fn fail(err: anyhow::Error, order: &Sender<OrderMsg>) -> ! {
    let (tx, rx) = mpsc::channel();
    let _ = order.send(OrderMsg::Current(tx));
    let step = rx.recv().ok().flatten();
    emit!("error", step: step, state: Some("fail"), detail: Some(err.to_string()));
    std::process::exit(1);
}

/// A step may run only after every earlier step has finished, and a finished step cannot start again.
fn check_order(order: &mut [&'static str], id: &str, state: &str) -> Result<()> {
    let Some(index) = STEPS.iter().position(|item| *item == id) else {
        bail!("unknown step {id}");
    };
    let later = (index + 1..STEPS.len()).find(|&i| order[i] == "now" || order[i] == "done");
    match state {
        "now" if order[index] == "now" => Ok(()),
        "now" => {
            if let Some(earlier) = (0..index).find(|&i| order[i] != "done") {
                bail!("{id} started before {} finished", STEPS[earlier]);
            }
            if let Some(later) = later {
                bail!("{id} started while {} is {}", STEPS[later], order[later]);
            }
            order[index] = "now";
            Ok(())
        }
        "done" if order[index] == "now" => {
            order[index] = "done";
            Ok(())
        }
        "done" => bail!("{id} finished without running"),
        "wait" if order[index] != "now" => {
            bail!("{id} went back to waiting without running")
        }
        "wait" => {
            if let Some(later) = later {
                bail!(
                    "cannot reset {id} while {} is {}",
                    STEPS[later],
                    order[later]
                );
            }
            order[index] = "wait";
            Ok(())
        }
        "fail" => {
            order[index] = "fail";
            Ok(())
        }
        other => bail!("unknown state {other}"),
    }
}

fn order_loop(rx: Receiver<OrderMsg>) {
    let mut order = ["wait"; STEPS.len()];
    while let Ok(msg) = rx.recv() {
        match msg {
            OrderMsg::Set(id, state, reply) => {
                let _ = reply.send(check_order(&mut order, id, state));
            }
            OrderMsg::Current(reply) => {
                let step = order
                    .iter()
                    .position(|state| *state == "now")
                    .map(|i| STEPS[i]);
                let _ = reply.send(step);
            }
        }
    }
}

/// True when this step's file is on disk. Signature check has no file of its own.
fn wrote(dir: &Path, cache: &Path, id: &str) -> bool {
    let has = |path: &Path| path.is_file();
    match id {
        "key" => has(&cache.join(verify::key_file())),
        "sums" => has(&cache.join("SHA512SUMS")),
        "sign" => has(&cache.join("SHA512SUMS.sign")),
        "check" => true,
        "download" => cached_iso(cache, guest_arch().unwrap_or("amd64")).is_ok(),
        "verify" => has(&cache.join("manifest.json")),
        "sshkey" => {
            has(&dir.join("ssh").join("id_ed25519"))
                && has(&dir.join("ssh").join("id_ed25519.pub"))
                && has(&dir.join("ssh").join(preseed::AUTHORIZED))
        }
        "preseed" => has(&dir.join(preseed::DIR).join(preseed::FILE)),
        "disk" => has(&dir.join(preseed::SNAPSHOT).join(preseed::DISK)),
        "patch" => has(&dir.join(preseed::DIR).join(preseed::BOOT)),
        _ => false,
    }
}

/// Writes until the step's file is still there. One delete during the write is not a failed run.
fn keep(
    dir: &Path,
    cache: &Path,
    id: &'static str,
    mut write: impl FnMut() -> Result<()>,
) -> Result<()> {
    for _ in 0..4 {
        write()?;
        if wrote(dir, cache, id) {
            return Ok(());
        }
    }
    bail!("{id} file is missing");
}

fn step(id: &'static str, state: &'static str, order: &Sender<OrderMsg>) {
    let (tx, rx) = mpsc::channel();
    if order.send(OrderMsg::Set(id, state, tx)).is_err() {
        fail(anyhow!("step order closed"), order);
    }
    if let Err(err) = rx
        .recv()
        .unwrap_or_else(|_| Err(anyhow!("step order closed")))
    {
        fail(err, order);
    }
    emit!("step", step: Some(id), state: Some(state));
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

fn guest_arch() -> Result<&'static str> {
    match env::consts::ARCH {
        "aarch64" => Ok("arm64"),
        "x86_64" => Ok("amd64"),
        other => bail!("{other} has no Debian installer image. This Mac needs the arm64 or amd64 installer that matches its processor."),
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
        emit!("progress", step: Some(self.step), state: Some("now"), received: Some(received), total: Some(total));
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

fn download(agent: &ureq::Agent, url: &str, partial: &Path) -> Result<u64> {
    let url = verify::locate(url);
    let have = partial.metadata().map(|m| m.len()).unwrap_or(0);
    let mut progress = Progress::new("download");
    // DNS, TLS, and mirror redirects can take seconds. Paint the bar before that, not after the headers.
    progress.tick(have, 0, true);
    let mut request = agent.get(&url);
    if have > 0 {
        request = request.set("Range", &format!("bytes={have}-"));
    }
    let resp = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(416, _)) if have > 0 => {
            progress.tick(have, have, true);
            return Ok(have);
        }
        Err(ureq::Error::Status(code, resp)) => bail!("{}", verify::status_msg(&url, code, resp)),
        Err(err) => bail!("{url}: {err}"),
    };
    let status = resp.status();
    let total = total_size(&resp, status, have);
    let (mut file, mut received) = if status == 206 && have > 0 {
        (OpenOptions::new().append(true).open(partial)?, have)
    } else if status == 200 {
        (File::create(partial)?, 0)
    } else {
        bail!("{url} returned {status}");
    };
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 256 * 1024];
    progress.tick(received, total, true);
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|err| anyhow!("download read: {err}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|err| anyhow!("download write: {err}"))?;
        received += n as u64;
        progress.tick(received, total, false);
    }
    file.sync_all()?;
    progress.tick(received, total.max(received), true);
    if total > 0 && received != total {
        bail!("download stopped at {} of {}", gib(received), gib(total));
    }
    Ok(received)
}

fn hold_lock(cache: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(cache.join("download.lock"))?;
    file.try_lock().map_err(|_| {
        anyhow!("a download is already running. Close the other Quick start window and try again.")
    })?;
    Ok(file)
}

fn read_cached(path: &Path) -> Result<Vec<u8>> {
    let bytes = fs::read(path).map_err(|err| anyhow!("cannot read {}: {err}", path.display()))?;
    if bytes.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(bytes)
}

fn cached_iso(cache: &Path, arch: &str) -> Result<PathBuf> {
    let suffix = format!("-{arch}-netinst.iso");
    let mut found = Vec::new();
    let entries =
        fs::read_dir(cache).map_err(|err| anyhow!("cannot read {}: {err}", cache.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|err| anyhow!("cannot read {}: {err}", cache.display()))?
            .path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("debian-") && name.ends_with(&suffix) && path.is_file() {
            found.push(path);
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => bail!("installer image is missing"),
        _ => bail!("more than one installer image in {}", cache.display()),
    }
}

fn write_manifest(url: &str, published: &verify::Published, dest: &Path) -> Result<()> {
    let bytes = dest.metadata()?.len();
    let cache = dest
        .parent()
        .ok_or_else(|| anyhow!("image path has no directory"))?;
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
    fs::write(&tmp, serde_json::to_string_pretty(&manifest)? + "\n")?;
    fs::rename(&tmp, cache.join("manifest.json"))?;
    Ok(())
}

/// Reset keeps the download cache and does every later step again, the way Packer rebuilds an image.
/// The signed ISO, checksums, and keyring stay. Preseed, the guest disk, the installer copy, and the snapshot do not.
fn clear_for_rebuild(dir: &Path) -> Result<()> {
    for path in [
        dir.join(preseed::DIR),
        dir.join("ssh"),
        dir.join(preseed::SNAPSHOT),
    ] {
        if path.exists() {
            fs::remove_dir_all(&path)
                .map_err(|err| anyhow!("cannot remove {}: {err}", path.display()))?;
        }
    }
    Ok(())
}

fn emit_ready(dest: &Path, prepared: &preseed::Prepared) -> Result<()> {
    let bytes = dest.metadata()?.len();
    emit!("ready", step: Some("patch"), state: Some("done"), received: Some(bytes), total: Some(bytes), path: Some(dest.display().to_string()), disk: Some(prepared.disk.display().to_string()), key: Some(prepared.key.display().to_string()));
    Ok(())
}

fn run(dir: &Path, order: &Sender<OrderMsg>) -> Result<()> {
    let arch = guest_arch()?;
    let cache = dir.join("cache");
    fs::create_dir_all(&cache)
        .map_err(|err| anyhow!("cannot create {}: {err}", cache.display()))?;
    let _lock = hold_lock(&cache)?;
    // Every run starts at the first step. A file already in the cache is not fetched again.
    let agent = verify::agent();
    let base = verify::current_dir(arch)?;
    let key_name = verify::key_file();

    step("key", "now", order);
    let key = {
        keep(dir, &cache, "key", || {
            if !wrote(dir, &cache, "key") {
                let fetched = verify::fetch_key(&agent)?;
                verify::debian_key(&fetched)?;
                fs::write(cache.join(key_name), &fetched)?;
            }
            Ok(())
        })?;
        read_cached(&cache.join(key_name))?
    };
    step("key", "done", order);

    step("sums", "now", order);
    let sums = {
        keep(dir, &cache, "sums", || {
            if !wrote(dir, &cache, "sums") {
                let fetched = verify::fetch(&agent, &format!("{base}/SHA512SUMS"))?;
                fs::write(cache.join("SHA512SUMS"), &fetched)?;
            }
            Ok(())
        })?;
        read_cached(&cache.join("SHA512SUMS"))?
    };
    step("sums", "done", order);

    step("sign", "now", order);
    let signature = {
        keep(dir, &cache, "sign", || {
            if !wrote(dir, &cache, "sign") {
                let fetched = verify::fetch(&agent, &format!("{base}/SHA512SUMS.sign"))?;
                fs::write(cache.join("SHA512SUMS.sign"), &fetched)?;
            }
            Ok(())
        })?;
        read_cached(&cache.join("SHA512SUMS.sign"))?
    };
    step("sign", "done", order);

    step("check", "now", order);
    let published = verify::published(arch, &key, &sums, &signature)?;
    step("check", "done", order);

    let url = verify::image_url(&published.version, arch, &published.name);
    let dest = cache.join(&published.name);
    step("download", "now", order);
    if !dest.is_file() {
        keep(dir, &cache, "download", || {
            let partial = cache.join(format!("{}.partial", published.name));
            download(&agent, &url, &partial)?;
            fs::rename(&partial, &dest)?;
            Ok(())
        })?;
    }
    step("download", "done", order);

    // Verify checksum is an action. The ISO path is the previous row, not this one.
    step("verify", "now", order);
    let mut progress = Progress::new("verify");
    let got = verify::hash_file(&dest, &mut |done, total| progress.tick(done, total, false))?;
    let size = dest.metadata().map(|meta| meta.len()).unwrap_or(0);
    progress.tick(size, size, true);
    if got != published.sha512 {
        let _ = fs::remove_file(&dest);
        bail!(
            "the download did not match SHA512SUMS for {}",
            published.name
        );
    }
    if !wrote(dir, &cache, "verify") {
        keep(dir, &cache, "verify", || {
            write_manifest(&url, &published, &dest)?;
            Ok(())
        })?;
    }
    step("verify", "done", order);

    // The host key, the answer disk, the empty guest disk, and the installer copy are separate steps.
    let ssh = dir.join("ssh");
    step("sshkey", "now", order);
    if !wrote(dir, &cache, "sshkey") {
        keep(dir, &cache, "sshkey", || {
            ssh::ensure(&ssh)?;
            Ok(())
        })?;
    }
    step("sshkey", "done", order);
    step("preseed", "now", order);
    if !wrote(dir, &cache, "preseed") {
        keep(dir, &cache, "preseed", || {
            preseed::write_preseed(dir)?;
            Ok(())
        })?;
    }
    step("preseed", "done", order);
    step("disk", "now", order);
    if !wrote(dir, &cache, "disk") {
        keep(dir, &cache, "disk", || {
            preseed::write_guest_disk(dir)?;
            Ok(())
        })?;
    }
    step("disk", "done", order);
    step("patch", "now", order);
    if !wrote(dir, &cache, "patch") {
        keep(dir, &cache, "patch", || {
            preseed::prepare_installer(&dest, dir)?;
            Ok(())
        })?;
    }
    step("patch", "done", order);
    let prepared = preseed::Prepared {
        disk: dir.join(preseed::SNAPSHOT).join(preseed::DISK),
        key: dir.join("ssh").join(preseed::AUTHORIZED),
    };
    emit_ready(&dest, &prepared)?;
    Ok(())
}

#[derive(Parser)]
#[command(
    name = "virtual-machine-worker",
    about = "Downloads the Debian 13 installer. SHA512SUMS.sign must verify with the downloaded OpenPGP keyring. A missing signature is a failure."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download the Debian 13 installer and check its signature.
    Download {
        /// Support directory. Downloads go in PATH/cache. preseed.img and boot.iso go in PATH/installer. Defaults to the app support directory.
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
        /// Rebuild the image. Only the download cache is kept. Preseed, the guest disk, and the snapshot are done again.
        #[arg(long)]
        rebuild: bool,
    },
    /// Copy a finished install so later machines skip the installer.
    Snapshot {
        /// Support directory that contains snapshots/debian. Defaults to the app support directory.
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },
    /// Clone the snapshot into a new machine directory.
    Clone {
        /// Support directory. Defaults to the app support directory.
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
        /// Directory name under machines/. Defaults to the next debian-N.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// One SSH attempt. A closed port is not a finished install.
    Ssh {
        /// Guest disk whose host key should log in.
        #[arg(long, value_name = "PATH")]
        disk: PathBuf,
    },
}

fn main() {
    let cli = Cli::try_parse().unwrap_or_else(|err| err.exit());
    match cli.command {
        Command::Download { dir, rebuild } => {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || order_loop(rx));
            let support = dir.unwrap_or_else(support_dir);
            if rebuild {
                if let Err(err) = clear_for_rebuild(&support) {
                    fail(err, &tx);
                }
            }
            if let Err(err) = run(&support, &tx) {
                fail(err, &tx);
            }
        }
        Command::Snapshot { dir } => {
            let support = dir.unwrap_or_else(support_dir);
            match snapshot::save(&support) {
                Ok(saved) => emit_saved("snapshot", &saved),
                Err(err) => die(err),
            }
        }
        Command::Clone { dir, name } => {
            let support = dir.unwrap_or_else(support_dir);
            match snapshot::clone_machine(&support, name.as_deref()) {
                Ok(saved) => emit_saved("clone", &saved),
                Err(err) => die(err),
            }
        }
        Command::Ssh { disk } => {
            ssh::own_group();
            match ssh::probe_disk(&disk) {
                Ok(true) => emit!("ssh", state: Some("done")),
                Ok(false) => {
                    emit!("ssh", state: Some("wait"));
                    std::process::exit(1);
                }
                Err(err) => die(err),
            }
        }
    }
}

fn die(err: anyhow::Error) -> ! {
    emit!("error", detail: Some(err.to_string()));
    std::process::exit(1);
}

fn emit_saved(event: &str, saved: &snapshot::Saved) {
    let path = saved.dir.display().to_string();
    let disk = saved.disk.display().to_string();
    match event {
        "clone" => emit!("clone", state: Some("done"), path: Some(path), disk: Some(disk)),
        _ => emit!("snapshot", state: Some("done"), path: Some(path), disk: Some(disk)),
    }
}

#[cfg(test)]
mod tests {
    use super::{check_order, STEPS};

    fn run_seq(seq: &[(&str, &str)]) -> anyhow::Result<()> {
        let mut order = ["wait"; STEPS.len()];
        for (id, state) in seq {
            check_order(&mut order, id, state)?;
        }
        Ok(())
    }

    fn full() -> Vec<(&'static str, &'static str)> {
        [
            "key", "sums", "sign", "check", "download", "verify", "sshkey", "preseed", "disk",
            "patch",
        ]
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
