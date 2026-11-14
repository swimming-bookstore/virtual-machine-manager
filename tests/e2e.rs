//! End-to-end worker run. A local server stands in for Debian.
//! The signature is checked before the ISO is fetched.
//! Quick start is replayed from that JSON so the window stays tied to it.

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
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

/// Test side of the local Debian mirror. The server thread owns the other end of each channel.
struct World {
    log_rx: Receiver<String>,
    /// Each ISO request sends its release handle here. Send `()` on it to continue.
    go_rx: Receiver<Sender<()>>,
    /// Byte limit for a paced ISO body. `None` finishes it.
    pace_tx: Sender<Option<usize>>,
    wake: Sender<()>,
}

fn file(path: &str, files: &[(&'static str, Vec<u8>)]) -> Option<Vec<u8>> {
    files
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, b)| b.clone())
}

fn log_of(world: &World) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(line) = world.log_rx.try_recv() {
        lines.push(line);
    }
    lines
}

/// Let a held ISO response finish. No-op when this server was not asked to hold.
fn release(world: &World) {
    if let Ok(go) = world.go_rx.recv_timeout(Duration::from_secs(10)) {
        let _ = go.send(());
    }
}

/// Stop the next ISO body at `n` bytes. Call this before the request.
fn pause_at(world: &World, n: usize) {
    world.pace_tx.send(Some(n)).unwrap();
}

fn more(world: &World, n: Option<usize>) {
    world.pace_tx.send(n).unwrap();
    let _ = world.wake.send(());
}

fn serve(
    sums: &[u8],
    sign: &[u8],
    iso: &[u8],
    key: &[u8],
    cut_once: bool,
    hold: bool,
    pace: bool,
) -> (String, World) {
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
    let (log_tx, log_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let (pace_tx, pace_rx) = mpsc::channel();
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    let world = World {
        log_rx,
        go_rx,
        pace_tx,
        wake: wake_tx.clone(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        let (cut_tx, cut_rx) = mpsc::channel();
        if cut_once {
            let _ = cut_tx.send(());
        }
        let mut waiting: Option<Sender<Option<usize>>> = None;
        let mut queued: Option<Option<usize>> = None;
        let (conn_tx, conn_rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(conn) = listener.accept() {
                if conn_tx.send(conn).is_err() {
                    break;
                }
            }
        });
        loop {
            if pace {
                while let Ok(limit) = pace_rx.try_recv() {
                    if let Some(tx) = waiting.as_ref() {
                        let _ = tx.send(limit);
                    } else {
                        queued = Some(limit);
                    }
                }
            }
            match conn_rx.try_recv() {
                Ok((mut conn, _)) => {
                    let _ = conn.set_nonblocking(false);
                    let log_tx = log_tx.clone();
                    let files = files.clone();
                    let go_tx = go_tx.clone();
                    let head = read_head(&mut conn);
                    let path = request_path(&head);
                    let range = request_range(&head);
                    let _ = log_tx.send(match range {
                        Some(start) => format!("{path} range={start}"),
                        None => path.clone(),
                    });
                    let cut = path.ends_with(".iso") && cut_rx.try_recv().is_ok();
                    let (release_tx, release_rx) = mpsc::channel();
                    if path.ends_with(".iso") {
                        let _ = go_tx.send(release_tx);
                    }
                    let (limit_tx, limit_rx) = mpsc::channel();
                    let (next_tx, next_rx) = mpsc::channel();
                    if pace && path.ends_with(".iso") {
                        if let Some(limit) = queued.take() {
                            let _ = limit_tx.send(limit);
                        }
                        waiting = Some(next_tx);
                    }
                    thread::spawn(move || {
                        handle(
                            conn,
                            Serve {
                                files,
                                hold,
                                path,
                                range,
                                cut,
                                release: release_rx,
                                limits: limit_rx,
                                next: next_rx,
                            },
                        )
                    });
                }
                Err(mpsc::TryRecvError::Empty) => {
                    let _ = wake_rx.recv_timeout(Duration::from_millis(50));
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
    });
    (base, world)
}

fn read_head(sock: &mut TcpStream) -> String {
    let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 8192 {
        let Ok(n) = sock.read(&mut tmp) else { break };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn request_path(head: &str) -> String {
    head.split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string()
}

fn request_range(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
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
    })
}

struct Serve {
    files: Vec<(&'static str, Vec<u8>)>,
    hold: bool,
    path: String,
    range: Option<usize>,
    cut: bool,
    release: Receiver<()>,
    limits: Receiver<Option<usize>>,
    next: Receiver<Option<usize>>,
}

fn handle(mut sock: TcpStream, job: Serve) {
    let Serve {
        files,
        hold,
        path,
        range,
        cut,
        release,
        limits,
        next,
    } = job;
    let Some(body) = file(&path, &files) else {
        let _ = respond(&mut sock, "404 Not Found", "", b"missing");
        return;
    };
    if path.ends_with(".iso") {
        if hold {
            let _ = release.recv();
        }
        if range.is_none() && cut {
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
        if range.is_none() {
            if let Ok(limit) = limits.try_recv() {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.flush();
                let mut sent = 0usize;
                let mut limit = limit;
                loop {
                    let target = limit.map(|n| n.min(body.len())).unwrap_or(body.len());
                    if target > sent {
                        let _ = sock.write_all(&body[sent..target]);
                        let _ = sock.flush();
                        sent = target;
                        if sent == body.len() {
                            return;
                        }
                    }
                    limit = match next.recv() {
                        Ok(next) => next,
                        Err(_) => return,
                    };
                }
            }
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

fn cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_virtual-machine-worker"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn clap_parses_download_and_rejects_unknown_args() {
    let help = cli(&["download", "--help"]);
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("--dir"));

    let unknown = cli(&["download", "--nope"]);
    assert!(!unknown.status.success());
    assert!(unknown.stdout.is_empty());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unexpected argument"));

    let snapshot = cli(&["snapshot", "--help"]);
    assert!(
        snapshot.status.success(),
        "{}",
        String::from_utf8_lossy(&snapshot.stderr)
    );
    assert!(String::from_utf8_lossy(&snapshot.stdout).contains("--dir"));
    let clone = cli(&["clone", "--help"]);
    assert!(
        clone.status.success(),
        "{}",
        String::from_utf8_lossy(&clone.stderr)
    );
    assert!(String::from_utf8_lossy(&clone.stdout).contains("--name"));
}

fn spawn(base: &str, dir: &Path) -> std::process::Child {
    spawn_from(base, dir, None)
}

fn spawn_from(base: &str, dir: &Path, from: Option<&str>) -> std::process::Child {
    let mut args = vec!["download", "--dir", dir.to_str().unwrap()];
    if let Some(from) = from {
        args.push("--from");
        args.push(from);
    }
    Command::new(env!("CARGO_BIN_EXE_virtual-machine-worker"))
        .args(args)
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
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, events, dir) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let log = log_of(&world);
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
            "step sshkey now",
            "step sshkey done",
            "step preseed now",
            "step preseed done",
            "step disk now",
            "step disk done",
            "step patch now",
            "step patch done",
            "ready patch done",
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
    let progress: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "progress" && e["step"] == "download")
        .collect();
    assert!(progress.len() >= 2, "{progress:?}");
    assert_eq!(num(&progress[0]["received"]), Some(0.0));
    assert_eq!(num(&progress[0]["total"]), Some(0.0));
    assert_eq!(
        num(&progress.last().unwrap()["received"]),
        Some(ISO.len() as f64)
    );
    let download: Vec<_> = events
        .iter()
        .take_while(|event| event["step"] != "sshkey")
        .cloned()
        .collect();
    let download = drive(&download);
    assert_eq!(download.page, 0);
    assert_eq!(download.steps.len(), 14);
    assert!(download.steps.iter().any(|step| step.id == "sshkey"));
    assert_eq!(download.page_rows(), ["download", "partition", "snapshot"]);
    let mut screen = finish(&events, status);
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
    assert_eq!(screen.row("download").detail, iso_path(&dir));
    assert_eq!(
        screen.row("snapshot").detail,
        dir.join("snapshots/debian").display().to_string()
    );
    assert_eq!(
        screen.disk_path.as_deref(),
        Some(dir.join("machines/debian/disk.img").to_str().unwrap())
    );
    assert_eq!(screen.page, 0, "the wizard is one page");
    assert_eq!(screen.title(), "Debian 13");
    assert_eq!(screen.steps.len(), 14);
    assert_eq!(screen.page_rows(), ["download", "partition", "snapshot"]);
    assert_eq!(screen.buttons(), ["Open folder"]);
    screen.show_all = true;
    assert_eq!(
        screen.page_rows(),
        [
            "key",
            "sums",
            "sign",
            "check",
            "download",
            "verify",
            "sshkey",
            "preseed",
            "disk",
            "patch",
            "installer",
            "partition",
            "ssh",
            "snapshot"
        ]
    );
    let patch_now = events
        .iter()
        .position(|event| {
            event["event"] == "step" && event["step"] == "patch" && event["state"] == "now"
        })
        .unwrap();
    let mut mid = drive(&events[..=patch_now]);
    assert_eq!(mid.page, 0);
    assert_eq!(mid.title(), "Debian 13");
    assert_eq!(mid.visible_ids(), ["download", "partition", "snapshot"]);
    assert_eq!(mid.row("patch").state, "now");
    assert_eq!(mid.buttons(), ["Open folder", "Cancel"]);
    mid.show_all = true;
    assert_eq!(
        mid.visible_ids(),
        [
            "key",
            "sums",
            "sign",
            "check",
            "download",
            "verify",
            "sshkey",
            "preseed",
            "disk",
            "patch",
            "installer",
            "partition",
            "ssh",
            "snapshot"
        ]
    );
    for event in &events[patch_now + 1..] {
        mid.apply(event);
    }
    assert_eq!(mid.page, 0, "ready does not open another page");
    assert!(mid.installing());
    assert_eq!(mid.buttons(), ["Open folder"]);
    assert_eq!(mid.row("patch").detail, "boot.iso");
    assert_eq!(
        mid.page_rows(),
        [
            "key",
            "sums",
            "sign",
            "check",
            "download",
            "verify",
            "sshkey",
            "preseed",
            "disk",
            "patch",
            "installer",
            "partition",
            "ssh",
            "snapshot"
        ]
    );
    mid.show_all = false;
    assert_eq!(mid.page_rows(), ["download", "partition", "snapshot"]);
    assert_eq!(screen.row("download").state, "done");
    assert_eq!(screen.row("verify").state, "done");
    assert_eq!(screen.row("preseed").state, "done");
    assert_eq!(screen.row("patch").state, "done");
    assert_eq!(screen.row("patch").detail, "boot.iso");
    assert_eq!(screen.row("disk").state, "done");
    assert_eq!(screen.row("disk").detail, "disk.img");
    assert!(screen.installing());
    assert_eq!(screen.page, 0);
    let setup = open_setup(&screen, &dir);
    assert_eq!(setup.title, "Debian setup");
    assert!(setup.shows_installer());
    assert_eq!(setup.buttons(), ["Open folder", "Back", "Show installer"]);
    assert_eq!(setup.row("installer").state, "now");
    assert!(!setup.captures_pointer());
    assert!(dir.join("installer/preseed.img").is_file());
    assert!(dir.join("installer/boot.iso").is_file());
    assert!(!dir.join("preseed.img").exists());
    assert!(!dir.join("boot.iso").exists());
    assert!(!dir.join("cache/preseed.img").exists());
    assert!(!dir.join("cache/boot.iso").exists());
    assert_eq!(
        setup.attached,
        vec![
            dir.join("installer/boot.iso").display().to_string(),
            dir.join("installer/preseed.img").display().to_string(),
        ],
        "setup attaches the copies in installer/, not the signed ISO"
    );
    assert_eq!(
        std::fs::metadata(dir.join("machines/debian/disk.img"))
            .unwrap()
            .len(),
        16 * 1024 * 1024 * 1024
    );
    let key = dir.join("ssh/authorized_keys");
    assert!(key.is_file(), "host key was not generated");
    assert!(std::fs::read_to_string(&key)
        .unwrap()
        .contains("ssh-ed25519"));
    assert!(!dir.join("machines/debian/marker.img").exists());
    let ready = events.iter().find(|e| e["event"] == "ready").unwrap();
    assert_eq!(ready["key"], key.display().to_string());
    assert_eq!(screen.row("preseed").state, "done");

    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    let log = log_of(&world);
    assert_eq!(log.len(), 3, "cached iso was fetched again: {log:?}");
    assert!(events.iter().any(|e| e["event"] == "ready"));
    assert!(events
        .iter()
        .all(|e| !(e["event"] == "progress" && e["step"] == "download")));
    assert!(events
        .iter()
        .any(|e| e["event"] == "progress" && e["step"] == "verify"));
    let mut cached = finish(&events, status);
    assert!(!cached.bar_visible());
    assert_eq!(cached.received, 0.0);
    assert_eq!(cached.total, 0.0);
    assert_eq!(cached.bytes_label(), "");
    assert_eq!(cached.ready_path.as_deref(), Some(iso_path(&dir).as_str()));
    assert_eq!(cached.row("download").detail, iso_path(&dir));
    assert_eq!(
        cached.row("snapshot").detail,
        dir.join("snapshots/debian").display().to_string()
    );
    assert!(cached.installing());
    assert_eq!(cached.page, 0);
    assert_eq!(cached.row("preseed").state, "done");
    assert_eq!(cached.row("sshkey").detail, "id_ed25519");
    assert_eq!(cached.row("disk").detail, "disk.img");
    assert_eq!(cached.row("patch").detail, "boot.iso");
    assert_eq!(cached.buttons(), ["Open folder"]);
    assert_eq!(cached.page_rows(), ["download", "partition", "snapshot"]);
    cached.show_all = true;
    assert!(cached
        .page_rows()
        .starts_with(&["key", "sums", "sign", "check", "download"]));
    let setup = open_setup(&cached, &dir);
    assert!(setup.shows_installer());
    assert_eq!(setup.row("installer").state, "now");
    assert_eq!(
        setup.attached,
        vec![
            dir.join("installer/boot.iso").display().to_string(),
            dir.join("installer/preseed.img").display().to_string(),
        ]
    );
    assert!(setup.attached.iter().all(|path| !path.contains("/cache/")));
}

#[test]
fn a_bad_signature_stops_before_the_iso() {
    let mut sums = SUMS.to_vec();
    sums[0] ^= 0xff;
    let (base, world) = serve(&sums, SIGN, ISO, KEY, false, false, false);
    let (status, events, dir) = run(&base);
    assert_ne!(status, 0);
    assert!(log_of(&world).iter().all(|line| !line.contains(".iso")));
    assert_eq!(
        events.iter().find(|e| e["event"] == "error").unwrap()["step"],
        "check"
    );
    assert!(!dir.join("cache").join(iso_name()).exists());
    let mut screen = finish(&events, status);
    let error = screen.error.clone().unwrap();
    assert!(!screen.bar_visible());
    assert_eq!(screen.headline.as_deref(), Some("Download failed"));
    assert_eq!(screen.buttons(), ["Open folder", "Try again"]);
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
    assert_eq!(screen.resume_from.as_deref(), Some("key"));
    assert_eq!(screen.row("key").state, "now");
    assert!(screen.steps.iter().skip(1).all(|step| step.state == "wait"));
    assert_eq!(screen.buttons(), ["Open folder", "Cancel"]);
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
        false,
    );
    let (status, events, _) = run(&base);
    assert_ne!(status, 0);
    assert_eq!(log_of(&world), ["/CD/key-DA87E80D6294BE9B.txt"]);
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
    assert_eq!(screen.buttons(), ["Open folder", "Try again"]);
    assert_eq!(screen.headline.as_deref(), Some("Download failed"));
}

#[test]
fn a_checksum_mismatch_removes_the_iso() {
    let mut iso = ISO.to_vec();
    iso[0] ^= 0xff;
    let (base, _) = serve(SUMS, SIGN, &iso, KEY, false, false, false);
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
    assert_eq!(screen.buttons(), ["Open folder", "Try again"]);
    assert_eq!(screen.headline.as_deref(), Some("Download failed"));
}

#[test]
fn a_partial_download_resumes() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, true, false, false);
    let (status, events, dir) = run(&base);
    assert_ne!(status, 0, "{events:?} log={:?}", log_of(&world));
    assert!(dir
        .join("cache")
        .join(format!("{}.partial", iso_name()))
        .is_file());
    let mut screen = finish(&events, status);
    let last = events
        .iter()
        .rfind(|event| event["event"] == "progress" && event["step"] == "download")
        .unwrap();
    assert!(screen.bar_visible());
    assert!(screen.received < screen.total);
    assert_eq!(screen.received, num(&last["received"]).unwrap());
    assert_eq!(screen.total, num(&last["total"]).unwrap());
    assert_eq!(screen.row("download").state, "fail");
    assert_eq!(screen.row("download").detail, screen.error.clone().unwrap());
    assert_eq!(screen.buttons(), ["Open folder", "Try again"]);
    screen.retry();
    assert!(!screen.bar_visible());
    assert_eq!(screen.received, 0.0);
    assert!(screen.steps.iter().all(|step| step.state == "wait"));
    assert_eq!(screen.buttons(), ["Open folder", "Cancel"]);
    assert!(screen.headline.is_none());
    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    assert!(log_of(&world).iter().any(|line| line.contains("range=")));
    assert_eq!(
        std::fs::read(dir.join("cache").join(iso_name())).unwrap(),
        ISO
    );
    let screen = finish(&events, status);
    assert!(screen.bar_visible());
    assert_eq!(screen.percent(), 100);
    assert_eq!(screen.ready_path.as_deref(), Some(iso_path(&dir).as_str()));
    assert!(screen.installing());
}

#[test]
fn a_second_download_is_refused_while_the_lock_is_held() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, true, false);
    let dir = dir();
    let mut first = spawn(&base, &dir);
    let started = Instant::now();
    while !log_of(&world)
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
    release(&world);
    assert!(first.wait().unwrap().success());
    let screen = finish(&parse_events(&second.stdout), 1);
    let error = screen.error.clone().unwrap();
    assert!(!screen.bar_visible());
    assert_eq!(screen.row("key").state, "fail");
    assert_eq!(screen.row("key").detail, "key-DA87E80D6294BE9B.txt");
    assert!(error.contains("a download is already running"));
    assert!(screen.steps.iter().skip(1).all(|step| step.state == "wait"));
    assert_eq!(screen.buttons(), ["Open folder", "Try again"]);
    assert_eq!(screen.headline.as_deref(), Some("Download failed"));
}

/// Quick start window. Same rules as Sources/QuickStart.swift.
#[derive(Debug)]
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
    headline: Option<String>,
    error: Option<String>,
    ready_path: Option<String>,
    disk_path: Option<String>,
    iso_path: Option<String>,
    resume_from: Option<String>,
    missing_file: Option<String>,
    cancelled: bool,
    downloading: bool,
    started: bool,
    running: bool,
    page: i32,
    show_all: bool,
}

fn default_detail(id: &str) -> String {
    match id {
        "key" => "key-DA87E80D6294BE9B.txt".into(),
        "sums" | "verify" => "SHA512SUMS".into(),
        "sign" | "check" => "SHA512SUMS.sign".into(),
        "download" => iso_glob().into(),
        "sshkey" => "id_ed25519".into(),
        "preseed" => "preseed.img".into(),
        "disk" => "disk.img".into(),
        "patch" | "installer" => "boot.iso".into(),
        "snapshot" => "snapshots/debian".into(),
        _ => String::new(),
    }
}

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
                Step {
                    id: "sshkey",
                    title: "Prepare the host key",
                    detail: "id_ed25519".into(),
                    state: "wait".into(),
                },
                Step {
                    id: "preseed",
                    title: "Prepare preseed disk",
                    detail: "preseed.img".into(),
                    state: "wait".into(),
                },
                Step {
                    id: "disk",
                    title: "Prepare the guest disk",
                    detail: "disk.img".into(),
                    state: "wait".into(),
                },
                Step {
                    id: "patch",
                    title: "Prepare installer",
                    detail: "boot.iso".into(),
                    state: "wait".into(),
                },
                Step {
                    id: "installer",
                    title: "Start the installer",
                    detail: "boot.iso".into(),
                    state: "wait".into(),
                },
                Step {
                    id: "partition",
                    title: "Install guest",
                    detail: "disk.img".into(),
                    state: "wait".into(),
                },
                Step {
                    id: "ssh",
                    title: "Log in with the host key",
                    detail: "guest port 22".into(),
                    state: "wait".into(),
                },
                Step {
                    id: "snapshot",
                    title: "Save a snapshot",
                    detail: "snapshots/debian".into(),
                    state: "wait".into(),
                },
            ],
            received: 0.0,
            total: 0.0,
            headline: None,
            error: None,
            ready_path: None,
            disk_path: None,
            iso_path: None,
            resume_from: None,
            missing_file: None,
            cancelled: false,
            downloading: false,
            started: false,
            running: false,
            page: 0,
            show_all: false,
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
        self.headline = Some("Could not start".into());
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
        self.headline = Some("Cancelled. The download was not saved.".into());
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
            self.headline = Some("Download failed".into());
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
                    self.disk_path = event["disk"].as_str().map(str::to_string);
                    if let Some(path) = event["path"].as_str() {
                        self.iso_path = Some(path.to_string());
                        let state = self
                            .steps
                            .iter()
                            .find(|row| row.id == "download")
                            .map(|row| row.state.clone())
                            .unwrap_or_else(|| "done".into());
                        self.apply_state("download", &state, Some(path));
                    }
                    self.show_snapshot_location();
                    if self.downloading {
                        self.total = self.total.max(1.0);
                        self.received = self.total;
                    }
                }
            }
            "error" => {
                let detail = event["detail"].as_str();
                self.error = Some(detail.unwrap_or("Download failed.").to_string());
                self.headline = Some("Download failed".into());
                if let Some(id) = step {
                    if self.steps.iter().any(|row| row.id == id) {
                        self.apply_state(id, "fail", detail);
                    } else {
                        self.fail_current();
                    }
                } else {
                    self.fail_current();
                }
            }
            _ => {}
        }
    }

    fn title(&self) -> &'static str {
        "Debian 13"
    }

    fn page_rows(&self) -> Vec<&'static str> {
        self.visible_ids()
    }

    /// Download ISO, Install guest, and Save a snapshot, unless Show all steps is on.
    fn visible_ids(&self) -> Vec<&'static str> {
        const BRIEF: &[&str] = &["download", "partition", "snapshot"];
        const ORDER: &[&str] = &[
            "key",
            "sums",
            "sign",
            "check",
            "download",
            "verify",
            "sshkey",
            "preseed",
            "disk",
            "patch",
            "installer",
            "partition",
            "ssh",
            "snapshot",
        ];
        let ids = if self.show_all { ORDER } else { BRIEF };
        ids.iter()
            .copied()
            .filter(|id| self.steps.iter().any(|step| step.id == *id))
            .collect()
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

    /// The worker calls the saved copy a snapshot. The directory is snapshots/debian beside the cache.
    fn show_snapshot_location(&mut self) {
        let Some(iso) = self.iso_path.as_deref() else {
            return;
        };
        let support = Path::new(iso)
            .parent()
            .and_then(|cache| cache.parent())
            .map(|dir| dir.join("snapshots/debian"));
        let Some(path) = support else { return };
        if let Some(row) = self.steps.iter_mut().find(|step| step.id == "snapshot") {
            row.detail = path.display().to_string();
        }
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
        if self.cancelled || self.running {
            return false;
        }
        if let Some(missing) = self.missing_file.clone() {
            let done = self
                .steps
                .iter()
                .any(|step| step.id == missing && step.state == "done");
            if done {
                self.missing_file = None;
                self.resume_id(&missing);
                return true;
            }
        }
        let Some(missing) = self.first_missing(cache) else {
            return false;
        };
        self.resume(missing);
        true
    }

    /// The first finished step whose file is gone.
    fn first_missing(&self, cache: &Path) -> Option<&'static str> {
        const ORDER: [&str; 10] = [
            "key", "sums", "sign", "check", "download", "verify", "sshkey", "preseed", "disk",
            "patch",
        ];
        for id in ORDER {
            let Some(step) = self.steps.iter().find(|step| step.id == id) else {
                continue;
            };
            if step.state != "done" {
                continue;
            }
            if !self.artifact_ok(id, cache) {
                return Some(id);
            }
        }
        None
    }

    fn artifact_ok(&self, id: &str, cache: &Path) -> bool {
        let support = cache.parent().unwrap();
        let has = |path: &Path| path.is_file();
        match id {
            "key" => has(&cache.join("key-DA87E80D6294BE9B.txt")),
            "sums" => has(&cache.join("SHA512SUMS")),
            "sign" => has(&cache.join("SHA512SUMS.sign")),
            "check" => true,
            "download" => self
                .iso_path
                .as_ref()
                .is_some_and(|path| Path::new(path).is_file()),
            "verify" => has(&cache.join("manifest.json")),
            "sshkey" => {
                has(&support.join("ssh/id_ed25519"))
                    && has(&support.join("ssh/id_ed25519.pub"))
                    && has(&support.join("ssh/authorized_keys"))
            }
            "preseed" => has(&support.join("installer/preseed.img")),
            "disk" => {
                let disk = self
                    .disk_path
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| support.join("machines/debian/disk.img"));
                has(&disk)
            }
            "patch" => has(&support.join("installer/boot.iso")),
            _ => true,
        }
    }

    fn resume(&mut self, id: &'static str) {
        self.resume_id(id);
    }

    fn resume_id(&mut self, id: &str) {
        self.running = false;
        let support = self.support_of_disk();
        self.rewind(id);
        self.forget_disk_if_needed(support);
        self.started = false;
        self.resume_from = Some(id.to_string());
        self.start();
    }

    fn support_of_disk(&self) -> Option<PathBuf> {
        self.disk_path.as_ref().and_then(|path| {
            Path::new(path)
                .parent()
                .and_then(|dir| dir.parent())
                .and_then(|dir| dir.parent())
                .map(|dir| dir.to_path_buf())
        })
    }

    fn forget_disk_if_needed(&self, support: Option<PathBuf>) {
        let Some(support) = support else { return };
        let disk = support.join("machines/debian/disk.img");
        if disk.is_file() {
            return;
        }
        let _ = std::fs::remove_file(disk.parent().unwrap().join("logged-in"));
        let _ = std::fs::remove_file(support.join("ssh/logged-in"));
    }

    fn rewind(&mut self, id: &str) {
        let Some(index) = self.steps.iter().position(|step| step.id == id) else {
            return;
        };
        for i in index..self.steps.len() {
            self.steps[i].state = if i == index { "now" } else { "wait" }.into();
            self.steps[i].detail = default_detail(self.steps[i].id);
        }
        self.ready_path = None;
        self.disk_path = None;
        self.downloading = false;
        self.received = 0.0;
        self.total = 0.0;
        self.error = None;
        self.headline = None;
        self.cancelled = false;
    }

    fn row(&self, id: &str) -> &Step {
        self.steps
            .iter()
            .find(|step| step.id == id)
            .unwrap_or_else(|| panic!("missing {id}"))
    }

    fn bar_visible(&self) -> bool {
        self.downloading
    }

    fn shows_percent(&self) -> bool {
        self.bar_visible() && self.total > 0.0
    }

    fn buttons(&self) -> Vec<&'static str> {
        let mut buttons = vec!["Open folder"];
        if self.error.is_some() || self.cancelled {
            buttons.push("Try again");
        } else if self.ready_path.is_none() {
            buttons.push("Cancel");
        }
        buttons
    }

    fn installing(&self) -> bool {
        self.error.is_none() && self.ready_path.is_some() && self.disk_path.is_some()
    }

    fn bytes_label(&self) -> String {
        if self.total > 0.0 {
            format!(
                "{} of {}",
                byte_count(self.received),
                byte_count(self.total)
            )
        } else if self.downloading {
            if self.received > 0.0 {
                byte_count(self.received)
            } else {
                "Connecting...".into()
            }
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
            assert!(screen.headline.is_none());
        }
        assert_eq!(screen.downloading, saw_download);
        assert_eq!(screen.bar_visible(), saw_download);
    }
    screen
}

fn iso_path(dir: &Path) -> String {
    dir.join("cache").join(iso_name()).display().to_string()
}

/// The installer is not run here. The worker still refuses a snapshot until SSH has logged in and the partition table exists.
#[test]
fn snapshot_waits_for_ssh_then_clones() {
    let (base, _) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, events, dir) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let screen = finish(&events, status);
    assert!(screen.installing());
    assert_eq!(
        screen.disk_path.as_deref(),
        Some(dir.join("machines/debian/disk.img").to_str().unwrap())
    );
    assert!(dir.join("ssh/id_ed25519").is_file());
    assert!(dir.join("ssh/authorized_keys").is_file());

    let missing = cli(&["clone", "--dir", dir.to_str().unwrap()]);
    assert!(!missing.status.success());
    let missing_out = String::from_utf8_lossy(&missing.stdout);
    assert!(missing_out.contains("no Debian snapshot"), "{missing_out}");
    assert!(missing_out.contains("\"event\":\"error\""), "{missing_out}");
    let install = finish_install(&missing.stdout);
    assert_eq!(
        install.error.as_deref(),
        Some("no Debian snapshot yet. Finish the installer first.")
    );
    assert_eq!(
        install.headline.as_deref(),
        Some("Debian shut down, but the snapshot was not saved.")
    );
    assert!(!install.snapshot_ready);

    let early = cli(&["snapshot", "--dir", dir.to_str().unwrap()]);
    assert!(!early.status.success());
    let early_out = String::from_utf8_lossy(&early.stdout);
    assert!(early_out.contains("install is not finished"), "{early_out}");
    assert!(!dir.join("snapshots/debian/ready").exists());
    let refused = finish_install(&early.stdout);
    assert!(!refused.snapshot_ready);
    assert!(refused.error.unwrap().contains("install is not finished"));

    let disk = dir.join("machines/debian/disk.img");
    let mut file = std::fs::OpenOptions::new().write(true).open(&disk).unwrap();
    use std::io::{Seek, SeekFrom, Write};
    file.seek(SeekFrom::Start(510)).unwrap();
    file.write_all(&[0x55, 0xaa]).unwrap();
    file.seek(SeekFrom::Start(4096)).unwrap();
    file.write_all(b"GUEST-BYTES!").unwrap();
    drop(file);
    std::fs::write(dir.join("machines/debian/nvram.bin"), b"efi").unwrap();
    let refused = cli(&["snapshot", "--dir", dir.to_str().unwrap()]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stdout).contains("SSH has not logged in"));
    std::fs::write(dir.join("ssh/logged-in"), b"debian ok\n").unwrap();

    let saved = cli(&["snapshot", "--dir", dir.to_str().unwrap()]);
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stdout)
    );
    let event = one_event(&saved.stdout);
    assert_eq!(event["event"], "snapshot");
    assert_eq!(event["state"], "done");
    assert_eq!(
        event["path"],
        dir.join("snapshots/debian").display().to_string()
    );
    assert_eq!(
        event["disk"],
        dir.join("snapshots/debian/disk.img").display().to_string()
    );
    assert!(
        event.get("detail").is_none(),
        "snapshot success has no error detail"
    );
    assert!(dir.join("snapshots/debian/ready").is_file());
    assert!(std::fs::read_to_string(dir.join("snapshots/debian/ready"))
        .unwrap()
        .starts_with("ssh-ok"));
    std::fs::remove_file(dir.join("snapshots/debian/ready")).unwrap();
    let again = cli(&["snapshot", "--dir", dir.to_str().unwrap()]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stdout)
    );
    assert!(dir.join("snapshots/debian/ready").is_file());
    std::fs::remove_file(dir.join("snapshots/debian/disk.img")).unwrap();
    let recopied = cli(&["snapshot", "--dir", dir.to_str().unwrap()]);
    assert!(
        recopied.status.success(),
        "{}",
        String::from_utf8_lossy(&recopied.stdout)
    );
    assert_eq!(
        std::fs::metadata(dir.join("snapshots/debian/disk.img"))
            .unwrap()
            .len(),
        16 * 1024 * 1024 * 1024
    );
    assert_eq!(
        std::fs::metadata(dir.join("snapshots/debian/disk.img"))
            .unwrap()
            .len(),
        16 * 1024 * 1024 * 1024
    );
    assert_eq!(
        std::fs::read(dir.join("snapshots/debian/nvram.bin")).unwrap(),
        b"efi"
    );
    let installed = finish_install(&saved.stdout);
    assert!(installed.error.is_none());
    assert!(installed.snapshot_ready);
    assert_eq!(
        installed.headline.as_deref(),
        Some("Debian is installed and shut down. A snapshot is saved for new machines.")
    );
    assert_eq!(
        installed.buttons(),
        ["Open folder", "Back", "New virtual machine"]
    );
    assert!(
        !installed.shows_installer,
        "a saved snapshot hides the thumbnail"
    );
    assert_eq!(installed.row("snapshot").state, "done");
    assert_eq!(installed.row("ssh").detail, "debian");

    let mut src = std::fs::OpenOptions::new().write(true).open(&disk).unwrap();
    src.seek(SeekFrom::Start(4096)).unwrap();
    src.write_all(b"CHANGED!!!!!").unwrap();
    drop(src);
    std::fs::write(dir.join("machines/debian/nvram.bin"), b"mutated").unwrap();
    let again = cli(&["snapshot", "--dir", dir.to_str().unwrap()]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stdout)
    );
    let mut frozen = [0u8; 12];
    let mut snap = std::fs::File::open(dir.join("snapshots/debian/disk.img")).unwrap();
    snap.seek(SeekFrom::Start(4096)).unwrap();
    std::io::Read::read_exact(&mut snap, &mut frozen).unwrap();
    assert_eq!(&frozen, b"GUEST-BYTES!");
    assert_eq!(
        std::fs::read(dir.join("snapshots/debian/nvram.bin")).unwrap(),
        b"efi"
    );

    let bad = cli(&["clone", "--dir", dir.to_str().unwrap(), "--name", "../x"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stdout).contains("machine name"));
    let named = cli(&["clone", "--dir", dir.to_str().unwrap(), "--name", "lab"]);
    assert!(
        named.status.success(),
        "{}",
        String::from_utf8_lossy(&named.stdout)
    );
    let named_event = one_event(&named.stdout);
    assert_eq!(named_event["event"], "clone");
    assert_eq!(
        named_event["disk"],
        dir.join("machines/lab/disk.img").display().to_string()
    );
    let dup = cli(&["clone", "--dir", dir.to_str().unwrap(), "--name", "lab"]);
    assert!(!dup.status.success());
    assert!(String::from_utf8_lossy(&dup.stdout).contains("already exists"));
    let installer = cli(&["clone", "--dir", dir.to_str().unwrap(), "--name", "debian"]);
    assert!(!installer.status.success());
    assert!(String::from_utf8_lossy(&installer.stdout).contains("installer disk"));

    let cloned = cli(&["clone", "--dir", dir.to_str().unwrap()]);
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stdout)
    );
    let event = one_event(&cloned.stdout);
    assert_eq!(event["event"], "clone");
    assert_eq!(event["state"], "done");
    assert_eq!(
        event["disk"],
        dir.join("machines/debian-2/disk.img").display().to_string()
    );
    assert_eq!(
        std::fs::metadata(dir.join("machines/debian-2/disk.img"))
            .unwrap()
            .len(),
        16 * 1024 * 1024 * 1024
    );
    let mut cloned_buf = [0u8; 12];
    let mut cloned_disk = std::fs::File::open(dir.join("machines/debian-2/disk.img")).unwrap();
    cloned_disk.seek(SeekFrom::Start(4096)).unwrap();
    std::io::Read::read_exact(&mut cloned_disk, &mut cloned_buf).unwrap();
    assert_eq!(&cloned_buf, b"GUEST-BYTES!");
    assert_eq!(
        std::fs::read(dir.join("machines/debian-2/nvram.bin")).unwrap(),
        b"efi"
    );
    let welcome = finish_clone(&cloned.stdout);
    assert_eq!(welcome.disk.as_deref(), event["disk"].as_str());
    assert!(welcome.error.is_none());
    assert!(welcome.can_clone);
    let machine = open_clone(event["disk"].as_str().unwrap());
    assert_eq!(machine.title, "Debian");
    assert!(machine.captures_pointer());
    assert!(!machine.shows_installer());
    assert_eq!(machine.headline.as_deref(), Some("Starting Debian."));
    assert_eq!(machine.buttons(), [] as [&str; 0]);

    let next = cli(&["clone", "--dir", dir.to_str().unwrap()]);
    assert!(
        next.status.success(),
        "{}",
        String::from_utf8_lossy(&next.stdout)
    );
    assert_eq!(
        one_event(&next.stdout)["disk"],
        dir.join("machines/debian-3/disk.img").display().to_string()
    );
}

fn one_event(stdout: &[u8]) -> Value {
    let text = std::str::from_utf8(stdout).unwrap();
    let line = text.lines().next().unwrap();
    serde_json::from_str(line).unwrap_or_else(|_| panic!("bad event {text}"))
}

/// Same snapshot outcome as Install.swift `saveSnapshot`, then the setup rows that page paints.
struct Installed {
    error: Option<String>,
    headline: Option<String>,
    snapshot_ready: bool,
    steps: Vec<Step>,
    stopped: bool,
    shows_installer: bool,
    cloning: bool,
}

fn finish_install(stdout: &[u8]) -> Installed {
    let err = worker_error(stdout);
    let mut page = setup_page(false);
    if let Some(err) = err {
        page.fail_current();
        page.headline = Some("Debian shut down, but the snapshot was not saved.".into());
        page.error = Some(err);
        page.snapshot_ready = false;
    } else {
        page.mark("ssh", "done", Some("debian"));
        page.mark("snapshot", "done", Some("Saved"));
        page.headline =
            Some("Debian is installed and shut down. A snapshot is saved for new machines.".into());
        page.error = None;
        page.snapshot_ready = true;
        page.show_window = false;
    }
    page.stopped = true;
    let shows_installer = page.shows_installer();
    Installed {
        error: page.error,
        headline: page.headline,
        snapshot_ready: page.snapshot_ready,
        steps: page.steps,
        stopped: page.stopped,
        shows_installer,
        cloning: false,
    }
}

impl Installed {
    fn row(&self, id: &str) -> &Step {
        self.steps.iter().find(|step| step.id == id).unwrap()
    }

    fn buttons(&self) -> Vec<&'static str> {
        let mut buttons = vec!["Open folder", "Back"];
        if !self.stopped {
            return buttons;
        }
        if !self.snapshot_ready {
            buttons.push("Start Debian");
        } else {
            buttons.push("New virtual machine");
        }
        if self.error.is_some() && !self.snapshot_ready {
            buttons.push("Save snapshot");
        }
        let _ = self.cloning;
        buttons
    }
}

/// Same clone outcome as Welcome.swift `newDebian`.
struct Cloned {
    disk: Option<String>,
    error: Option<String>,
    can_clone: bool,
}

fn finish_clone(stdout: &[u8]) -> Cloned {
    match worker_disk(stdout) {
        Some(disk) => Cloned {
            disk: Some(disk),
            error: None,
            can_clone: true,
        },
        None => Cloned {
            disk: None,
            error: Some(
                worker_error(stdout)
                    .unwrap_or_else(|| "Could not create a virtual machine.".into()),
            ),
            can_clone: false,
        },
    }
}

fn worker_error(stdout: &[u8]) -> Option<String> {
    parse_events(stdout).into_iter().find_map(|event| {
        (event["event"] == "error").then(|| {
            event["detail"]
                .as_str()
                .filter(|text| !text.is_empty())
                .unwrap_or("Could not save the snapshot.")
                .to_string()
        })
    })
}

fn worker_disk(stdout: &[u8]) -> Option<String> {
    for event in parse_events(stdout) {
        if let Some(disk) = event["disk"].as_str() {
            if !disk.is_empty() {
                return Some(disk.to_string());
            }
        }
    }
    None
}

fn watch(base: &str, dir: &Path) -> (std::process::Child, Receiver<Value>, thread::JoinHandle<()>) {
    let mut child = spawn(base, dir);
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.is_empty() {
                continue;
            }
            if tx.send(serde_json::from_str(&line).expect(&line)).is_err() {
                break;
            }
        }
    });
    (child, rx, reader)
}

fn finish_watch(
    mut child: std::process::Child,
    reader: thread::JoinHandle<()>,
    events: &Receiver<Value>,
    seen: &mut Vec<Value>,
) -> (i32, Vec<Value>) {
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(1),
            Ok(None) if started.elapsed() > Duration::from_secs(30) => {
                let _ = child.kill();
                panic!("worker timed out");
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(err) => panic!("wait failed: {err}"),
        }
    };
    reader.join().unwrap();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    (status, seen.clone())
}

/// The bar stays hidden until the first ISO progress line, and that line has no size yet.
fn bar_hidden_until_iso_progress(events: &[Value]) {
    let mut screen = Screen::new();
    screen.start();
    let mut saw = false;
    for event in events {
        let kind = event["event"].as_str().unwrap_or("");
        let step = event["step"].as_str();
        if !saw && kind == "progress" && step == Some("download") {
            assert_eq!(
                num(&event["total"]),
                Some(0.0),
                "first bar paint must not wait for the content length: {event}"
            );
            saw = true;
        }
        screen.apply(event);
        assert_eq!(screen.bar_visible(), saw, "{event}");
        if !saw {
            assert!(!screen.shows_percent());
            assert_eq!(screen.bytes_label(), "");
            assert_eq!(screen.received, 0.0);
            assert_eq!(screen.total, 0.0);
        }
    }
    assert!(saw, "download never painted a bar");
}

fn until(
    events: &Receiver<Value>,
    seen: &mut Vec<Value>,
    pred: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    let started = Instant::now();
    loop {
        if seen.iter().any(&pred) {
            return seen.clone();
        }
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => seen.push(event),
            Err(RecvTimeoutError::Timeout) if started.elapsed() > Duration::from_secs(10) => {
                panic!("timed out waiting for the window event: {seen:?}");
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                panic!("worker closed its event channel: {seen:?}");
            }
        }
    }
}

#[test]
fn cancel_stops_the_window_and_ignores_later_events() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, true, false);
    let dir = dir();
    let (mut child, events, reader) = watch(&base, &dir);
    let mut seen = Vec::new();
    let before = until(&events, &mut seen, |event| {
        event["event"] == "progress" && event["step"] == "download"
    });
    let release = world
        .go_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("iso request never published a release handle");
    let mut screen = drive(&before);
    assert_eq!(screen.row("download").state, "now");
    assert!(screen.bar_visible());
    assert_eq!(screen.bytes_label(), "Connecting...");
    screen.cancel();
    release.send(()).unwrap();
    let status = child.wait().unwrap().code().unwrap_or(1);
    reader.join().unwrap();
    let mut after = seen;
    while let Ok(event) = events.try_recv() {
        after.push(event);
    }
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
    assert_eq!(
        screen.headline.as_deref(),
        Some("Cancelled. The download was not saved.")
    );
    assert_eq!(screen.row("download").state, "wait");
    assert_eq!(screen.row("download").detail, "Cancelled");
    assert_eq!(screen.row("check").state, "done");
    assert_eq!(screen.buttons(), ["Open folder", "Try again"]);
    assert!(
        screen.bar_visible(),
        "cancel does not wipe a bar that was already on screen"
    );
    screen.retry();
    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    let screen = finish(&events, status);
    assert_eq!(screen.ready_path.as_deref(), Some(iso_path(&dir).as_str()));
    assert!(screen.installing());
}

#[test]
fn a_killed_worker_fails_the_step_that_was_running() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, true, false);
    let dir = dir();
    let (mut child, events, reader) = watch(&base, &dir);
    let mut seen_lines = Vec::new();
    let seen = until(&events, &mut seen_lines, |event| {
        event["event"] == "progress" && event["step"] == "download"
    });
    let mut screen = drive(&seen);
    assert!(screen.bar_visible());
    assert_eq!(screen.row("download").state, "now");
    let _ = child.kill();
    let status = child.wait().unwrap().code().unwrap_or(1);
    reader.join().unwrap();
    release(&world);
    screen.worker_exited(status);
    assert_eq!(
        screen.error.as_deref(),
        Some("The download stopped before the image was ready.")
    );
    assert_eq!(screen.headline.as_deref(), Some("Download failed"));
    assert_eq!(screen.row("download").state, "fail");
    assert_eq!(screen.row("download").detail, iso_name());
    assert!(screen.bar_visible());
    assert!(screen.ready_path.is_none());
    assert_eq!(screen.buttons(), ["Open folder", "Try again"]);
    assert!(!screen.cancelled);
}

#[test]
fn deleting_the_finished_iso_clears_the_window_and_starts_again() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, first, dir) = run(&base);
    assert_eq!(status, 0, "{first:?}");
    let cache = dir.join("cache");
    let iso = cache.join(iso_name());
    let mut screen = finish(&first, status);
    assert!(!screen.cache_went_away(&cache));
    assert!(screen.ready_path.is_some());
    std::fs::remove_file(&iso).unwrap();
    assert!(screen.cache_went_away(&cache));
    assert_eq!(screen.resume_from.as_deref(), Some("download"));
    assert_eq!(screen.row("check").state, "done");
    assert_eq!(screen.row("download").state, "now");
    assert_eq!(screen.row("preseed").state, "wait");
    assert!(!screen.bar_visible());
    assert!(screen.ready_path.is_none());
    assert!(screen.headline.is_none());
    assert_eq!(screen.page, 0);
    assert_eq!(screen.buttons(), ["Open folder", "Cancel"]);
    assert!(screen.running);
    let _ = log_of(&world);
    let (status, events) = wait(spawn_from(&base, &dir, Some("download")));
    assert_eq!(status, 0, "{events:?}");
    let log = log_of(&world);
    assert!(log.iter().any(|line| line.contains(".iso")), "{log:?}");
    assert!(
        log.iter().all(|line| !line.contains("SHA512SUMS")),
        "a missing ISO does not fetch the checksums again: {log:?}"
    );
    for event in &events {
        screen.apply(event);
    }
    screen.worker_exited(status);
    assert!(screen.bar_visible());
    assert_eq!(screen.percent(), 100);
    assert_eq!(
        screen.ready_path.as_deref(),
        Some(iso.display().to_string()).as_deref()
    );
    assert!(screen.installing());
    assert_eq!(screen.page, 0);
    assert_eq!(screen.buttons(), ["Open folder"]);

    let mut running = drive(
        &first[..first
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
fn deleting_the_debian_disk_clears_the_window_and_starts_again() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, events, dir) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let cache = dir.join("cache");
    let disk = dir.join("machines/debian/disk.img");
    let mut screen = finish(&events, status);
    assert!(screen.installing());
    assert!(!screen.cache_went_away(&cache));
    std::fs::remove_file(&disk).unwrap();
    assert!(
        screen.cache_went_away(&cache),
        "removing machines/debian/disk.img is not a finished install"
    );
    assert_eq!(screen.resume_from.as_deref(), Some("disk"));
    assert_eq!(screen.row("verify").state, "done");
    assert_eq!(screen.row("preseed").state, "done");
    assert_eq!(screen.row("disk").state, "now");
    assert_eq!(screen.row("patch").state, "wait");
    assert!(screen.ready_path.is_none());
    assert!(screen.disk_path.is_none());
    assert!(!screen.installing());
    assert_eq!(screen.page, 0);
    assert_eq!(screen.buttons(), ["Open folder", "Cancel"]);
    assert!(screen.running);
    let _ = log_of(&world);
    let (status, events) = wait(spawn_from(&base, &dir, Some("disk")));
    assert_eq!(status, 0, "{events:?}");
    assert!(
        log_of(&world).is_empty(),
        "a missing guest disk does not download the ISO again"
    );
    assert!(events.iter().all(|event| event["step"] != "preseed"));
    for event in &events {
        screen.apply(event);
    }
    screen.worker_exited(status);
    assert!(
        disk.is_file(),
        "prepare the guest disk must write disk.img again"
    );
    assert_eq!(screen.disk_path.as_deref(), Some(disk.to_str().unwrap()));
    assert!(screen.installing());
    assert_eq!(screen.row("preseed").state, "done");
    assert_eq!(screen.row("disk").state, "done");
    assert_eq!(screen.row("disk").detail, "disk.img");
    assert!(
        !std::fs::read_dir(dir.join("ssh"))
            .unwrap()
            .flatten()
            .any(|item| item.file_name() == "logged-in"),
        "a new empty disk must not inherit the previous login"
    );
}

#[test]
fn a_removed_file_reruns_from_the_step_that_wrote_it() {
    let help = cli(&["download", "--help"]);
    assert!(String::from_utf8_lossy(&help.stdout).contains("--from"));
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, events, dir) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let cache = dir.join("cache");
    let mut screen = finish(&events, status);
    assert_eq!(screen.first_missing(&cache), None);
    let bad = cli(&["download", "--from", "nope", "--dir", dir.to_str().unwrap()]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stdout).contains("unknown step"));

    let files = [
        ("cache/key-DA87E80D6294BE9B.txt", "key"),
        ("cache/SHA512SUMS", "sums"),
        ("cache/SHA512SUMS.sign", "sign"),
        ("cache/manifest.json", "verify"),
        ("installer/preseed.img", "preseed"),
        ("ssh/id_ed25519", "sshkey"),
        ("ssh/id_ed25519.pub", "sshkey"),
        ("ssh/authorized_keys", "sshkey"),
        ("machines/debian/disk.img", "disk"),
        ("installer/boot.iso", "patch"),
    ];
    for (rel, step) in files {
        let path = dir.join(rel);
        // disk.img is a 16 GiB sparse file. Rename it; do not read it into memory.
        let held = path.with_extension("held");
        std::fs::rename(&path, &held).unwrap();
        assert_eq!(screen.first_missing(&cache), Some(step), "{rel}");
        std::fs::rename(&held, &path).unwrap();
        assert_eq!(screen.first_missing(&cache), None, "{rel}");
    }
    let iso = cache.join(iso_name());
    let iso_bytes = std::fs::read(&iso).unwrap();
    std::fs::remove_file(&iso).unwrap();
    assert_eq!(screen.first_missing(&cache), Some("download"));
    std::fs::write(&iso, &iso_bytes).unwrap();
    std::fs::write(dir.join("machines/debian/nvram.bin"), b"efi").unwrap();
    std::fs::remove_file(dir.join("machines/debian/nvram.bin")).unwrap();
    std::fs::remove_file(cache.join("download.lock")).unwrap();
    assert_eq!(
        screen.first_missing(&cache),
        None,
        "nvram and the lock are not wizard steps"
    );

    std::fs::remove_file(dir.join("installer/preseed.img")).unwrap();
    assert!(screen.cache_went_away(&cache));
    assert_eq!(screen.resume_from.as_deref(), Some("preseed"));
    assert_eq!(screen.row("verify").state, "done");
    assert_eq!(screen.row("preseed").state, "now");
    assert_eq!(screen.row("preseed").detail, "preseed.img");
    assert_eq!(screen.row("patch").state, "wait");
    assert_eq!(screen.page, 0);
    assert_eq!(screen.buttons(), ["Open folder", "Cancel"]);
    let _ = log_of(&world);
    let (status, events) = wait(spawn_from(&base, &dir, Some("preseed")));
    assert_eq!(status, 0, "{events:?}");
    assert!(
        log_of(&world).is_empty(),
        "preseed.img does not download again"
    );
    assert!(dir.join("installer/preseed.img").is_file());
    assert!(!cache.join("preseed.img").exists());
    assert!(events
        .iter()
        .any(|event| event["step"] == "preseed" && event["state"] == "now"));
    assert!(events
        .iter()
        .all(|event| event["step"] != "download" && event["step"] != "key"));
    for event in &events {
        screen.apply(event);
    }
    screen.worker_exited(status);
    assert_eq!(screen.row("preseed").state, "done");
    assert_eq!(screen.row("sshkey").detail, "id_ed25519");
    assert_eq!(screen.row("disk").detail, "disk.img");
    assert_eq!(screen.row("patch").detail, "boot.iso");
    assert!(screen.installing());

    std::fs::remove_file(dir.join("installer/boot.iso")).unwrap();
    assert!(screen.cache_went_away(&cache));
    assert_eq!(screen.resume_from.as_deref(), Some("patch"));
    assert_eq!(screen.row("preseed").state, "done");
    assert_eq!(screen.row("patch").state, "now");
    let _ = log_of(&world);
    let (status, events) = wait(spawn_from(&base, &dir, Some("patch")));
    assert_eq!(status, 0, "{events:?}");
    assert!(log_of(&world).is_empty());
    assert!(dir.join("installer/boot.iso").is_file());
    assert!(events.iter().all(|event| event["step"] != "preseed"));
    for event in &events {
        screen.apply(event);
    }
    screen.worker_exited(status);
    assert_eq!(screen.row("patch").state, "done");
    assert_eq!(screen.row("patch").detail, "boot.iso");

    std::fs::remove_file(cache.join("SHA512SUMS")).unwrap();
    assert!(screen.cache_went_away(&cache));
    assert_eq!(screen.resume_from.as_deref(), Some("sums"));
    assert_eq!(screen.row("key").state, "done");
    assert_eq!(screen.row("sums").state, "now");
    assert_eq!(screen.row("preseed").state, "wait");
    assert_eq!(screen.page, 0);
    let _ = log_of(&world);
    let (status, events) = wait(spawn_from(&base, &dir, Some("sums")));
    assert_eq!(status, 0, "{events:?}");
    let log = log_of(&world);
    assert!(
        log.iter().any(|line| line.contains("SHA512SUMS")),
        "{log:?}"
    );
    assert!(
        log.iter().all(|line| !line.contains(".iso")),
        "checksums are fetched again, the ISO is not: {log:?}"
    );
    for event in &events {
        screen.apply(event);
    }
    screen.worker_exited(status);
    assert_eq!(screen.row("sums").state, "done");
    assert_eq!(screen.row("preseed").state, "done");
    assert!(dir.join("installer/preseed.img").is_file());
    assert!(!cache.join("preseed.img").exists());

    // Removed while the worker is still running. The window waits, then restarts that step and finishes.
    screen.running = true;
    std::fs::remove_file(dir.join("installer/preseed.img")).unwrap();
    assert!(
        !screen.cache_went_away(&cache),
        "a running step is not interrupted"
    );
    screen.running = false;
    assert!(screen.cache_went_away(&cache));
    assert_eq!(screen.resume_from.as_deref(), Some("preseed"));
    assert_eq!(screen.row("sshkey").state, "done");
    assert_eq!(screen.row("preseed").state, "now");
    let _ = log_of(&world);
    let (status, events) = wait(spawn_from(&base, &dir, Some("preseed")));
    assert_eq!(status, 0, "{events:?}");
    assert!(
        log_of(&world).is_empty(),
        "a mid-run delete does not download again"
    );
    for event in &events {
        screen.apply(event);
    }
    screen.worker_exited(status);
    assert!(dir.join("installer/preseed.img").is_file());
    assert!(dir.join("installer/boot.iso").is_file());
    assert_eq!(screen.row("preseed").state, "done");
    assert_eq!(screen.row("patch").state, "done");
    assert!(screen.installing());
}

#[test]
fn the_window_ignores_checksum_bytes_and_formats_its_own() {
    let mut screen = Screen::new();
    assert!(!screen.bar_visible());
    assert!(screen.headline.is_none());
    assert_eq!(screen.buttons(), ["Open folder", "Cancel"]);
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
    assert!(screen.bar_visible(), "unknown size still shows the bar");
    assert_eq!(screen.bytes_label(), "Connecting...");
    assert_eq!(screen.percent(), 0);
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
    assert_eq!(screen.buttons(), ["Open folder", "Try again"]);

    let mut missing = Screen::new();
    missing.worker_missing();
    assert_eq!(
        missing.error.as_deref(),
        Some("The worker is not built. Run cargo build --release, then try again.")
    );
    assert_eq!(missing.headline.as_deref(), Some("Could not start"));
    assert_eq!(missing.row("key").state, "fail");
    assert_eq!(missing.buttons(), ["Open folder", "Try again"]);
    assert!(!missing.bar_visible());
    missing.worker_missing();
    assert_eq!(
        missing.row("key").state,
        "fail",
        "a second start does not run again"
    );
}

/// The ISO bar is on screen while the mirror is still holding the response.
#[test]
fn the_download_bar_shows_when_the_iso_step_starts() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, true, false);
    let dir = dir();
    let (child, events, reader) = watch(&base, &dir);
    let mut seen = Vec::new();
    let early = until(&events, &mut seen, |event| {
        event["event"] == "step" && event["step"] == "check" && event["state"] == "done"
    });
    let check_done = early
        .iter()
        .position(|event| {
            event["event"] == "step" && event["step"] == "check" && event["state"] == "done"
        })
        .unwrap();
    let screen = drive(&early[..=check_done]);
    assert_eq!(screen.row("check").state, "done");
    assert_eq!(screen.row("download").state, "wait");
    assert!(!screen.bar_visible());
    assert!(!screen.shows_percent());
    assert_eq!(screen.bytes_label(), "");

    let started = until(&events, &mut seen, |event| {
        event["event"] == "progress" && event["step"] == "download"
    });
    assert_eq!(
        started
            .iter()
            .filter(|event| event["event"] == "progress" && event["step"] == "download")
            .count(),
        1,
        "size must not arrive while the response is held: {started:?}"
    );
    let step_at = started
        .iter()
        .position(|event| {
            event["event"] == "step" && event["step"] == "download" && event["state"] == "now"
        })
        .unwrap();
    let progress_at = started
        .iter()
        .position(|event| event["event"] == "progress" && event["step"] == "download")
        .unwrap();
    assert_eq!(progress_at, step_at + 1, "{started:?}");
    let at_step = drive(&started[..=step_at]);
    assert_eq!(at_step.row("download").state, "now");
    assert!(!at_step.bar_visible());
    let screen = drive(&started);
    assert_eq!(screen.row("download").state, "now");
    assert_eq!(screen.row("download").detail, iso_name());
    assert!(screen.bar_visible());
    assert!(!screen.shows_percent());
    assert_eq!(screen.received, 0.0);
    assert_eq!(screen.total, 0.0);
    assert_eq!(screen.percent(), 0);
    assert_eq!(screen.bytes_label(), "Connecting...");
    bar_hidden_until_iso_progress(&started);

    release(&world);
    let (status, all) = finish_watch(child, reader, &events, &mut seen);
    assert_eq!(status, 0, "{all:?}");
    bar_hidden_until_iso_progress(&all);
    let screen = finish(&all, status);
    assert!(screen.bar_visible());
    assert!(screen.shows_percent());
    assert_eq!(screen.percent(), 100);
    assert_eq!(screen.received, ISO.len() as f64);
    assert_eq!(screen.total, ISO.len() as f64);
    assert_eq!(
        screen.bytes_label(),
        format!(
            "{} of {}",
            byte_count(ISO.len() as f64),
            byte_count(ISO.len() as f64)
        )
    );
    assert_eq!(screen.page, 0);
    assert_eq!(screen.row("preseed").state, "done");
    assert_eq!(screen.ready_path.as_deref(), Some(iso_path(&dir).as_str()));
    assert!(screen.installing());
}

/// Headers can name the size while the body is still held. The bar shows that size immediately.
#[test]
fn the_download_bar_shows_the_size_before_any_body_byte() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false, true);
    pause_at(&world, 0);
    let dir = dir();
    let (child, events, reader) = watch(&base, &dir);
    let mut seen = Vec::new();
    let sized = until(&events, &mut seen, |event| {
        event["event"] == "progress"
            && event["step"] == "download"
            && num(&event["total"]).unwrap_or(0.0) > 0.0
    });
    assert!(log_of(&world).iter().any(|line| line.contains(".iso")));
    let first = sized
        .iter()
        .position(|event| event["event"] == "progress" && event["step"] == "download")
        .unwrap();
    let connecting = drive(&sized[..=first]);
    assert!(connecting.bar_visible());
    assert!(!connecting.shows_percent());
    assert_eq!(connecting.total, 0.0);
    assert_eq!(connecting.bytes_label(), "Connecting...");
    let screen = drive(&sized);
    assert_eq!(screen.row("download").state, "now");
    assert!(screen.bar_visible());
    assert!(screen.shows_percent());
    assert_eq!(screen.received, 0.0);
    assert_eq!(screen.total, ISO.len() as f64);
    assert_eq!(screen.percent(), 0);
    assert_eq!(
        screen.bytes_label(),
        format!("0 KB of {}", byte_count(ISO.len() as f64))
    );
    let partial = dir.join("cache").join(format!("{}.partial", iso_name()));
    assert_eq!(std::fs::metadata(&partial).unwrap().len(), 0);

    more(&world, None);
    let (status, all) = finish_watch(child, reader, &events, &mut seen);
    assert_eq!(status, 0, "{all:?}");
    let screen = finish(&all, status);
    assert!(screen.bar_visible());
    assert!(screen.shows_percent());
    assert_eq!(screen.percent(), 100);
    assert_eq!(screen.received, ISO.len() as f64);
    assert_eq!(
        std::fs::read(dir.join("cache").join(iso_name())).unwrap(),
        ISO
    );
    assert!(!partial.exists());
}

/// A slice of the body is enough. The bar shows that slice before the rest is released.
#[test]
fn the_download_bar_shows_partial_bytes_before_the_body_finishes() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false, true);
    pause_at(&world, 0);
    let dir = dir();
    let (child, events, reader) = watch(&base, &dir);
    let mut seen = Vec::new();
    let sized = until(&events, &mut seen, |event| {
        event["event"] == "progress"
            && event["step"] == "download"
            && num(&event["total"]).unwrap_or(0.0) == ISO.len() as f64
            && num(&event["received"]).unwrap_or(1.0) == 0.0
    });
    let connecting = sized.iter().position(|event| {
        event["event"] == "progress"
            && event["step"] == "download"
            && num(&event["total"]) == Some(0.0)
    });
    assert!(
        connecting.is_some(),
        "bar must appear before the size is known"
    );
    assert!(drive(&sized[..=connecting.unwrap()]).bar_visible());
    assert!(!drive(&sized[..=connecting.unwrap()]).shows_percent());
    let empty = drive(&sized);
    assert!(empty.bar_visible());
    assert!(empty.shows_percent());
    assert_eq!(empty.percent(), 0);
    assert_eq!(empty.received, 0.0);
    // The next body tick is dropped if it lands inside 250ms of the size tick.
    thread::sleep(Duration::from_millis(300));
    let sent = 1024usize;
    more(&world, Some(sent));
    let mid = until(&events, &mut seen, |event| {
        event["event"] == "progress"
            && event["step"] == "download"
            && num(&event["received"]).unwrap_or(0.0) == sent as f64
    });
    let screen = drive(&mid);
    assert_eq!(screen.row("download").state, "now");
    assert!(screen.bar_visible());
    assert!(screen.shows_percent());
    assert_eq!(screen.received, sent as f64);
    assert_eq!(screen.total, ISO.len() as f64);
    assert!(screen.percent() > 0 && screen.percent() < 100);
    assert_eq!(
        screen.percent(),
        ((sent as f64 / ISO.len() as f64) * 100.0).round() as i64
    );
    assert_eq!(
        screen.bytes_label(),
        format!(
            "{} of {}",
            byte_count(sent as f64),
            byte_count(ISO.len() as f64)
        )
    );
    let partial = dir.join("cache").join(format!("{}.partial", iso_name()));
    assert_eq!(std::fs::metadata(&partial).unwrap().len(), sent as u64);

    more(&world, None);
    let (status, all) = finish_watch(child, reader, &events, &mut seen);
    assert_eq!(status, 0, "{all:?}");
    let screen = finish(&all, status);
    assert!(screen.bar_visible());
    assert_eq!(screen.percent(), 100);
    assert_eq!(screen.received, ISO.len() as f64);
    assert_eq!(
        std::fs::read(dir.join("cache").join(iso_name())).unwrap(),
        ISO
    );
    assert!(!partial.exists());
}

/// A resumed fetch paints the bytes already on disk before the range response comes back.
#[test]
fn a_resume_shows_the_download_bar_before_the_range_response() {
    let (base, _world) = serve(SUMS, SIGN, ISO, KEY, true, false, false);
    let (status, _, dir) = run(&base);
    assert_ne!(status, 0);
    let partial = dir.join("cache").join(format!("{}.partial", iso_name()));
    let have = std::fs::metadata(&partial).unwrap().len();
    assert!(have > 0 && have < ISO.len() as u64, "{have}");
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, true, false);
    let (child, events, reader) = watch(&base, &dir);
    let mut seen = Vec::new();
    let started = until(&events, &mut seen, |event| {
        event["event"] == "progress" && event["step"] == "download"
    });
    assert_eq!(
        started
            .iter()
            .filter(|event| event["event"] == "progress" && event["step"] == "download")
            .count(),
        1
    );
    let screen = drive(&started);
    assert_eq!(screen.row("download").state, "now");
    assert!(screen.bar_visible());
    assert!(!screen.shows_percent());
    assert_eq!(screen.received, have as f64);
    assert_eq!(screen.total, 0.0);
    assert_eq!(screen.bytes_label(), byte_count(have as f64));
    bar_hidden_until_iso_progress(&started);

    release(&world);
    let (status, all) = finish_watch(child, reader, &events, &mut seen);
    assert_eq!(status, 0, "{all:?}");
    assert!(log_of(&world).iter().any(|line| line.contains("range=")));
    let screen = finish(&all, status);
    assert!(screen.bar_visible());
    assert_eq!(screen.percent(), 100);
    assert_eq!(screen.received, ISO.len() as f64);
    assert_eq!(
        std::fs::read(dir.join("cache").join(iso_name())).unwrap(),
        ISO
    );
}

#[test]
fn a_cached_iso_does_not_show_the_download_bar() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, events, dir) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let fetched = finish(&events, status);
    assert!(fetched.bar_visible());
    assert_eq!(fetched.percent(), 100);
    let _ = log_of(&world);
    let (status, again) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{again:?}");
    let again_log = log_of(&world);
    assert_eq!(
        again_log.len(),
        3,
        "cached iso was fetched again: {again_log:?}"
    );
    assert!(again
        .iter()
        .all(|event| !(event["event"] == "progress" && event["step"] == "download")));
    assert!(again
        .iter()
        .any(|event| event["event"] == "progress" && event["step"] == "verify"));
    let mut screen = Screen::new();
    screen.start();
    for event in &again {
        screen.apply(event);
        assert!(!screen.bar_visible(), "{event}");
        assert!(!screen.shows_percent());
    }
    screen.worker_exited(status);
    assert!(!screen.bar_visible());
    assert_eq!(screen.received, 0.0);
    assert_eq!(screen.total, 0.0);
    assert_eq!(screen.bytes_label(), "");
    assert_eq!(screen.ready_path.as_deref(), Some(iso_path(&dir).as_str()));
    assert!(screen.installing());
    assert_eq!(screen.page, 0);
    assert_eq!(screen.row("preseed").state, "done");
    assert_eq!(screen.page_rows(), ["download", "partition", "snapshot"]);
    screen.show_all = true;
    assert!(screen
        .page_rows()
        .starts_with(&["key", "sums", "sign", "check", "download"]));
}

/// Reopen is a new process. The ISO is cached, so the worker is fast, then the window
/// builds Install again. A disk the installer already wrote must boot alone.
/// A saved snapshot is the finished installer. Reopening that disk does not start it.
#[test]
fn reopening_quick_start_boots_the_installed_disk() {
    let (base, _) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let dir = dir();
    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    let disk = dir.join("machines/debian/disk.img");
    let mut file = std::fs::OpenOptions::new().write(true).open(&disk).unwrap();
    use std::io::{Seek, SeekFrom, Write};
    file.seek(SeekFrom::Start(510)).unwrap();
    file.write_all(&[0x55, 0xaa]).unwrap();
    drop(file);

    let (status, again) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{again:?}");
    let screen = finish(&again, status);
    assert!(screen.installing());
    assert_eq!(screen.disk_path.as_deref(), Some(disk.to_str().unwrap()));
    let boot = reopen_boot(screen.disk_path.as_deref().unwrap());
    assert_eq!(
        boot.headline.as_deref(),
        Some("Debian is already installed. Checking SSH.")
    );
    assert!(boot.installed);
    assert!(!boot.attaches_installer, "{boot:?}");
    assert!(
        !boot.captures_pointer,
        "a written disk boots in the popup with no pointer"
    );
    assert!(boot.checks_ssh);

    std::fs::write(disk.parent().unwrap().join("logged-in"), b"debian ok\n").unwrap();
    let boot = reopen_boot(disk.to_str().unwrap());
    assert_eq!(
        boot.headline.as_deref(),
        Some("Debian is already installed. Checking SSH.")
    );
    assert!(boot.installed);
    assert!(!boot.attaches_installer, "{boot:?}");
    assert!(boot.checks_ssh);
    assert!(
        !boot.captures_pointer,
        "quick start checks SSH in the popup and does not capture"
    );
    assert!(!boot.snapshot_saved);
    assert!(boot.starts_vm);

    std::fs::create_dir_all(dir.join("snapshots/debian")).unwrap();
    std::fs::write(dir.join("snapshots/debian/ready"), b"ssh-ok\n").unwrap();
    std::fs::write(dir.join("snapshots/debian/disk.img"), b"snap").unwrap();
    let boot = reopen_boot(disk.to_str().unwrap());
    assert!(boot.snapshot_saved, "{boot:?}");
    assert!(
        !boot.starts_vm,
        "a saved snapshot must not start the installer virtual machine"
    );
    assert!(!boot.attaches_installer);
    assert!(!boot.captures_pointer);
    assert!(!boot.checks_ssh);
    assert_eq!(
        boot.headline.as_deref(),
        Some("Debian is installed and shut down. A snapshot is saved for new machines.")
    );

    std::fs::remove_file(&disk).unwrap();
    std::fs::remove_file(dir.join("snapshots/debian/ready")).unwrap();
    std::fs::write(dir.join("ssh/logged-in"), b"debian ok\n").unwrap();
    let (status, fresh) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{fresh:?}");
    assert!(
        !dir.join("ssh/logged-in").exists(),
        "recreating disk.img must drop the login that belonged to the deleted disk"
    );
    let boot = reopen_boot(dir.join("machines/debian/disk.img").to_str().unwrap());
    assert!(!boot.installed);
    assert!(boot.attaches_installer);
    assert!(!boot.captures_pointer);
    assert!(!boot.checks_ssh);
    assert!(boot.starts_vm);
    assert!(!boot.snapshot_saved);
    assert_eq!(boot.headline.as_deref(), Some("Starting the installer."));
}

/// Debian setup, the page Quick start shows once the worker is ready.
/// Same rules as Install.swift. The worker is not asked to boot a virtual machine.
#[derive(Debug)]
struct Setup {
    title: &'static str,
    headline: Option<String>,
    error: Option<String>,
    steps: Vec<Step>,
    installed: bool,
    logged_in: bool,
    stopped: bool,
    snapshot_ready: bool,
    from_snapshot: bool,
    show_window: bool,
    started: bool,
    checking: bool,
    missing_file: Option<String>,
    /// Disks attached for a fresh install. Empty unless the installer starts. The signed ISO is not one of them.
    attached: Vec<String>,
}

fn setup_steps() -> Vec<Step> {
    vec![
        Step {
            id: "installer",
            title: "Start the installer",
            detail: "boot.iso".into(),
            state: "wait".into(),
        },
        Step {
            id: "partition",
            title: "Install guest",
            detail: "disk.img".into(),
            state: "wait".into(),
        },
        Step {
            id: "ssh",
            title: "Log in with the host key",
            detail: "guest port 22".into(),
            state: "wait".into(),
        },
        Step {
            id: "snapshot",
            title: "Save a snapshot",
            detail: "snapshots/debian".into(),
            state: "wait".into(),
        },
    ]
}

fn setup_page(from_snapshot: bool) -> Setup {
    Setup {
        title: if from_snapshot {
            "Debian"
        } else {
            "Debian setup"
        },
        headline: None,
        error: None,
        steps: setup_steps(),
        installed: from_snapshot,
        logged_in: from_snapshot,
        stopped: false,
        snapshot_ready: false,
        from_snapshot,
        show_window: false,
        started: false,
        checking: false,
        missing_file: None,
        attached: Vec::new(),
    }
}

impl Setup {
    fn row(&self, id: &str) -> &Step {
        self.steps.iter().find(|step| step.id == id).unwrap()
    }

    fn shows_installer(&self) -> bool {
        !self.from_snapshot && !self.snapshot_ready
    }

    fn captures_pointer(&self) -> bool {
        self.from_snapshot
    }

    fn mark(&mut self, id: &str, state: &str, detail: Option<&str>) {
        let Some(index) = self.steps.iter().position(|step| step.id == id) else {
            return;
        };
        if let Some(detail) = detail.filter(|text| !text.is_empty()) {
            self.steps[index].detail = detail.to_string();
        }
        self.steps[index].state = state.to_string();
        if state == "done" || state == "now" {
            for earlier in 0..index {
                if self.steps[earlier].state != "done" && self.steps[earlier].state != "fail" {
                    self.steps[earlier].state = "done".into();
                }
            }
        }
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

    /// Same choices as GuestSession.noteMissingFiles. No virtual machine is started here.
    fn note_missing(&mut self, disk: &Path) -> Option<&'static str> {
        if !disk.is_file() {
            if self.from_snapshot {
                self.error = Some("The guest disk was removed.".into());
                self.headline = Some("The virtual machine did not start.".into());
                self.stopped = true;
            } else if self.missing_file.is_none()
                && self
                    .steps
                    .iter()
                    .any(|step| step.id == "installer" && step.state == "done")
            {
                self.missing_file = Some("disk".into());
            }
            return self.missing_file.as_deref().and_then(static_step);
        }
        if self.from_snapshot {
            return None;
        }
        let support = disk
            .parent()
            .and_then(|dir| dir.parent())
            .and_then(|dir| dir.parent());
        if let Some(support) = support {
            if !support.join("installer/boot.iso").is_file() && self.missing_file.is_none() {
                self.missing_file = Some("patch".into());
            } else if !support.join("installer/preseed.img").is_file()
                && self.missing_file.is_none()
            {
                self.missing_file = Some("preseed".into());
            }
            if self.missing_file.is_some() {
                return self.missing_file.as_deref().and_then(static_step);
            }
        }
        let proof = guest_logged_in(disk);
        let snap = snapshot_ready(disk);
        let mut ran = None;
        if self.snapshot_ready && !snap {
            self.snapshot_ready = false;
            self.show_window = false;
            if proof {
                self.mark("snapshot", "now", Some("snapshots/debian"));
                self.headline = Some("SSH login succeeded. Saving a snapshot.".into());
            } else {
                // mark() would treat wait as a plain write. Set it directly so earlier rows stay.
                if let Some(row) = self.steps.iter_mut().find(|step| step.id == "snapshot") {
                    row.state = "wait".into();
                    row.detail = "snapshots/debian".into();
                }
                self.headline = Some("The snapshot was removed.".into());
            }
            ran = Some("snapshot");
        }
        let ssh_done = self
            .steps
            .iter()
            .any(|step| step.id == "ssh" && step.state == "done");
        if ssh_done && !proof && !self.checking {
            self.logged_in = false;
            self.installed = true;
            self.checking = true;
            self.stopped = false;
            self.error = None;
            self.mark("ssh", "now", Some("guest port 22"));
            self.headline = Some("Debian is already installed. Checking SSH.".into());
            ran = Some("ssh");
        }
        ran
    }

    fn hold_finished(&mut self, _disk: &str) {
        self.started = true;
        self.stopped = true;
        self.installed = true;
        self.logged_in = true;
        self.snapshot_ready = true;
        self.checking = false;
        self.show_window = false;
        self.error = None;
        self.headline =
            Some("Debian is installed and shut down. A snapshot is saved for new machines.".into());
        self.mark("installer", "done", None);
        self.mark("partition", "done", Some("Installed"));
        self.mark("ssh", "done", Some("debian"));
        self.mark("snapshot", "done", Some("Saved"));
    }

    /// Install.onAppear. A saved snapshot does not start. A written disk boots alone.
    fn appear(&mut self, iso: &str, disk: &str) {
        if self.snapshot_ready && !self.from_snapshot {
            self.hold_finished(disk);
        } else if self.from_snapshot || self.installed {
            self.boot_disk(disk, self.from_snapshot);
        } else {
            self.start(iso, disk);
        }
    }

    fn start(&mut self, iso: &str, disk: &str) {
        if self.started {
            return;
        }
        self.started = true;
        self.error = None;
        self.stopped = false;
        if self.snapshot_ready {
            self.hold_finished(disk);
            return;
        }
        if guest_logged_in(Path::new(disk)) || disk_looks_installed(Path::new(disk)) {
            self.installed = true;
            self.logged_in = guest_logged_in(Path::new(disk));
            self.checking = true;
            self.headline = Some("Debian is already installed. Checking SSH.".into());
            return;
        }
        self.steps = setup_steps();
        self.mark("installer", "now", Some("boot.iso"));
        self.headline = Some("Starting the installer.".into());
        // The signed ISO lives in cache/. boot.iso and preseed.img are in installer/, not that cache.
        let support = Path::new(iso)
            .parent()
            .and_then(|cache| cache.parent())
            .unwrap();
        self.attached = vec![
            support.join("installer/boot.iso").display().to_string(),
            support.join("installer/preseed.img").display().to_string(),
        ];
    }

    fn boot_disk(&mut self, disk: &str, pointer: bool) {
        if self.started {
            return;
        }
        self.started = true;
        self.installed = true;
        self.logged_in = guest_logged_in(Path::new(disk)) || self.from_snapshot;
        self.checking = true;
        self.stopped = false;
        self.error = None;
        if !self.from_snapshot && snapshot_ready(Path::new(disk)) {
            self.hold_finished(disk);
            return;
        }
        let _ = pointer;
        self.headline = Some(if self.from_snapshot {
            "Starting Debian.".into()
        } else {
            "Debian is already installed. Checking SSH.".into()
        });
        self.mark("installer", "done", None);
        self.mark("partition", "done", Some("Installed"));
        self.mark("ssh", "now", Some("guest port 22"));
    }

    fn buttons(&self) -> Vec<&'static str> {
        let mut buttons = Vec::new();
        if !self.from_snapshot {
            buttons.push("Open folder");
            buttons.push("Back");
        }
        if self.shows_installer() {
            buttons.push("Show installer");
        }
        if self.stopped {
            if !self.snapshot_ready {
                buttons.push(if self.installed {
                    "Start Debian"
                } else {
                    "Start from the guest disk"
                });
            }
            if self.snapshot_ready {
                buttons.push("New virtual machine");
            }
            if self.error.is_some() && self.installed && !self.snapshot_ready {
                buttons.push("Save snapshot");
            }
            if self.error.is_some() && !self.installed && !self.from_snapshot {
                buttons.push("Try again");
            }
        }
        buttons
    }
}

fn open_setup(screen: &Screen, dir: &Path) -> Setup {
    assert!(screen.installing());
    let disk = screen.disk_path.as_deref().unwrap();
    let written = guest_logged_in(Path::new(disk)) || disk_looks_installed(Path::new(disk));
    let saved = snapshot_ready(Path::new(disk));
    let mut page = setup_page(false);
    page.installed = written || saved;
    page.logged_in = guest_logged_in(Path::new(disk)) || saved;
    page.snapshot_ready = saved;
    if saved {
        page.stopped = true;
        page.headline =
            Some("Debian is installed and shut down. A snapshot is saved for new machines.".into());
        page.mark("installer", "done", None);
        page.mark("partition", "done", Some("Installed"));
        page.mark("ssh", "done", Some("debian"));
        page.mark("snapshot", "done", Some("Saved"));
    } else {
        page.headline = Some(if written {
            "Debian is already installed. Checking SSH.".into()
        } else {
            "Starting the installer.".into()
        });
    }
    page.appear(screen.ready_path.as_deref().unwrap(), disk);
    let _ = dir;
    page
}

fn open_clone(disk: &str) -> Setup {
    let mut page = setup_page(true);
    page.snapshot_ready = false;
    page.headline = Some("Starting Debian.".into());
    page.appear("", disk);
    page
}

#[test]
fn the_setup_page_hides_the_installer_once_the_snapshot_is_saved() {
    let (base, _) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, events, dir) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let screen = finish(&events, status);
    let mut setup = open_setup(&screen, &dir);
    assert_eq!(setup.title, "Debian setup");
    assert!(setup.shows_installer());
    assert!(!setup.captures_pointer());
    assert_eq!(setup.buttons(), ["Open folder", "Back", "Show installer"]);
    assert_eq!(setup.row("installer").state, "now");
    assert_eq!(setup.row("partition").state, "wait");
    setup.show_window = true;
    assert_eq!(setup.buttons(), ["Open folder", "Back", "Show installer"]);

    let disk = dir.join("machines/debian/disk.img");
    let mut file = std::fs::OpenOptions::new().write(true).open(&disk).unwrap();
    use std::io::{Seek, SeekFrom, Write};
    file.seek(SeekFrom::Start(510)).unwrap();
    file.write_all(&[0x55, 0xaa]).unwrap();
    drop(file);
    let written = open_setup(&screen, &dir);
    assert_eq!(
        written.headline.as_deref(),
        Some("Debian is already installed. Checking SSH.")
    );
    assert!(written.shows_installer());
    assert!(written.checking);
    assert!(!written.stopped);
    assert_eq!(written.buttons(), ["Open folder", "Back", "Show installer"]);
    assert!(
        written.attached.is_empty(),
        "a written disk does not attach boot.iso or preseed.img"
    );

    std::fs::write(dir.join("ssh/logged-in"), b"debian ok\n").unwrap();
    let saved = cli(&["snapshot", "--dir", dir.to_str().unwrap()]);
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stdout)
    );
    let done = finish_install(&saved.stdout);
    assert!(!done.shows_installer);
    assert_eq!(
        done.buttons(),
        ["Open folder", "Back", "New virtual machine"]
    );
    assert_eq!(done.row("installer").state, "done");
    assert_eq!(done.row("snapshot").detail, "Saved");
    let reopened = open_setup(&screen, &dir);
    assert!(reopened.snapshot_ready);
    assert!(!reopened.shows_installer());
    assert!(!reopened.checking);
    assert!(reopened.stopped);
    assert_eq!(
        reopened.buttons(),
        ["Open folder", "Back", "New virtual machine"]
    );
    assert_eq!(
        reopened.headline.as_deref(),
        Some("Debian is installed and shut down. A snapshot is saved for new machines.")
    );
}

/// Same choice as Install.onAppear after Quick start emits ready.
/// A partition table boots the disk alone and still checks SSH. It does not attach the installer or a pointer.
/// snapshots/debian/ready means that installer already shut down. Reopening does not start it.
fn reopen_boot(disk: &str) -> Reopen {
    let url = std::path::Path::new(disk);
    let logged_in = guest_logged_in(url);
    let written = logged_in || disk_looks_installed(url);
    let snapshot_saved = snapshot_ready(url);
    let starts_vm = !snapshot_saved;
    let attaches_installer = starts_vm && !written;
    Reopen {
        installed: written || snapshot_saved,
        attaches_installer,
        captures_pointer: false,
        checks_ssh: starts_vm && written,
        starts_vm,
        snapshot_saved,
        headline: Some(if snapshot_saved {
            "Debian is installed and shut down. A snapshot is saved for new machines.".into()
        } else if written {
            "Debian is already installed. Checking SSH.".into()
        } else {
            "Starting the installer.".into()
        }),
    }
}

fn static_step(id: &str) -> Option<&'static str> {
    [
        "key",
        "sums",
        "sign",
        "check",
        "download",
        "verify",
        "sshkey",
        "preseed",
        "disk",
        "patch",
        "installer",
        "partition",
        "ssh",
        "snapshot",
    ]
    .into_iter()
    .find(|item| *item == id)
}

fn snapshot_ready(disk: &std::path::Path) -> bool {
    disk.parent()
        .and_then(|dir| dir.parent())
        .and_then(|dir| dir.parent())
        .is_some_and(|support| {
            let snap = support.join("snapshots/debian");
            snap.join("disk.img").is_file()
                && std::fs::read_to_string(snap.join("ready"))
                    .ok()
                    .is_some_and(|text| text.starts_with("ssh-ok"))
        })
}

fn guest_logged_in(disk: &std::path::Path) -> bool {
    let beside = disk.parent().unwrap().join("logged-in");
    let shared = disk
        .parent()
        .and_then(|dir| dir.parent())
        .and_then(|dir| dir.parent())
        .unwrap()
        .join("ssh/logged-in");
    [beside, shared].into_iter().any(|path| {
        std::fs::read_to_string(path)
            .ok()
            .is_some_and(|text| text.starts_with("debian") && text.contains("ok"))
    })
}

fn disk_looks_installed(disk: &std::path::Path) -> bool {
    let mut file = match std::fs::File::open(disk) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut sector = [0u8; 512];
    std::io::Read::read_exact(&mut file, &mut sector).is_ok()
        && sector[510] == 0x55
        && sector[511] == 0xaa
}

#[derive(Debug)]
struct Reopen {
    installed: bool,
    attaches_installer: bool,
    captures_pointer: bool,
    checks_ssh: bool,
    starts_vm: bool,
    snapshot_saved: bool,
    headline: Option<String>,
}

#[test]
fn removing_a_setup_file_reruns_that_step() {
    let dir = dir();
    let disk = dir.join("machines/debian/disk.img");
    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
    std::fs::write(&disk, b"disk").unwrap();
    std::fs::create_dir_all(dir.join("ssh")).unwrap();
    std::fs::create_dir_all(dir.join("installer")).unwrap();
    std::fs::write(dir.join("installer/preseed.img"), b"preseed").unwrap();
    std::fs::write(dir.join("installer/boot.iso"), b"boot").unwrap();
    std::fs::create_dir_all(dir.join("snapshots/debian")).unwrap();
    std::fs::write(dir.join("ssh/logged-in"), b"debian ok\n").unwrap();
    std::fs::write(disk.parent().unwrap().join("logged-in"), b"debian ok\n").unwrap();
    std::fs::write(dir.join("snapshots/debian/ready"), b"ssh-ok\n").unwrap();
    std::fs::write(dir.join("snapshots/debian/disk.img"), b"snap").unwrap();

    let mut setup = setup_page(false);
    setup.snapshot_ready = true;
    setup.logged_in = true;
    setup.installed = true;
    setup.stopped = true;
    setup.mark("ssh", "done", Some("debian"));
    setup.mark("snapshot", "done", Some("Saved"));
    assert_eq!(setup.row("installer").state, "done");

    std::fs::remove_file(dir.join("snapshots/debian/disk.img")).unwrap();
    assert_eq!(setup.note_missing(&disk), Some("snapshot"));
    assert_eq!(setup.row("snapshot").state, "now");
    assert_eq!(setup.row("ssh").state, "done");
    assert!(!setup.snapshot_ready);
    assert_eq!(
        setup.headline.as_deref(),
        Some("SSH login succeeded. Saving a snapshot.")
    );

    std::fs::write(dir.join("snapshots/debian/disk.img"), b"snap").unwrap();
    setup.snapshot_ready = true;
    setup.mark("snapshot", "done", Some("Saved"));
    std::fs::remove_file(dir.join("ssh/logged-in")).unwrap();
    std::fs::remove_file(disk.parent().unwrap().join("logged-in")).unwrap();
    assert_eq!(setup.note_missing(&disk), Some("ssh"));
    assert_eq!(setup.row("ssh").state, "now");
    assert!(setup.checking);
    assert_eq!(setup.row("snapshot").state, "done");
    assert!(setup.snapshot_ready);
    assert_eq!(
        setup.headline.as_deref(),
        Some("Debian is already installed. Checking SSH.")
    );

    setup.checking = false;
    setup.mark("ssh", "done", Some("debian"));
    std::fs::remove_file(dir.join("snapshots/debian/ready")).unwrap();
    // The login file is still gone, so that step runs too. It is the one that has to boot.
    assert_eq!(setup.note_missing(&disk), Some("ssh"));
    assert_eq!(setup.row("snapshot").state, "wait");
    assert_eq!(setup.row("ssh").state, "now");
    assert_eq!(
        setup.headline.as_deref(),
        Some("Debian is already installed. Checking SSH.")
    );

    // The installer file goes away while that step is still the current one. The download step that wrote it runs again.
    std::fs::write(dir.join("ssh/logged-in"), b"debian ok\n").unwrap();
    std::fs::write(dir.join("snapshots/debian/ready"), b"ssh-ok\n").unwrap();
    setup.checking = false;
    setup.snapshot_ready = true;
    setup.mark("ssh", "done", Some("debian"));
    setup.mark("snapshot", "done", Some("Saved"));
    std::fs::remove_file(dir.join("installer/preseed.img")).unwrap();
    assert_eq!(setup.note_missing(&disk), Some("preseed"));
    assert_eq!(setup.missing_file.as_deref(), Some("preseed"));
    assert_eq!(setup.row("ssh").state, "done");
    std::fs::write(dir.join("installer/preseed.img"), b"preseed").unwrap();
    std::fs::remove_file(dir.join("installer/boot.iso")).unwrap();
    assert_eq!(
        setup.note_missing(&disk),
        Some("preseed"),
        "the first missing file stays until that step runs again"
    );
}

#[test]
fn quick_start_swift_gates_the_bar_on_a_real_download() {
    let src = include_str!("../Sources/QuickStart.swift");
    assert!(src.contains("if event.event == \"progress\", event.step == \"download\""));
    assert!(src.contains("if wizard.downloading {"));
    assert!(src.contains("steps = WizardStep.initial + WizardStep.preseed + WizardStep.setup"));
    assert!(!src.contains("showPreseed"));
    assert!(src.contains("static let brief = [\"download\", \"partition\", \"snapshot\"]"));
    assert!(src.contains("Button(wizard.showAll ? \"Show important steps\" : \"Show all steps\")"));
    assert!(src.contains("Install(iso: iso, disk: disk, embedded: true, steps: $wizard.steps, missingFile: $wizard.missingFile)"));
    assert!(src.contains("Button(\"Open folder\")"));
    assert!(src.contains("support.appendingPathComponent(\"installer/preseed.img\")"));
    assert!(src.contains("support.appendingPathComponent(\"installer/boot.iso\")"));
    assert!(!src.contains("preseedSteps"));
    assert!(!include_str!("../Sources/Install.swift").contains("preseedSteps"));
    assert!(include_str!("../Sources/Install.swift").contains("let shown = guest.setupSteps"));
    assert!(src.contains("args += [\"--from\", resume]"));
    assert!(src.contains("support.appendingPathComponent(\"installer/preseed.img\")"));
    assert!(src.contains("support.appendingPathComponent(\"installer/boot.iso\")"));
    assert!(include_str!("../Sources/Install.swift")
        .contains("appendingPathComponent(\"installer\", isDirectory: true)"));
    assert!(src.contains("firstMissing"));
    assert!(include_str!("../Sources/Install.swift").contains("func noteMissingFiles"));
    assert!(
        include_str!("../Sources/Install.swift").contains("SupportDir.snapshotReady && written")
    );
    assert!(src.contains("Button(\"Open folder\")"));
    assert!(include_str!("../Sources/Install.swift").contains("SupportDir.open()"));
    assert!(!src.contains("Button(\"Back\""));
    assert!(!src.contains("Button(\"Next\""));
    assert!(src.contains("static let preseed"));
    assert!(include_str!("../Sources/Install.swift").contains("Debian setup"));
    assert!(include_str!("../Sources/Install.swift").contains("missingFile"));
    assert!(include_str!("../Sources/Install.swift")
        .contains("private var showsGuest: Bool { setup && !guest.snapshotReady }"));
    assert!(include_str!("../Sources/Install.swift").contains("if !guest.snapshotReady {"));
    assert!(include_str!("../Sources/Install.swift").contains("GuestMirror"));
    assert!(
        include_str!("../Sources/Install.swift").contains("func attach(_ screen: InstallScreen)")
    );
    assert!(include_str!("../Sources/Install.swift").contains(".nonactivatingPanel"));
    assert!(include_str!("../Sources/Install.swift").contains("becomesKeyOnlyIfNeeded = true"));
    assert!(include_str!("../Sources/Install.swift").contains("struct InstallerWindow"));
    assert!(!include_str!("../Sources/Install.swift").contains(".popover(isPresented:"));
    assert!(include_str!("../Sources/Install.swift").contains("frame(width: setup ? 440 : 848"));
    assert!(!include_str!("../Sources/Install.swift").contains("frame(height: 96)"));
    assert!(include_str!("../Sources/Install.swift")
        .contains("private var capturePointer: Bool { fromSnapshot }"));
    assert!(include_str!("../Sources/Install.swift").contains("pointer: false"));
    assert!(include_str!("../Sources/Guest.swift").contains("pointer: Bool = false"));
    assert!(include_str!("../Sources/Install.swift").contains("InstalledDisk.looksInstalled"));
    assert!(include_str!("../Sources/Guest.swift").contains("func looksInstalled"));
    assert!(include_str!("../Sources/Install.swift").contains("guest.setupSteps"));
    assert!(include_str!("../Sources/QuickStart.swift").contains("static let setup"));
    assert!(include_str!("../Sources/Install.swift").contains("guest.start(iso: iso, disk: disk)"));
    assert!(include_str!("../Sources/Install.swift")
        .contains("Debian is installed and shut down. A snapshot is saved for new machines."));
    assert!(include_str!("../Sources/Install.swift").contains("func holdFinished"));
    assert!(include_str!("../Sources/Install.swift").contains("SupportDir.snapshotReady"));
    assert!(include_str!("../Sources/Install.swift").contains("checking = true"));
    assert!(include_str!("../Sources/Guest.swift").contains("VZNATNetworkDeviceAttachment()"));
    assert!(!include_str!("../Sources/Guest.swift").contains("VZNATNetworkDeviceForwardingRules"));
    assert!(include_str!("../Sources/Guest.swift").contains("192.168.64."));
    assert!(include_str!("../Sources/Guest.swift").contains("for n in 2...254"));
    assert!(include_str!("../Sources/Guest.swift").contains("ConnectTimeout=1"));
    assert!(include_str!("../Sources/Guest.swift").contains("terminationHandler"));
    assert!(include_str!("../Sources/Install.swift").contains("self?.watchSSH()"));
    assert!(!include_str!("../Sources/Install.swift").contains("machine?.state == .running"));
    assert!(include_str!("../Sources/Guest.swift").contains("func forget(beside disk: URL)"));
    assert!(include_str!("../Sources/Install.swift")
        .contains("SSH login succeeded. Saving a snapshot."));
    assert!(include_str!("../Sources/Install.swift").contains("New virtual machine"));
    assert!(include_str!("../Sources/Guest.swift").contains("GuestSSH"));
    assert!(
        include_str!("../Sources/Guest.swift").contains("guest port 22")
            || include_str!("../Sources/Guest.swift").contains("sshPort: UInt16 = 22")
    );
    assert!(
        include_str!("../Sources/QuickStart.swift").contains("[\"snapshot\", \"--dir\", dir.path]")
    );
    assert!(include_str!("../Sources/Welcome.swift").contains("New Debian"));
    assert!(include_str!("../Sources/App.swift").contains("id: \"debian\""));
    assert!(!src.contains("wizard.downloading && wizard.total > 0"));
    assert!(src.contains("Connecting..."));
    assert!(src.contains("scaleEffect(x: wizard.fraction"));
    assert!(src.contains(".animation(nil, value: wizard.fraction)"));
    assert!(!src.contains("ProgressView(value: wizard.fraction)"));
    assert!(src.contains("if wizard.total > 0 {"));
    assert!(src.contains("downloading = false"));
    let apply = src.split("private func apply(").nth(1).unwrap();
    let ready = apply.split("if event.event == \"ready\"").nth(1).unwrap();
    assert!(ready
        .split("case \"error\"")
        .next()
        .unwrap()
        .contains("if downloading {"));
}
