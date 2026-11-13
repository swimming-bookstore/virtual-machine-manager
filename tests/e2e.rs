//! End-to-end worker run. A local server stands in for Debian.
//! The signature is checked before the ISO is fetched.
//! Quick start is replayed from that JSON so the window stays tied to it.

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const KEY: &[u8] = include_bytes!("fixtures/e2e/key.asc");
const SUMS: &[u8] = include_bytes!("fixtures/e2e/SHA512SUMS");
const SIGN: &[u8] = include_bytes!("fixtures/e2e/SHA512SUMS.sign");
const ISO: &[u8] = include_bytes!("fixtures/e2e/iso.bin");
const FP: &str = include_str!("fixtures/e2e/fp.txt");

fn arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}

fn iso_name() -> String {
    format!("debian-13.7.0-{}-netinst.iso", arch())
}

struct World {
    files: Vec<(&'static str, Vec<u8>)>,
    log: Mutex<Vec<String>>,
    cut: Mutex<bool>,
    hold: Mutex<bool>,
    cv: Condvar,
}

fn file(path: &str, world: &World) -> Option<Vec<u8>> {
    world
        .files
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, b)| b.clone())
}

fn serve(
    sums: &[u8],
    sign: &[u8],
    iso: &[u8],
    key: &[u8],
    cut: bool,
    hold: bool,
) -> (String, Arc<World>) {
    let mut files = vec![("/CD/key-DA87E80D6294BE9B.txt", key.to_vec())];
    for arch in ["amd64", "arm64"] {
        files.push((
            Box::leak(format!("/debian-cd/current/{arch}/iso-cd/SHA512SUMS").into_boxed_str()),
            sums.to_vec(),
        ));
        files.push((
            Box::leak(format!("/debian-cd/current/{arch}/iso-cd/SHA512SUMS.sign").into_boxed_str()),
            sign.to_vec(),
        ));
        files.push((
            Box::leak(
                format!("/debian-cd/13.7.0/{arch}/iso-cd/debian-13.7.0-{arch}-netinst.iso")
                    .into_boxed_str(),
            ),
            iso.to_vec(),
        ));
    }
    let world = Arc::new(World {
        files,
        log: Mutex::new(Vec::new()),
        cut: Mutex::new(cut),
        hold: Mutex::new(hold),
        cv: Condvar::new(),
    });
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let bg = world.clone();
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let world = bg.clone();
            thread::spawn(move || handle(conn, &world));
        }
    });
    (base, world)
}

fn handle(mut sock: TcpStream, world: &World) {
    let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 8192 {
        let Ok(n) = sock.read(&mut tmp) else { return };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let text = String::from_utf8_lossy(&buf);
    let path = text
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string();
    let range = text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("range").then(|| {
            value
                .trim()
                .strip_prefix("bytes=")?
                .split('-')
                .next()?
                .parse::<usize>()
                .ok()
        })?
    });
    world.log.lock().unwrap().push(match range {
        Some(start) => format!("{path} range={start}"),
        None => path.clone(),
    });
    let Some(body) = file(&path, world) else {
        let _ = respond(&mut sock, "404 Not Found", "", b"missing");
        return;
    };
    if path.ends_with(".iso") {
        let mut hold = world.hold.lock().unwrap();
        while *hold {
            hold = world.cv.wait(hold).unwrap();
        }
        if range.is_none() && std::mem::replace(&mut *world.cut.lock().unwrap(), false) {
            let half = body.len() / 2;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(&body[..half]);
            let _ = sock.flush();
            return;
        }
    }
    let _ = match range {
        Some(start) if start >= body.len() => respond(
            &mut sock,
            "416 Range Not Satisfiable",
            &format!("Content-Range: bytes */{}\r\n", body.len()),
            b"",
        ),
        Some(start) => respond(
            &mut sock,
            "206 Partial Content",
            &format!(
                "Content-Range: bytes {start}-{}/{}\r\n",
                body.len() - 1,
                body.len()
            ),
            &body[start..],
        ),
        None => respond(&mut sock, "200 OK", "", &body),
    };
}

fn respond(sock: &mut TcpStream, status: &str, extra: &str, body: &[u8]) -> std::io::Result<()> {
    write!(sock, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n{extra}\r\n", body.len())?;
    sock.write_all(body)?;
    sock.flush()
}

fn dir() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "vmm-e2e-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn(base: &str, dir: &Path) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_virtual-machine-worker"))
        .args(["download", "--dir", dir.to_str().unwrap()])
        .env("VMM_BASE", base)
        .env("VMM_KEY_FP", FP.trim())
        .env_remove("VMM_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait(mut child: std::process::Child) -> (i32, Vec<Value>) {
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code().unwrap_or(1);
        }
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!("worker timed out");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    child.stdout.unwrap().read_to_string(&mut out).unwrap();
    (
        status,
        out.lines()
            .map(|line| serde_json::from_str(line).expect(line))
            .collect(),
    )
}

fn run(base: &str) -> (i32, Vec<Value>, PathBuf) {
    let dir = dir();
    let (status, events) = wait(spawn(base, &dir));
    (status, events, dir)
}

fn names(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| {
            e["event"] == "step"
                || e["event"] == "ready"
                || e["event"] == "detail"
                || e["event"] == "error"
        })
        .map(|e| {
            format!(
                "{} {} {}",
                e["event"].as_str().unwrap(),
                e["step"].as_str().unwrap_or(""),
                e["state"].as_str().unwrap_or("")
            )
        })
        .collect()
}

#[test]
fn quick_start_checks_the_signature_before_the_iso() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false);
    let (status, events, dir) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let log = world.log.lock().unwrap().clone();
    let iso = format!("/debian-cd/13.7.0/{}/iso-cd/{}", arch(), iso_name());
    assert_eq!(
        log,
        vec![
            "/CD/key-DA87E80D6294BE9B.txt".into(),
            format!("/debian-cd/current/{}/iso-cd/SHA512SUMS", arch()),
            format!("/debian-cd/current/{}/iso-cd/SHA512SUMS.sign", arch()),
            iso.clone(),
        ]
    );
    assert!(
        log.iter().position(|l| l.contains(".sign")).unwrap()
            < log.iter().position(|l| l == &iso).unwrap()
    );
    assert_eq!(
        names(&events),
        [
            "step key now",
            "step key done",
            "step sums now",
            "step sums done",
            "step sign now",
            "step sign done",
            "step check now",
            "step check done",
            "detail download ",
            "step download now",
            "step download done",
            "step verify now",
            "step verify done",
            "ready verify done",
        ]
    );
    assert!(events
        .iter()
        .any(|e| e["event"] == "progress" && e["step"] == "download"));
    assert!(events
        .iter()
        .any(|e| e["event"] == "progress" && e["step"] == "verify"));
    let cache = dir.join("cache");
    for file in [
        "key-DA87E80D6294BE9B.txt",
        "SHA512SUMS",
        "SHA512SUMS.sign",
        &iso_name(),
        "manifest.json",
        "download.lock",
    ] {
        assert!(cache.join(file).is_file(), "{file}");
    }
    assert!(!cache.join(format!("{}.partial", iso_name())).exists());
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(cache.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["file"], iso_name());
    assert_eq!(manifest["version"], "13.7.0");
    assert_eq!(manifest["arch"], arch());
    assert_eq!(manifest["signature"], "verified:SHA512SUMS.sign");
    assert_eq!(manifest["bytes"], ISO.len());
    assert_eq!(
        manifest["url"],
        format!(
            "https://cdimage.debian.org/debian-cd/13.7.0/{}/iso-cd/{}",
            arch(),
            iso_name()
        )
    );
    assert_eq!(std::fs::read(cache.join(iso_name())).unwrap(), ISO);
    assert_eq!(
        events
            .iter()
            .find(|e| e["step"] == "check" && e["state"] == "done")
            .unwrap()["note"],
        "Debian 13.7.0 signed SHA512SUMS."
    );
    assert_eq!(
        events
            .iter()
            .find(|e| e["step"] == "key" && e["state"] == "done")
            .unwrap()["note"],
        format!("Key id {}.", &FP.trim()[FP.trim().len() - 16..])
    );
    let progress: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "progress" && e["step"] == "download")
        .collect();
    assert!(progress.len() >= 2, "{progress:?}");
    assert_eq!(num(&progress[0]["received"]), Some(0.0));
    assert_eq!(
        num(&progress.last().unwrap()["received"]),
        Some(ISO.len() as f64)
    );
    let screen = finish(&events, status);
    assert!(screen.bar_visible());
    assert_eq!(screen.percent(), 100);
    assert_eq!(
        screen.bytes_label(),
        format!(
            "{} of {}",
            byte_count(ISO.len() as f64),
            byte_count(ISO.len() as f64)
        )
    );
    assert_eq!(screen.ready_path.as_deref(), Some(iso_path(&dir).as_str()));
    assert_eq!(screen.buttons(), ["Open cache folder"]);
    assert!(!screen.show_status());
    assert_eq!(screen.row("verify").detail, "Checksum matches");

    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    assert_eq!(
        world.log.lock().unwrap().len(),
        7,
        "cached iso was fetched again: {:?}",
        world.log.lock().unwrap()
    );
    assert!(events.iter().any(|e| e["event"] == "ready"));
    assert!(events
        .iter()
        .all(|e| !(e["event"] == "progress" && e["step"] == "download")));
    assert!(events
        .iter()
        .any(|e| e["event"] == "progress" && e["step"] == "verify"));
    let cached = finish(&events, status);
    assert!(!cached.bar_visible());
    assert_eq!(cached.received, 0.0);
    assert_eq!(cached.total, 0.0);
    assert_eq!(cached.bytes_label(), "");
    assert_eq!(cached.ready_path.as_deref(), Some(iso_path(&dir).as_str()));
    assert_eq!(cached.buttons(), ["Open cache folder"]);
    assert_eq!(cached.row("key").detail, "key-DA87E80D6294BE9B.txt");
    assert_eq!(cached.row("download").detail, iso_name());
    assert_eq!(cached.row("verify").state, "done");
}

#[test]
fn a_bad_signature_stops_before_the_iso() {
    let mut sums = SUMS.to_vec();
    sums[0] ^= 0xff;
    let (base, world) = serve(&sums, SIGN, ISO, KEY, false, false);
    let (status, events, dir) = run(&base);
    assert_ne!(status, 0);
    assert!(world
        .log
        .lock()
        .unwrap()
        .iter()
        .all(|line| !line.contains(".iso")));
    assert_eq!(
        events.iter().find(|e| e["event"] == "error").unwrap()["step"],
        "check"
    );
    assert!(!dir.join("cache").join(iso_name()).exists());
    let mut screen = finish(&events, status);
    let error = screen.error.clone().unwrap();
    assert!(!screen.bar_visible());
    assert_eq!(screen.headline, "Download failed");
    assert_eq!(screen.buttons(), ["Open cache folder", "Try again"]);
    assert_eq!(screen.row("check").state, "fail");
    assert_eq!(screen.row("check").detail, error);
    assert!(error.contains("SHA512SUMS was not signed"));
    assert_eq!(screen.row("key").state, "done");
    assert_eq!(screen.row("download").state, "wait");
    assert_eq!(screen.row("download").detail, iso_glob());
    let cache = dir.join("cache");
    assert!(!screen.cache_went_away(&cache));
    std::fs::remove_dir_all(&cache).unwrap();
    assert!(screen.cache_went_away(&cache));
    assert!(screen.steps.iter().all(|step| step.state == "wait"));
    assert_eq!(screen.buttons(), ["Open cache folder", "Cancel"]);
    let mut idle = Screen::new();
    idle.start();
    assert!(!idle.cache_went_away(Path::new("/no/such/cache")));
}

#[test]
fn a_bad_key_stops_before_the_checksum_file() {
    let (base, world) = serve(
        SUMS,
        SIGN,
        ISO,
        b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nnope\n",
        false,
        false,
    );
    let (status, events, _) = run(&base);
    assert_ne!(status, 0);
    assert_eq!(
        world.log.lock().unwrap().as_slice(),
        ["/CD/key-DA87E80D6294BE9B.txt"]
    );
    assert_eq!(
        events.iter().find(|e| e["event"] == "error").unwrap()["step"],
        "key"
    );
    let screen = finish(&events, status);
    let error = screen.error.clone().unwrap();
    assert!(!screen.bar_visible());
    assert_eq!(screen.row("key").state, "fail");
    assert_eq!(screen.row("key").detail, error);
    assert!(screen.steps.iter().skip(1).all(|step| step.state == "wait"));
    assert_eq!(screen.buttons(), ["Open cache folder", "Try again"]);
    assert_eq!(screen.headline, "Download failed");
}

#[test]
fn a_checksum_mismatch_removes_the_iso() {
    let mut iso = ISO.to_vec();
    iso[0] ^= 0xff;
    let (base, _) = serve(SUMS, SIGN, &iso, KEY, false, false);
    let (status, events, dir) = run(&base);
    assert_ne!(status, 0);
    let error = events.iter().find(|e| e["event"] == "error").unwrap();
    assert_eq!(error["step"], "verify");
    assert!(error["detail"]
        .as_str()
        .unwrap()
        .contains("did not match SHA512SUMS"));
    assert!(!dir.join("cache").join(iso_name()).exists());
    assert!(dir.join("cache/SHA512SUMS").is_file());
    assert!(!dir.join("cache/manifest.json").exists());
    let screen = finish(&events, status);
    let shown = screen.error.clone().unwrap();
    assert!(screen.bar_visible());
    assert_eq!(screen.percent(), 100);
    assert_eq!(screen.row("verify").state, "fail");
    assert_eq!(screen.row("verify").detail, shown);
    assert!(screen.ready_path.is_none());
    assert_eq!(screen.buttons(), ["Open cache folder", "Try again"]);
    assert_eq!(screen.headline, "Download failed");
}

#[test]
fn a_partial_download_resumes() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, true, false);
    let (status, events, dir) = run(&base);
    assert_ne!(status, 0);
    assert!(dir
        .join("cache")
        .join(format!("{}.partial", iso_name()))
        .is_file());
    let mut screen = finish(&events, status);
    let last = events
        .iter()
        .filter(|event| event["event"] == "progress" && event["step"] == "download")
        .last()
        .unwrap();
    assert!(screen.bar_visible());
    assert!(screen.received < screen.total);
    assert_eq!(screen.received, num(&last["received"]).unwrap());
    assert_eq!(screen.total, num(&last["total"]).unwrap());
    assert_eq!(screen.row("download").state, "fail");
    assert_eq!(screen.row("download").detail, screen.error.clone().unwrap());
    assert_eq!(screen.buttons(), ["Open cache folder", "Try again"]);
    screen.retry();
    assert!(!screen.bar_visible());
    assert_eq!(screen.received, 0.0);
    assert!(screen.steps.iter().all(|step| step.state == "wait"));
    assert_eq!(screen.buttons(), ["Open cache folder", "Cancel"]);
    assert_eq!(screen.headline, PREPARE);
    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    assert!(world
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("range=")));
    assert_eq!(
        std::fs::read(dir.join("cache").join(iso_name())).unwrap(),
        ISO
    );
    let screen = finish(&events, status);
    assert!(screen.bar_visible());
    assert_eq!(screen.percent(), 100);
    assert_eq!(screen.ready_path.as_deref(), Some(iso_path(&dir).as_str()));
    assert_eq!(screen.buttons(), ["Open cache folder"]);
}

#[test]
fn a_second_download_is_refused_while_the_lock_is_held() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, true);
    let dir = dir();
    let mut first = spawn(&base, &dir);
    let started = Instant::now();
    while !world
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("SHA512SUMS.sign"))
    {
        if started.elapsed() > Duration::from_secs(10) {
            let _ = first.kill();
            panic!("first download did not reach the signature");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let second = Command::new(env!("CARGO_BIN_EXE_virtual-machine-worker"))
        .args(["download", "--dir", dir.to_str().unwrap()])
        .env("VMM_BASE", &base)
        .env("VMM_KEY_FP", FP.trim())
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stdout).contains("a download is already running"));
    *world.hold.lock().unwrap() = false;
    world.cv.notify_all();
    assert!(first.wait().unwrap().success());
    let screen = finish(&parse_events(&second.stdout), 1);
    let error = screen.error.clone().unwrap();
    assert!(!screen.bar_visible());
    assert_eq!(screen.row("key").state, "fail");
    assert_eq!(screen.row("key").detail, "key-DA87E80D6294BE9B.txt");
    assert!(error.contains("a download is already running"));
    assert!(screen.steps.iter().skip(1).all(|step| step.state == "wait"));
    assert_eq!(screen.buttons(), ["Open cache folder", "Try again"]);
    assert_eq!(screen.headline, "Download failed");
    assert!(screen.show_status());
}

/// Quick start window. Same rules as Sources/QuickStart.swift.
struct Step {
    id: &'static str,
    title: &'static str,
    detail: String,
    state: String,
}

struct Screen {
    steps: Vec<Step>,
    received: f64,
    total: f64,
    headline: String,
    error: Option<String>,
    ready_path: Option<String>,
    cancelled: bool,
    downloading: bool,
    started: bool,
    running: bool,
}

const PREPARE: &str = "Preparing Debian 13. This only needs to happen once.";

fn iso_glob() -> &'static str {
    match arch() {
        "arm64" => "debian-*-arm64-netinst.iso",
        "amd64" => "debian-*-amd64-netinst.iso",
        _ => "debian-*-netinst.iso",
    }
}

fn byte_count(n: f64) -> String {
    let mb = 1024.0 * 1024.0;
    let gb = mb * 1024.0;
    if n >= gb {
        format!("{:.1} GB", n / gb)
    } else if n >= mb {
        format!("{:.0} MB", n / mb)
    } else {
        format!("{:.0} KB", n / 1024.0)
    }
}

fn num(value: &Value) -> Option<f64> {
    value.as_f64()
}

impl Screen {
    fn new() -> Self {
        Self {
            steps: vec![
                Step {
                    id: "key",
                    title: "Download OpenPGP keyring",
                    detail: "key-DA87E80D6294BE9B.txt".into(),
                    state: "wait".into(),
                },
                Step {
                    id: "sums",
                    title: "Download checksums file",
                    detail: "SHA512SUMS".into(),
                    state: "wait".into(),
                },
                Step {
                    id: "sign",
                    title: "Download signature file",
                    detail: "SHA512SUMS.sign".into(),
                    state: "wait".into(),
                },
                Step {
                    id: "check",
                    title: "Verify signature",
                    detail: "SHA512SUMS.sign".into(),
                    state: "wait".into(),
                },
                Step {
                    id: "download",
                    title: "Download ISO",
                    detail: iso_glob().into(),
                    state: "wait".into(),
                },
                Step {
                    id: "verify",
                    title: "Verify checksum",
                    detail: "SHA512SUMS".into(),
                    state: "wait".into(),
                },
            ],
            received: 0.0,
            total: 0.0,
            headline: PREPARE.into(),
            error: None,
            ready_path: None,
            cancelled: false,
            downloading: false,
            started: false,
            running: false,
        }
    }

    fn start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.cancelled = false;
        self.error = None;
        self.ready_path = None;
        self.running = true;
    }

    fn worker_missing(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.cancelled = false;
        self.error = None;
        self.ready_path = None;
        self.error =
            Some("The worker is not built. Run cargo build --release, then try again.".into());
        self.fail_current();
        self.headline = "Could not start".into();
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn retry(&mut self) {
        self.running = false;
        self.reset();
        self.start();
    }

    fn cancel(&mut self) {
        self.cancelled = true;
        self.running = false;
        self.error = None;
        self.headline = "Cancelled. The download was not saved.".into();
        if let Some(step) = self.steps.iter_mut().find(|step| step.state == "now") {
            step.state = "wait".into();
            step.detail = "Cancelled".into();
        }
    }

    fn worker_exited(&mut self, status: i32) {
        let current = self.running;
        self.running = false;
        if current
            && !self.cancelled
            && self.ready_path.is_none()
            && self.error.is_none()
            && status != 0
        {
            self.error = Some("The download stopped before the image was ready.".into());
            self.headline = "Download failed".into();
            self.fail_current();
        }
    }

    fn apply(&mut self, event: &Value) {
        if self.cancelled {
            return;
        }
        let kind = event["event"].as_str().unwrap_or("");
        let step = event["step"].as_str();
        if kind == "progress" && step == Some("download") {
            self.downloading = true;
            if let Some(n) = num(&event["received"]) {
                self.received = n;
            }
            if let Some(n) = num(&event["total"]).filter(|n| *n > 0.0) {
                self.total = n;
            }
        }
        match kind {
            "detail" => {
                if let (Some(id), Some(detail)) = (step, event["detail"].as_str()) {
                    if !detail.is_empty() {
                        if let Some(row) = self.steps.iter_mut().find(|row| row.id == id) {
                            row.detail = detail.to_string();
                        }
                    }
                }
            }
            "step" | "ready" => {
                if let (Some(id), Some(state)) = (step, event["state"].as_str()) {
                    self.apply_state(id, state, event["detail"].as_str());
                }
                if kind == "ready" {
                    self.ready_path = event["path"].as_str().map(str::to_string);
                    if self.downloading {
                        self.total = self.total.max(1.0);
                        self.received = self.total;
                    }
                }
            }
            "error" => {
                let detail = event["detail"].as_str();
                self.error = Some(detail.unwrap_or("Download failed.").to_string());
                self.headline = "Download failed".into();
                if let Some(id) = step {
                    if self.steps.iter().any(|row| row.id == id) {
                        self.apply_state(id, "fail", detail);
                        return;
                    }
                }
                self.fail_current();
            }
            _ => {}
        }
    }

    fn apply_state(&mut self, id: &str, state: &str, detail: Option<&str>) {
        let Some(row) = self.steps.iter_mut().find(|row| row.id == id) else {
            return;
        };
        if let Some(detail) = detail {
            if !detail.is_empty() {
                row.detail = detail.to_string();
            }
        }
        row.state = state.to_string();
    }

    fn fail_current(&mut self) {
        let index = self
            .steps
            .iter()
            .position(|step| step.state == "now")
            .or_else(|| self.steps.iter().position(|step| step.state == "wait"));
        if let Some(index) = index {
            self.steps[index].state = "fail".into();
        }
    }

    fn cache_went_away(&mut self, cache: &Path) -> bool {
        let finished =
            self.ready_path.is_some() || self.steps.iter().any(|step| step.state == "done");
        if !finished || (self.ready_path.is_none() && self.running) || !self.image_missing(cache) {
            return false;
        }
        self.running = false;
        self.reset();
        self.start();
        true
    }

    fn image_missing(&self, cache: &Path) -> bool {
        if let Some(path) = &self.ready_path {
            return !Path::new(path).is_file();
        }
        if !cache.is_dir() {
            return true;
        }
        let names = [
            "SHA512SUMS",
            "SHA512SUMS.sign",
            "manifest.json",
            "key-DA87E80D6294BE9B.txt",
        ];
        if names.iter().any(|name| cache.join(name).is_file()) {
            return false;
        }
        std::fs::read_dir(cache)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .all(|item| item.path().extension().and_then(|ext| ext.to_str()) != Some("iso"))
    }

    fn row(&self, id: &str) -> &Step {
        self.steps
            .iter()
            .find(|step| step.id == id)
            .unwrap_or_else(|| panic!("missing {id}"))
    }

    fn bar_visible(&self) -> bool {
        self.downloading && self.total > 0.0
    }

    fn show_status(&self) -> bool {
        self.error.is_some()
            || self.cancelled
            || (self.ready_path.is_none() && self.steps.iter().all(|step| step.state == "wait"))
    }

    fn buttons(&self) -> Vec<&'static str> {
        let mut buttons = vec!["Open cache folder"];
        if self.error.is_some() || self.cancelled {
            buttons.push("Try again");
        } else if self.ready_path.is_none() {
            buttons.push("Cancel");
        }
        buttons
    }

    fn bytes_label(&self) -> String {
        if self.total > 0.0 {
            format!(
                "{} of {}",
                byte_count(self.received),
                byte_count(self.total)
            )
        } else {
            String::new()
        }
    }

    fn percent(&self) -> i64 {
        let fraction = if self.total > 0.0 {
            (self.received / self.total).min(1.0)
        } else {
            0.0
        };
        (fraction * 100.0).round() as i64
    }
}

fn parse_events(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("bad event {line}")))
        .collect()
}

fn finish(events: &[Value], status: i32) -> Screen {
    let mut screen = drive(events);
    screen.worker_exited(status);
    screen
}

/// Walk the worker log the way the window does. A bar means ISO bytes, not a checksum.
fn drive(events: &[Value]) -> Screen {
    let mut screen = Screen::new();
    screen.start();
    let mut saw_download = false;
    for event in events {
        let kind = event["event"].as_str().unwrap_or("");
        let step = event["step"].as_str();
        let (received, total) = (screen.received, screen.total);
        screen.apply(event);
        if kind == "detail" && step == Some("download") {
            assert_eq!(screen.row("download").detail, iso_name());
            assert_eq!(screen.row("download").state, "wait");
            assert!(!screen.bar_visible());
        } else if kind == "progress" && step == Some("download") {
            saw_download = true;
        } else if kind == "progress" {
            assert_eq!((screen.received, screen.total), (received, total));
        } else if kind == "ready" && !saw_download {
            assert_eq!(screen.received, 0.0);
            assert_eq!(screen.total, 0.0);
        }
        if kind == "step" && event["state"] == "now" {
            assert!(!screen.show_status());
        }
        assert_eq!(screen.downloading, saw_download);
        assert_eq!(screen.bar_visible(), saw_download && screen.total > 0.0);
    }
    screen
}

fn iso_path(dir: &Path) -> String {
    dir.join("cache").join(iso_name()).display().to_string()
}

fn watch(
    base: &str,
    dir: &Path,
) -> (
    std::process::Child,
    Arc<Mutex<Vec<Value>>>,
    thread::JoinHandle<()>,
) {
    let mut child = spawn(base, dir);
    let stdout = child.stdout.take().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let reader = thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.is_empty() {
                continue;
            }
            sink.lock()
                .unwrap()
                .push(serde_json::from_str(&line).expect(&line));
        }
    });
    (child, events, reader)
}

fn until(events: &Mutex<Vec<Value>>, pred: impl Fn(&Value) -> bool) -> Vec<Value> {
    let started = Instant::now();
    loop {
        let got = events.lock().unwrap().clone();
        if got.iter().any(&pred) {
            return got;
        }
        if started.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for the window event: {got:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn cancel_stops_the_window_and_ignores_later_events() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, true);
    let dir = dir();
    let (mut child, events, reader) = watch(&base, &dir);
    let before = until(&events, |event| {
        event["step"] == "download" && event["state"] == "now"
    });
    let mut screen = drive(&before);
    assert_eq!(screen.row("download").state, "now");
    assert!(!screen.bar_visible());
    screen.cancel();
    let _ = child.kill();
    let status = child.wait().unwrap().code().unwrap_or(1);
    reader.join().unwrap();
    *world.hold.lock().unwrap() = false;
    world.cv.notify_all();
    let after = events.lock().unwrap().clone();
    let frozen: Vec<_> = screen
        .steps
        .iter()
        .map(|step| (step.state.clone(), step.detail.clone()))
        .collect();
    let headline = screen.headline.clone();
    let received = screen.received;
    for event in after.iter().skip(before.len()) {
        screen.apply(event);
    }
    screen.worker_exited(status);
    let now: Vec<_> = screen
        .steps
        .iter()
        .map(|step| (step.state.clone(), step.detail.clone()))
        .collect();
    assert_eq!(now, frozen, "events after cancel must not paint");
    assert_eq!(screen.headline, headline);
    assert_eq!(screen.received, received);
    assert!(screen.error.is_none());
    assert_eq!(screen.headline, "Cancelled. The download was not saved.");
    assert_eq!(screen.row("download").state, "wait");
    assert_eq!(screen.row("download").detail, "Cancelled");
    assert_eq!(screen.row("check").state, "done");
    assert_eq!(screen.buttons(), ["Open cache folder", "Try again"]);
    assert!(screen.show_status());
    assert!(!screen.bar_visible());
    screen.retry();
    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    let screen = finish(&events, status);
    assert!(screen.bar_visible());
    assert_eq!(screen.ready_path.as_deref(), Some(iso_path(&dir).as_str()));
}

#[test]
fn a_killed_worker_fails_the_step_that_was_running() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, true);
    let dir = dir();
    let (mut child, events, reader) = watch(&base, &dir);
    let seen = until(&events, |event| {
        event["step"] == "download" && event["state"] == "now"
    });
    let mut screen = drive(&seen);
    let _ = child.kill();
    let status = child.wait().unwrap().code().unwrap_or(1);
    reader.join().unwrap();
    *world.hold.lock().unwrap() = false;
    world.cv.notify_all();
    screen.worker_exited(status);
    assert_eq!(
        screen.error.as_deref(),
        Some("The download stopped before the image was ready.")
    );
    assert_eq!(screen.headline, "Download failed");
    assert_eq!(screen.row("download").state, "fail");
    assert_eq!(screen.row("download").detail, iso_name());
    assert!(!screen.bar_visible());
    assert!(screen.ready_path.is_none());
    assert_eq!(screen.buttons(), ["Open cache folder", "Try again"]);
    assert!(!screen.cancelled);
}

#[test]
fn deleting_the_finished_iso_clears_the_window_and_starts_again() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false);
    let (status, events, dir) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let cache = dir.join("cache");
    let iso = cache.join(iso_name());
    let mut screen = finish(&events, status);
    assert!(!screen.cache_went_away(&cache));
    assert!(screen.ready_path.is_some());
    std::fs::remove_file(cache.join("SHA512SUMS")).unwrap();
    assert!(
        !screen.cache_went_away(&cache),
        "the finished path is the ISO, not the checksum file"
    );
    std::fs::remove_file(&iso).unwrap();
    assert!(screen.cache_went_away(&cache));
    assert!(screen.steps.iter().all(|step| step.state == "wait"));
    assert!(!screen.bar_visible());
    assert!(screen.ready_path.is_none());
    assert_eq!(screen.headline, PREPARE);
    assert_eq!(screen.buttons(), ["Open cache folder", "Cancel"]);
    assert!(screen.running);
    let fetched = world.log.lock().unwrap().len();
    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    assert!(world
        .log
        .lock()
        .unwrap()
        .iter()
        .skip(fetched)
        .any(|line| line.contains(".iso")));
    let screen = finish(&events, status);
    assert!(screen.bar_visible());
    assert_eq!(screen.percent(), 100);
    assert_eq!(
        screen.ready_path.as_deref(),
        Some(iso.display().to_string()).as_deref()
    );
    assert_eq!(screen.buttons(), ["Open cache folder"]);

    let mut running = drive(
        &events[..events
            .iter()
            .position(|event| event["step"] == "download" && event["state"] == "now")
            .unwrap()
            + 1],
    );
    assert!(running.running && running.ready_path.is_none());
    std::fs::remove_dir_all(&cache).unwrap();
    assert!(
        !running.cache_went_away(&cache),
        "do not clear ticks while the download is still running"
    );
    assert!(running.steps.iter().any(|step| step.state == "done"));
}

#[test]
fn the_window_ignores_checksum_bytes_and_formats_its_own() {
    let mut screen = Screen::new();
    assert!(!screen.bar_visible());
    assert!(screen.show_status());
    assert_eq!(screen.headline, PREPARE);
    assert_eq!(screen.buttons(), ["Open cache folder", "Cancel"]);
    assert_eq!(screen.row("key").title, "Download OpenPGP keyring");
    assert_eq!(screen.row("download").detail, iso_glob());
    screen.start();
    screen.apply(&serde_json::json!({"event": "progress", "step": "verify", "received": 5_000_000_000u64, "total": 5_000_000_000u64}));
    assert!(!screen.bar_visible());
    assert_eq!(screen.received, 0.0);
    screen.apply(&serde_json::json!({"event": "ready", "step": "verify", "state": "done", "received": 99, "total": 99, "path": "/tmp/cached.iso"}));
    assert!(!screen.bar_visible());
    assert_eq!(screen.received, 0.0);
    assert_eq!(screen.total, 0.0);
    assert_eq!(screen.ready_path.as_deref(), Some("/tmp/cached.iso"));

    screen.apply(&serde_json::json!({"event": "progress", "step": "download", "detail": "0 KiB", "received": 0, "total": 0}));
    assert!(screen.downloading);
    assert!(!screen.bar_visible());
    screen.apply(&serde_json::json!({"event": "progress", "step": "download", "detail": "1 KiB of 1.5 GiB", "received": 1024, "total": 1_610_612_736u64}));
    assert_eq!(screen.bytes_label(), "1 KB of 1.5 GB");
    assert_eq!(screen.percent(), 0);
    assert!(screen.bar_visible());
    screen.apply(&serde_json::json!({"event": "nope", "received": 10, "total": 10}));
    assert_eq!(screen.bytes_label(), "1 KB of 1.5 GB");
    screen.cancel();
    screen.apply(
        &serde_json::json!({"event": "error", "step": "download", "detail": "should be ignored"}),
    );
    assert!(screen.error.is_none());
    assert!(
        screen.bar_visible(),
        "cancel does not wipe a bar that was already on screen"
    );
    assert_eq!(screen.buttons(), ["Open cache folder", "Try again"]);

    let mut missing = Screen::new();
    missing.worker_missing();
    assert_eq!(
        missing.error.as_deref(),
        Some("The worker is not built. Run cargo build --release, then try again.")
    );
    assert_eq!(missing.headline, "Could not start");
    assert_eq!(missing.row("key").state, "fail");
    assert_eq!(missing.buttons(), ["Open cache folder", "Try again"]);
    assert!(!missing.bar_visible());
    missing.worker_missing();
    assert_eq!(
        missing.row("key").state,
        "fail",
        "a second start does not run again"
    );
}

#[test]
fn quick_start_swift_gates_the_bar_on_a_real_download() {
    let src = include_str!("../Sources/QuickStart.swift");
    assert!(src.contains("if event.event == \"progress\", event.step == \"download\""));
    assert!(src.contains("if wizard.downloading && wizard.total > 0"));
    assert!(src.contains("downloading = false"));
    let apply = src.split("private func apply(").nth(1).unwrap();
    let ready = apply.split("if event.event == \"ready\"").nth(1).unwrap();
    assert!(ready
        .split("case \"error\"")
        .next()
        .unwrap()
        .contains("if downloading {"));
}
