//! End-to-end worker run. A local server stands in for Debian.
//! The signature is checked before the ISO is fetched.
//! Quick start is replayed from that JSON so the window stays tied to it.

use std::collections::HashMap;
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
const EXAMPLE: &[u8] = b"#_preseed_V1\nd-i debian-installer/locale string en_US.UTF-8\n";

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
    let mut files = vec![
        ("/CD/key-DA87E80D6294BE9B.txt", key.to_vec()),
        ("/releases/trixie/example-preseed.txt", EXAMPLE.to_vec()),
    ];
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
    assert!(text.contains("--rebuild"));

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
    let name = cli(&["name", "--help"]);
    assert!(
        name.status.success(),
        "{}",
        String::from_utf8_lossy(&name.stderr)
    );
    assert!(String::from_utf8_lossy(&name.stdout).contains("--name"));
    let pause = cli(&["pause", "--help"]);
    assert!(
        pause.status.success(),
        "{}",
        String::from_utf8_lossy(&pause.stderr)
    );
    assert!(String::from_utf8_lossy(&pause.stdout).contains("--disk"));
    let unpause = cli(&["unpause", "--help"]);
    assert!(
        unpause.status.success(),
        "{}",
        String::from_utf8_lossy(&unpause.stderr)
    );
    assert!(String::from_utf8_lossy(&unpause.stdout).contains("--disk"));
    let ssh = cli(&["ssh", "--help"]);
    assert!(
        ssh.status.success(),
        "{}",
        String::from_utf8_lossy(&ssh.stderr)
    );
    assert!(String::from_utf8_lossy(&ssh.stdout).contains("--disk"));
}

#[test]
fn ssh_probe_is_true_only_after_login() {
    let dir = dir();
    let disk = dir.join("snapshots/debian/disk.img");
    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
    std::fs::write(&disk, b"disk").unwrap();
    let missing = cli(&["ssh", "--disk", disk.to_str().unwrap()]);
    assert!(!missing.status.success());
    std::fs::create_dir_all(dir.join("ssh")).unwrap();
    std::fs::write(dir.join("ssh/ssh-ok"), b"debian ok\n").unwrap();
    let ok = cli(&["ssh", "--disk", disk.to_str().unwrap()]);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stdout)
    );
    let event = one_event(&ok.stdout);
    assert_eq!(event["event"], "ssh");
    assert_eq!(event["state"], "done");
}

fn spawn(base: &str, dir: &Path) -> std::process::Child {
    spawn_args(base, dir, &["download", "--dir", dir.to_str().unwrap()])
}

/// Reset. The download cache stays. Preseed, the guest disk, and the snapshot are done again.
fn rebuild(base: &str, dir: &Path) -> std::process::Child {
    spawn_args(
        base,
        dir,
        &["download", "--rebuild", "--dir", dir.to_str().unwrap()],
    )
}

fn spawn_args(base: &str, _dir: &Path, args: &[&str]) -> std::process::Child {
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
            "/releases/trixie/example-preseed.txt".into(),
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
        "preseed.cfg",
        "download.lock",
    ] {
        assert!(cache.join(file).is_file(), "{file}");
    }
    assert_eq!(std::fs::read(cache.join("preseed.cfg")).unwrap(), EXAMPLE);
    assert!(std::fs::read(dir.join("installer/preseed.img"))
        .unwrap()
        .windows(b"d-i preseed/include string debian.cfg\n".len())
        .any(|word| word == b"d-i preseed/include string debian.cfg\n"));
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
    assert_eq!(download.steps.len(), 13);
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
    assert_eq!(screen.row("download").state, "done");
    assert_eq!(screen.row("verify").state, "done");
    assert_eq!(screen.row("ssh").state, "wait");
    assert_eq!(screen.row("snapshot").state, "wait");
    assert_eq!(
        screen.disk_path.as_deref(),
        Some(dir.join("snapshots/debian/disk.img").to_str().unwrap())
    );
    assert_eq!(screen.page, 0, "the wizard is one page");
    assert_eq!(screen.title(), "Debian 13");
    assert_eq!(screen.steps.len(), 13);
    assert_eq!(screen.page_rows(), ["download", "partition", "snapshot"]);
    assert_eq!(screen.shown("download"), "done");
    assert_eq!(screen.status("download"), "Downloaded");
    assert_eq!(screen.shown("partition"), "now");
    assert_eq!(screen.status("partition"), "Installing");
    assert_eq!(screen.shown("snapshot"), "wait");
    assert_eq!(screen.status("snapshot"), "Waiting");
    // The three words follow the work. A hidden check does not add a row.
    let mut words = Screen::new();
    words.start();
    assert_eq!(
        (
            words.status("download"),
            words.status("partition"),
            words.status("snapshot")
        ),
        ("Waiting", "Waiting", "Waiting")
    );
    for event in &events {
        words.apply(event);
        for id in ["download", "partition", "snapshot"] {
            let word = words.status(id);
            assert!(!word.contains('/'), "{id} {word}");
            assert!(
                matches!(
                    word,
                    "Waiting" | "Downloading" | "Downloaded" | "Failed" | "Installing" | "Installed" | "Saving" | "Saved"
                ),
                "{id} {word}"
            );
        }
        assert_eq!(words.page_rows(), ["download", "partition", "snapshot"]);
    }
    assert_eq!(words.status("download"), "Downloaded");
    assert_eq!(words.status("partition"), "Installing");
    assert_eq!(words.status("snapshot"), "Waiting");
    // Open waits for the tick. Download ISO is done, so it has the button. The guest disk and snapshot are already on disk, but those steps have not ticked.
    assert_eq!(screen.row_buttons("download", &dir), [] as [&str; 0]);
    assert_eq!(screen.row_buttons("partition", &dir), [] as [&str; 0]);
    assert_eq!(screen.row_buttons("snapshot", &dir), [] as [&str; 0]);
    assert!(dir.join("snapshots/debian/disk.img").is_file());
    assert!(
        !dir.join("machines").exists(),
        "the install disk is the snapshot, not a machine"
    );
    screen.show_log = true;
    assert_eq!(screen.row_buttons("key", &dir), [] as [&str; 0]);
    assert_eq!(screen.row_buttons("check", &dir), [] as [&str; 0]);
    assert_eq!(screen.row_buttons("ssh", &dir), [] as [&str; 0]);
    assert_eq!(screen.page_rows(), ["download", "partition", "snapshot"]);
    let patch_now = events
        .iter()
        .position(|event| {
            event["event"] == "step" && event["step"] == "patch" && event["state"] == "now"
        })
        .unwrap();
    let download_done = events
        .iter()
        .position(|event| {
            event["event"] == "step" && event["step"] == "download" && event["state"] == "done"
        })
        .unwrap();
    let before_iso = drive(&events[..download_done]);
    assert_eq!(before_iso.row("download").state, "now");
    assert_eq!(before_iso.row_buttons("download", &dir), [] as [&str; 0]);
    let iso_ticked = drive(&events[..=download_done]);
    assert_eq!(iso_ticked.row("download").state, "done");
    assert_eq!(iso_ticked.row_buttons("download", &dir), [] as [&str; 0]);
    assert_eq!(iso_ticked.row_buttons("partition", &dir), [] as [&str; 0]);
    assert_eq!(iso_ticked.row_buttons("snapshot", &dir), [] as [&str; 0]);
    let mut mid = drive(&events[..=patch_now]);
    assert_eq!(mid.page, 0);
    assert_eq!(mid.title(), "Debian 13");
    assert_eq!(mid.visible_ids(), ["download", "partition", "snapshot"]);
    assert_eq!(mid.row("patch").state, "now");
    assert_eq!(mid.shown("download"), "done");
    assert_eq!(mid.shown("partition"), "now");
    assert_eq!(mid.shown("snapshot"), "wait");
    assert_eq!(mid.buttons(), ["Reset", "Cancel"]);
    mid.show_log = true;
    assert_eq!(mid.visible_ids(), ["download", "partition", "snapshot"]);
    for event in &events[patch_now + 1..] {
        mid.apply(event);
    }
    assert_eq!(mid.page, 0, "ready does not open another page");
    assert!(mid.installing());
    assert_eq!(mid.buttons(), ["Reset"]);
    assert_eq!(mid.row("patch").state, "done");
    assert_eq!(mid.page_rows(), ["download", "partition", "snapshot"]);
    mid.show_log = false;
    assert_eq!(mid.page_rows(), ["download", "partition", "snapshot"]);
    assert_eq!(screen.row("download").state, "done");
    assert_eq!(screen.row("verify").state, "done");
    assert_eq!(screen.row("preseed").state, "done");
    assert_eq!(screen.row("patch").state, "done");
    assert_eq!(screen.row("disk").state, "done");
    assert!(screen.installing());
    assert_eq!(screen.page, 0);
    let setup = open_setup(&screen, &dir);
    assert_eq!(setup.title, "Debian setup");
    assert!(setup.shows_installer());
    assert_eq!(setup.thumbnail_row(), Some("partition"));
    assert_eq!(setup.buttons(), [] as [&str; 0]);
    assert_eq!(setup.row("partition").state, "now");
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
        std::fs::metadata(dir.join("snapshots/debian/disk.img"))
            .unwrap()
            .len(),
        16 * 1024 * 1024 * 1024
    );
    let key = dir.join("ssh/authorized_keys");
    assert!(key.is_file(), "host key was not generated");
    assert!(std::fs::read_to_string(&key)
        .unwrap()
        .contains("ssh-ed25519"));
    assert!(!dir.join("snapshots/debian/marker.img").exists());
    let ready = events.iter().find(|e| e["event"] == "ready").unwrap();
    assert_eq!(ready["key"], key.display().to_string());
    assert_eq!(screen.row("preseed").state, "done");

    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    let log = log_of(&world);
    assert!(log.is_empty(), "cached files were fetched again: {log:?}");
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
    assert_eq!(cached.row("download").state, "done");
    assert_eq!(cached.row("snapshot").state, "wait");
    assert!(cached.installing());
    assert_eq!(cached.page, 0);
    assert_eq!(cached.row("preseed").state, "done");
    assert_eq!(cached.row("sshkey").state, "done");
    assert_eq!(cached.row("disk").state, "done");
    assert_eq!(cached.row("patch").state, "done");
    assert_eq!(cached.buttons(), ["Reset"]);
    assert_eq!(cached.page_rows(), ["download", "partition", "snapshot"]);
    cached.show_log = true;
    assert_eq!(cached.page_rows(), ["download", "partition", "snapshot"]);
    let setup = open_setup(&cached, &dir);
    assert!(setup.shows_installer());
    assert_eq!(setup.thumbnail_row(), Some("partition"));
    assert_eq!(setup.row("partition").state, "now");
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
    let screen = finish(&events, status);
    let error = screen.error.clone().unwrap();
    assert!(!screen.bar_visible());
    assert_eq!(screen.headline.as_deref(), Some("Download failed"));
    assert_eq!(screen.buttons(), ["Reset", "Try again"]);
    assert_eq!(screen.row("check").state, "fail");
    assert_eq!(screen.shown("download"), "fail");
    assert_eq!(screen.page_rows(), ["download", "partition", "snapshot"]);
    assert!(error.contains("SHA512SUMS was not signed"));
    assert_eq!(screen.row("key").state, "done");
    assert_eq!(screen.row("download").state, "wait");
    let cache = dir.join("cache");
    let frozen: Vec<_> = screen
        .steps
        .iter()
        .map(|step| step.state.clone())
        .collect();
    std::fs::remove_dir_all(&cache).unwrap();
    assert_eq!(
        screen
            .steps
            .iter()
            .map(|step| step.state.clone())
            .collect::<Vec<_>>(),
        frozen,
        "deleting the cache does not start again"
    );
    assert_eq!(screen.buttons(), ["Reset", "Try again"]);
    let mut idle = Screen::new();
    idle.start();
    assert!(idle.running);
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
    assert!(!error.is_empty());
    assert!(!screen.bar_visible());
    assert_eq!(screen.row("key").state, "fail");
    assert_eq!(screen.shown("download"), "fail");
    assert_eq!(screen.shown("partition"), "wait");
    assert!(screen.steps.iter().skip(1).all(|step| step.state == "wait"));
    assert_eq!(screen.buttons(), ["Reset", "Try again"]);
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
    assert!(screen.bar_visible());
    assert_eq!(screen.percent(), 100);
    assert_eq!(screen.row("verify").state, "fail");
    assert_eq!(screen.shown("download"), "fail");
    assert!(screen.ready_path.is_none());
    assert_eq!(screen.buttons(), ["Reset", "Try again"]);
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
    assert_eq!(screen.row("download").title, "Download ISO");
    assert_eq!(screen.buttons(), ["Reset", "Try again"]);
    screen.retry();
    assert!(!screen.bar_visible());
    assert_eq!(screen.received, 0.0);
    assert!(screen.steps.iter().all(|step| step.state == "wait"));
    assert_eq!(screen.buttons(), ["Reset", "Cancel"]);
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
    assert!(error.contains("a download is already running"));
    assert!(screen.steps.iter().skip(1).all(|step| step.state == "wait"));
    assert_eq!(screen.buttons(), ["Reset", "Try again"]);
    assert_eq!(screen.headline.as_deref(), Some("Download failed"));
}

/// Quick start window. Same rules as Sources/QuickStart.swift.
#[derive(Debug, Clone)]
struct Step {
    id: &'static str,
    title: &'static str,
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
    cancelled: bool,
    downloading: bool,
    /// True only for the run Reset just started. A later open does not set this.
    rebuild: bool,
    /// Set when start() consumes rebuild, which is the worker's --rebuild.
    passed_rebuild: bool,
    /// Changes on Reset so the mounted install is a new guest.
    run_id: i32,
    started: bool,
    running: bool,
    page: i32,
    /// Kept so older checks can set it. The page does not grow a log of hidden steps.
    show_log: bool,
}

fn face_of(id: &str) -> &'static str {
    match id {
        "sshkey" | "preseed" | "disk" | "patch" | "partition" | "ssh" => "partition",
        "snapshot" => "snapshot",
        _ => "download",
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
                    state: "wait".into(),
                },
                Step {
                    id: "sums",
                    title: "Download checksums file",
                    state: "wait".into(),
                },
                Step {
                    id: "sign",
                    title: "Download signature file",
                    state: "wait".into(),
                },
                Step {
                    id: "check",
                    title: "Verify signature",
                    state: "wait".into(),
                },
                Step {
                    id: "download",
                    title: "Download ISO",
                    state: "wait".into(),
                },
                Step {
                    id: "verify",
                    title: "Verify checksum",
                    state: "wait".into(),
                },
                Step {
                    id: "sshkey",
                    title: "Prepare the host key",
                    state: "wait".into(),
                },
                Step {
                    id: "preseed",
                    title: "Prepare preseed disk",
                    state: "wait".into(),
                },
                Step {
                    id: "disk",
                    title: "Prepare the guest disk",
                    state: "wait".into(),
                },
                Step {
                    id: "patch",
                    title: "Prepare installer",
                    state: "wait".into(),
                },
                Step {
                    id: "partition",
                    title: "Install guest",
                    state: "wait".into(),
                },
                Step {
                    id: "ssh",
                    title: "Verify with host key",
                    state: "wait".into(),
                },
                Step {
                    id: "snapshot",
                    title: "Create snapshot",
                    state: "wait".into(),
                },
            ],
            received: 0.0,
            total: 0.0,
            headline: None,
            error: None,
            ready_path: None,
            disk_path: None,
            cancelled: false,
            downloading: false,
            rebuild: false,
            passed_rebuild: false,
            run_id: 0,
            started: false,
            running: false,
            page: 0,
            show_log: false,
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
        self.disk_path = None;
        // Reset already set this. Consuming it here is what the worker sees as --rebuild.
        // A later open calls start() with it false and reuses the disk.
        let rebuilding = self.rebuild;
        self.rebuild = false;
        if rebuilding {
            self.passed_rebuild = true;
        }
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
        let run_id = self.run_id;
        *self = Self::new();
        // The caller sets rebuild after this. Clearing it here made Reset reuse the stuck disk.
        self.run_id = run_id;
        self.passed_rebuild = false;
    }

    fn retry(&mut self) {
        self.running = false;
        self.reset();
        self.start();
    }

    /// Reset keeps the download cache and rebuilds the image. Deleting a file does not.
    fn reset_wizard(&mut self) {
        self.running = false;
        self.reset();
        self.rebuild = true;
        self.run_id += 1;
        self.start();
    }

    fn cancel(&mut self) {
        self.cancelled = true;
        self.running = false;
        self.error = None;
        self.headline = Some("Cancelled. The download was not saved.".into());
        if let Some(step) = self.steps.iter_mut().find(|step| step.state == "now") {
            step.state = "wait".into();
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
            "step" | "ready" => {
                if let (Some(id), Some(state)) = (step, event["state"].as_str()) {
                    self.apply_state(id, state);
                }
                if kind == "ready" {
                    self.ready_path = event["path"].as_str().map(str::to_string);
                    self.disk_path = event["disk"].as_str().map(str::to_string);
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
                        self.apply_state(id, "fail");
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

    /// Waiting, Downloading, Downloaded. A hidden step only changes this word.
    fn status(&self, id: &str) -> &'static str {
        match (id, self.shown(id)) {
            ("download", "now") => "Downloading",
            ("download", "done") => "Downloaded",
            ("download", "fail") => "Failed",
            ("partition", "now") => "Installing",
            ("partition", "done") => "Installed",
            ("partition", "fail") => "Failed",
            ("snapshot", "now") => "Saving",
            ("snapshot", "done") => "Saved",
            ("snapshot", "fail") => "Failed",
            _ => "Waiting",
        }
    }

    /// Download ISO, Install guest, and Create snapshot. The other steps still run.
    fn visible_ids(&self) -> Vec<&'static str> {
        let _ = self.show_log;
        ["download", "partition", "snapshot"]
            .into_iter()
            .filter(|id| self.steps.iter().any(|step| step.id == *id))
            .collect()
    }

    /// The row the user sees. A hidden check uses the row it belongs to.
    fn shown(&self, id: &str) -> &str {
        let group: Vec<_> = self
            .steps
            .iter()
            .filter(|step| face_of(step.id) == id)
            .collect();
        if group.iter().any(|step| step.state == "fail") {
            return "fail";
        }
        if group.iter().any(|step| step.state == "now") {
            return "now";
        }
        if !group.is_empty() && group.iter().all(|step| step.state == "done") {
            return "done";
        }
        if !self.cancelled && group.iter().any(|step| step.state == "done") {
            return "now";
        }
        "wait"
    }

    fn apply_state(&mut self, id: &str, state: &str) {
        let Some(row) = self.steps.iter_mut().find(|row| row.id == id) else {
            return;
        };
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

    fn row(&self, id: &str) -> &Step {
        self.steps
            .iter()
            .find(|step| step.id == id)
            .unwrap_or_else(|| panic!("missing {id}"))
    }

    /// The installer sits on Install guest, not in a slot above the list.
    /// The download bar sits on Download ISO. A saved snapshot has nothing to install.
    fn shows_thumbnail(&self, id: &str, dir: &Path, snapshot_saved: bool, window_open: bool) -> bool {
        let _ = dir;
        id == "partition" && !snapshot_saved && !window_open && self.error.is_none()
    }

    /// Nothing sits above the steps. The bar and the installer are inside their rows.
    fn top_slot(&self, snapshot_saved: bool, window_open: bool) -> &'static str {
        let _ = (snapshot_saved, window_open);
        "empty"
    }

    /// A row has no Open button. The path is the label.
    fn row_buttons(&self, id: &str, dir: &Path) -> Vec<&'static str> {
        let _ = (id, dir);
        vec![]
    }

    fn bar_visible(&self) -> bool {
        self.downloading
    }

    fn shows_percent(&self) -> bool {
        self.bar_visible() && self.total > 0.0
    }

    fn buttons(&self) -> Vec<&'static str> {
        let mut buttons = vec!["Reset"];
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
        if kind == "progress" && step == Some("download") {
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
        Some(dir.join("snapshots/debian/disk.img").to_str().unwrap())
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

    let disk = dir.join("snapshots/debian/disk.img");
    let mut file = std::fs::OpenOptions::new().write(true).open(&disk).unwrap();
    use std::io::{Seek, SeekFrom, Write};
    file.seek(SeekFrom::Start(510)).unwrap();
    file.write_all(&[0x55, 0xaa]).unwrap();
    file.seek(SeekFrom::Start(4096)).unwrap();
    file.write_all(b"GUEST-BYTES!").unwrap();
    drop(file);
    std::fs::write(dir.join("snapshots/debian/nvram.bin"), b"efi").unwrap();
    let refused = cli(&["snapshot", "--dir", dir.to_str().unwrap()]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stdout).contains("SSH has not logged in"));
    std::fs::write(dir.join("ssh/ssh-ok"), b"debian ok\n").unwrap();

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
    // The guest disk is the snapshot. Removing it does not leave a second copy to restore.
    std::fs::remove_file(dir.join("snapshots/debian/disk.img")).unwrap();
    let recopied = cli(&["snapshot", "--dir", dir.to_str().unwrap()]);
    assert!(!recopied.status.success());
    assert!(String::from_utf8_lossy(&recopied.stdout).contains("guest disk is missing"));
    let mut restored = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .open(&disk)
        .unwrap();
    restored.set_len(16 * 1024 * 1024 * 1024).unwrap();
    restored.seek(SeekFrom::Start(510)).unwrap();
    restored.write_all(&[0x55, 0xaa]).unwrap();
    drop(restored);
    let installed = finish_install(&saved.stdout);
    assert!(installed.error.is_none());
    assert!(installed.snapshot_ready);
    assert_eq!(
        installed.headline.as_deref(),
        Some("Debian is installed and shut down. A snapshot is saved for new machines.")
    );
    assert_eq!(
        installed.buttons(),
        ["New virtual machine"]
    );
    assert!(
        !installed.shows_installer,
        "a saved snapshot hides the thumbnail"
    );
    assert_eq!(installed.row("snapshot").state, "done");
    assert_eq!(installed.row("ssh").state, "done");

    let mut src = std::fs::OpenOptions::new().write(true).open(&disk).unwrap();
    src.seek(SeekFrom::Start(4096)).unwrap();
    src.write_all(b"CHANGED!!!!!").unwrap();
    drop(src);
    std::fs::write(dir.join("snapshots/debian/nvram.bin"), b"mutated").unwrap();
    let again = cli(&["snapshot", "--dir", dir.to_str().unwrap()]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stdout)
    );
    // The saved snapshot is this disk, so a later save keeps the bytes written here.
    let mut frozen = [0u8; 12];
    let mut snap = std::fs::File::open(dir.join("snapshots/debian/disk.img")).unwrap();
    snap.seek(SeekFrom::Start(4096)).unwrap();
    std::io::Read::read_exact(&mut snap, &mut frozen).unwrap();
    assert_eq!(&frozen, b"CHANGED!!!!!");
    assert_eq!(
        std::fs::read(dir.join("snapshots/debian/nvram.bin")).unwrap(),
        b"mutated"
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
    assert!(String::from_utf8_lossy(&installer.stdout).contains("snapshot name"));
    let checked = cli(&["name", "--dir", dir.to_str().unwrap(), "--name", "My Debian"]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stdout)
    );
    assert_eq!(one_event(&checked.stdout)["name"], "My Debian");
    let pretty = cli(&["clone", "--dir", dir.to_str().unwrap(), "--name", "My Debian"]);
    assert!(
        pretty.status.success(),
        "{}",
        String::from_utf8_lossy(&pretty.stdout)
    );
    assert_eq!(
        one_event(&pretty.stdout)["disk"],
        dir.join("machines/My Debian/disk.img").display().to_string()
    );
    let taken = cli(&["name", "--dir", dir.to_str().unwrap(), "--name", "My Debian"]);
    assert!(!taken.status.success());
    assert!(String::from_utf8_lossy(&taken.stdout).contains("already exists"));

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
    assert_eq!(&cloned_buf, b"CHANGED!!!!!");
    assert_eq!(
        std::fs::read(dir.join("machines/debian-2/nvram.bin")).unwrap(),
        b"mutated"
    );
    let cloned_machine = finish_clone(&cloned.stdout);
    assert_eq!(cloned_machine.disk.as_deref(), event["disk"].as_str());
    assert!(cloned_machine.error.is_none());
    let machine = open_clone(event["disk"].as_str().unwrap());
    // Adding the next machine is a new detail view. The clone that was started must keep running.
    let mut library = Library {
        machines: vec![event["disk"].as_str().unwrap().to_string()],
        selected: Some(event["disk"].as_str().unwrap().to_string()),
        sessions: HashMap::new(),
    };
    library.show();
    library.session(event["disk"].as_str().unwrap()).boot_disk(event["disk"].as_str().unwrap(), true);
    let next_disk = dir.join("machines/debian-3/disk.img").display().to_string();
    library.hide();
    library.clone_new(next_disk.clone());
    library.show();
    assert!(
        !library.session(event["disk"].as_str().unwrap()).stopped,
        "the worker clone must stay running when the library adds another machine"
    );
    assert_eq!(library.selected.as_deref(), Some(next_disk.as_str()));
    assert_eq!(machine.title, "Debian");
    assert!(!machine.captures_pointer());
    assert!(!machine.shows_installer());
    assert_eq!(machine.thumbnail_row(), None, "a clone is a machine, not the install thumbnail");
    assert_eq!(
        machine.headline.as_deref(),
        Some("Debian is stopped. The guest disk is the installed system.")
    );
    assert!(machine.stopped);
    assert!(!machine.paused);
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

    let mut started = open_clone(event["disk"].as_str().unwrap());
    started.boot_disk(event["disk"].as_str().unwrap(), true);
    assert!(!started.stopped);
    assert!(!started.paused);
    assert!(started.captures_pointer());
    assert_eq!(started.headline.as_deref(), Some("Starting Debian."));

    // The install disk is the snapshot. A broken copy cannot be rebuilt from a second file.
    std::fs::write(dir.join("snapshots/debian/ready"), b"nope").unwrap();
    std::fs::write(dir.join("snapshots/debian/disk.img"), b"short").unwrap();
    let replaced = cli(&["snapshot", "--dir", dir.to_str().unwrap()]);
    assert!(!replaced.status.success());
    assert!(String::from_utf8_lossy(&replaced.stdout).contains("guest disk is"));
}

/// Pause files sit beside the guest disk so a later open can resume instead of a cold boot.
#[test]
fn pause_marks_a_save_beside_the_disk() {
    let dir = dir();
    let disk = dir.join("machines/lab/disk.img");
    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
    std::fs::write(&disk, b"disk").unwrap();
    let missing = cli(&["pause", "--disk", disk.to_str().unwrap()]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stdout).contains("pause state"),
        "{}",
        String::from_utf8_lossy(&missing.stdout)
    );
    std::fs::write(disk.parent().unwrap().join("paused.vzvmsave"), b"vz").unwrap();
    let saved = cli(&["pause", "--disk", disk.to_str().unwrap()]);
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stdout)
    );
    let event = one_event(&saved.stdout);
    assert_eq!(event["event"], "pause");
    assert_eq!(event["state"], "done");
    assert!(dir.join("machines/lab/paused").is_file());
    std::fs::write(dir.join("machines/lab/paused.png"), b"png").unwrap();
    let machine = open_clone(disk.to_str().unwrap());
    assert!(machine.paused);
    assert!(machine.pause_preview);
    assert!(machine.stopped);
    assert!(!machine.captures_pointer());
    assert_eq!(machine.headline.as_deref(), Some("Debian is paused."));
    assert_eq!(machine.buttons(), [] as [&str; 0]);

    let mut resumed = open_clone(disk.to_str().unwrap());
    resumed.boot_disk(disk.to_str().unwrap(), true);
    assert!(!resumed.paused);
    assert!(!resumed.pause_preview);
    assert!(!resumed.stopped);
    assert!(resumed.captures_pointer());
    assert_eq!(
        resumed.headline.as_deref(),
        Some("Debian is running from the guest disk.")
    );

    let cleared = cli(&["unpause", "--disk", disk.to_str().unwrap()]);
    assert!(
        cleared.status.success(),
        "{}",
        String::from_utf8_lossy(&cleared.stdout)
    );
    assert!(!dir.join("machines/lab/paused").exists());
    assert!(!dir.join("machines/lab/paused.vzvmsave").exists());
    assert!(!dir.join("machines/lab/paused.png").exists());
    let again = open_clone(disk.to_str().unwrap());
    assert!(!again.paused);
    assert_eq!(
        again.headline.as_deref(),
        Some("Debian is stopped. The guest disk is the installed system.")
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
        page.mark("ssh", "done");
        page.mark("snapshot", "done");
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
        if !self.stopped {
            return Vec::new();
        }
        let mut buttons = Vec::new();
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

/// Same clone outcome as Install.swift `newMachine`.
struct Cloned {
    disk: Option<String>,
    error: Option<String>,
}

fn finish_clone(stdout: &[u8]) -> Cloned {
    match worker_disk(stdout) {
        Some(disk) => Cloned {
            disk: Some(disk),
            error: None,
        },
        None => Cloned {
            disk: None,
            error: Some(
                worker_error(stdout)
                    .unwrap_or_else(|| "Could not create a virtual machine.".into()),
            ),
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
        .map(|step| step.state.clone())
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
        .map(|step| step.state.clone())
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
    assert_eq!(screen.row("check").state, "done");
    assert_eq!(screen.buttons(), ["Reset", "Try again"]);
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
    assert!(screen.bar_visible());
    assert!(screen.ready_path.is_none());
    assert_eq!(screen.buttons(), ["Reset", "Try again"]);
    assert!(!screen.cancelled);
}

#[test]
fn deleting_a_file_does_not_start_again() {
    let (base, _) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, first, dir) = run(&base);
    assert_eq!(status, 0, "{first:?}");
    let screen = finish(&first, status);
    let before = screen.steps.iter().map(|step| step.state.clone()).collect::<Vec<_>>();
    std::fs::remove_file(dir.join("cache").join(iso_name())).unwrap();
    std::fs::remove_file(dir.join("snapshots/debian/disk.img")).unwrap();
    assert_eq!(
        screen.steps.iter().map(|step| step.state.clone()).collect::<Vec<_>>(),
        before,
        "deleting a file does not rewind the wizard"
    );
    assert!(screen.ready_path.is_some());
    assert_eq!(screen.buttons(), ["Reset"]);
    assert!(!screen.running);
}

#[test]
fn a_second_run_reuses_the_cache_and_rewrites_a_missing_disk() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, events, dir) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let disk = dir.join("snapshots/debian/disk.img");
    let screen = finish(&events, status);
    assert!(screen.installing());
    std::fs::remove_file(&disk).unwrap();
    std::fs::write(dir.join("ssh/ssh-ok"), b"debian ok\n").unwrap();
    let _ = log_of(&world);
    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    assert!(
        log_of(&world).is_empty(),
        "a missing guest disk does not download the ISO again"
    );
    assert!(events.iter().any(|event| event["step"] == "key" && event["state"] == "now"));
    assert!(events.iter().any(|event| event["step"] == "disk" && event["state"] == "now"));
    let screen = finish(&events, status);
    assert!(disk.is_file(), "prepare the guest disk must write disk.img again");
    assert_eq!(screen.disk_path.as_deref(), Some(disk.to_str().unwrap()));
    assert!(screen.installing());
    assert_eq!(screen.row("disk").state, "done");
    assert!(
        !std::fs::read_dir(dir.join("ssh"))
            .unwrap()
            .flatten()
            .any(|item| item.file_name() == "ssh-ok"),
        "a new empty disk must not inherit the previous login"
    );
}

#[test]
fn a_full_run_reuses_cached_files_and_rewrites_missing_ones() {
    let help = cli(&["download", "--help"]);
    assert!(!String::from_utf8_lossy(&help.stdout).contains("--from"));
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, events, dir) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let cache = dir.join("cache");
    std::fs::remove_file(dir.join("installer/preseed.img")).unwrap();
    std::fs::remove_file(dir.join("installer/boot.iso")).unwrap();
    std::fs::remove_file(cache.join("SHA512SUMS")).unwrap();
    let _ = log_of(&world);
    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    let log = log_of(&world);
    assert!(
        log.iter().any(|line| line.contains("SHA512SUMS")) && log.iter().all(|line| !line.contains(".iso")),
        "a missing checksum is fetched again, the ISO is not: {log:?}"
    );
    assert!(events.iter().any(|event| event["step"] == "key" && event["state"] == "now"));
    assert!(events.iter().any(|event| event["step"] == "preseed" && event["state"] == "now"));
    assert!(events.iter().any(|event| event["step"] == "patch" && event["state"] == "now"));
    let screen = finish(&events, status);
    assert!(dir.join("installer/preseed.img").is_file());
    assert!(dir.join("installer/boot.iso").is_file());
    assert!(!cache.join("preseed.img").exists());
    assert_eq!(screen.row("preseed").state, "done");
    assert_eq!(screen.row("patch").state, "done");
    assert!(screen.installing());
}

/// Reset keeps the download cache and rebuilds the image. A plain second run still reuses the disk.
#[test]
fn reset_keeps_the_download_cache_and_rebuilds_the_image() {
    let (base, world) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, events, dir) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let iso = std::fs::read(dir.join("cache").join(iso_name())).unwrap();
    let disk = dir.join("snapshots/debian/disk.img");
    std::fs::write(&disk, b"already installed").unwrap();
    std::fs::write(dir.join("snapshots/debian/ready"), b"ssh-ok\n").unwrap();
    std::fs::write(dir.join("installer/preseed.img"), b"old preseed").unwrap();
    std::fs::write(dir.join("installer/boot.iso"), b"old boot").unwrap();
    std::fs::write(dir.join("ssh/ssh-ok"), b"debian ok\n").unwrap();
    let finished = finish(&events, status);
    assert!(finished.installing());
    assert!(snapshot_ready(&disk), "a saved snapshot skips Install guest");
    assert!(!reopen_boot(disk.to_str().unwrap()).starts_vm);

    let _ = log_of(&world);
    let (status, again) = wait(rebuild(&base, &dir));
    assert_eq!(status, 0, "{again:?}");
    assert!(log_of(&world).is_empty(), "reset must not download the ISO again");
    assert_eq!(std::fs::read(dir.join("cache").join(iso_name())).unwrap(), iso);
    assert!(dir.join("cache/SHA512SUMS").is_file());
    assert!(dir.join("cache/SHA512SUMS.sign").is_file());
    assert!(dir.join("cache/key-DA87E80D6294BE9B.txt").is_file());
    assert_eq!(std::fs::metadata(&disk).unwrap().len(), 16 * 1024 * 1024 * 1024);
    assert_ne!(std::fs::read(dir.join("installer/preseed.img")).unwrap(), b"old preseed");
    assert_ne!(std::fs::read(dir.join("installer/boot.iso")).unwrap(), b"old boot");
    assert!(!dir.join("snapshots/debian/ready").exists());
    assert!(!dir.join("ssh/ssh-ok").exists(), "the old login does not belong to the new disk");
    for step in ["key", "sums", "sign", "check", "download", "verify", "sshkey", "preseed", "disk", "patch"] {
        assert!(again.iter().any(|event| event["step"] == step && event["state"] == "now"), "{step}");
    }
    let mut screen = finished;
    screen.reset_wizard();
    assert_eq!(screen.run_id, 1);
    assert!(screen.passed_rebuild, "Reset passes --rebuild; opening the window does not");
    assert!(!screen.rebuild, "the flag is consumed, so a later open reuses the disk");
    assert!(screen.steps.iter().all(|step| step.state == "wait"));
    for event in &again {
        screen.apply(event);
    }
    screen.worker_exited(status);
    assert!(screen.installing());
    assert_eq!(screen.disk_path.as_deref(), Some(disk.to_str().unwrap()));
    assert!(!snapshot_ready(&disk));
    let boot = reopen_boot(disk.to_str().unwrap());
    assert!(boot.starts_vm, "the rebuilt image starts the installer again");
    assert!(!boot.snapshot_saved);

    // A plain second run still reuses the disk, the preseed, and the installer copy.
    std::fs::write(&disk, b"kept").unwrap();
    std::fs::write(dir.join("installer/preseed.img"), b"kept preseed").unwrap();
    std::fs::write(dir.join("installer/boot.iso"), b"kept boot").unwrap();
    let _ = log_of(&world);
    let (status, reused) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{reused:?}");
    assert!(log_of(&world).is_empty());
    assert_eq!(std::fs::read(&disk).unwrap(), b"kept");
    assert_eq!(std::fs::read(dir.join("installer/preseed.img")).unwrap(), b"kept preseed");
    assert_eq!(std::fs::read(dir.join("installer/boot.iso")).unwrap(), b"kept boot");
    assert!(reused.iter().any(|event| event["step"] == "preseed" && event["state"] == "done"));
    let mut plain = Screen::new();
    plain.start();
    assert!(!plain.rebuild);
    assert!(!plain.passed_rebuild);
    assert_eq!(plain.run_id, 0);
}

#[test]
fn the_window_ignores_checksum_bytes_and_formats_its_own() {
    let mut screen = Screen::new();
    assert!(!screen.bar_visible());
    assert!(screen.headline.is_none());
    assert_eq!(screen.buttons(), ["Reset", "Cancel"]);
    assert_eq!(screen.row("key").title, "Download OpenPGP keyring");
    assert_eq!(screen.row("download").title, "Download ISO");
    assert_eq!(screen.row("verify").title, "Verify checksum");
    assert_eq!(screen.row("check").title, "Verify signature");
    assert_eq!(screen.row("ssh").title, "Verify with host key");
    assert_eq!(screen.row("partition").title, "Install guest");
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
    assert_eq!(screen.buttons(), ["Reset", "Try again"]);

    let mut missing = Screen::new();
    missing.worker_missing();
    assert_eq!(
        missing.error.as_deref(),
        Some("The worker is not built. Run cargo build --release, then try again.")
    );
    assert_eq!(missing.headline.as_deref(), Some("Could not start"));
    assert_eq!(missing.row("key").state, "fail");
    assert_eq!(missing.shown("download"), "fail");
    assert_eq!(missing.buttons(), ["Reset", "Try again"]);
    assert!(!missing.bar_visible());
    missing.worker_missing();
    assert_eq!(
        missing.row("key").state,
        "fail",
        "a second start does not run again"
    );
}

/// The row the user sees is its title and one status word. A path is not that word.
#[test]
fn a_worker_path_does_not_become_the_status_word() {
    let src = include_str!("../Sources/QuickStart.swift");
    assert!(!src.contains("func rowLabel"));
    assert!(!src.contains("WrappingPath(text: rowLabel"));
    assert!(src.contains("static func status"));
    let mut screen = Screen::new();
    assert_eq!(screen.status("download"), "Waiting");
    assert_eq!(screen.status("partition"), "Waiting");
    assert_eq!(screen.status("snapshot"), "Waiting");
    for (id, path) in [
        ("check", "cache/SHA512SUMS.sign"),
        ("verify", "cache/SHA512SUMS"),
        ("partition", "snapshots/debian/disk.img"),
        ("ssh", "snapshots/debian/ssh-ok"),
        ("download", "cache/debian-13.7.0-amd64-netinst.iso"),
    ] {
        let title = screen.row(id).title;
        screen.apply(&serde_json::json!({"event": "detail", "step": id, "detail": path}));
        screen.apply(&serde_json::json!({"event": "step", "step": id, "state": "now", "detail": format!("/tmp/{path}")}));
        assert_eq!(screen.row(id).title, title);
        assert_eq!(screen.row(id).state, "now");
        let word = screen.status(face_of(id));
        assert!(!word.contains('/'), "{word}");
        assert_ne!(word, "Waiting");
    }
    assert_eq!(screen.status("download"), "Downloading");
    screen.apply(&serde_json::json!({"event": "error", "step": "verify", "detail": "/tmp/cache/SHA512SUMS"}));
    assert_eq!(screen.row("verify").state, "fail");
    assert_eq!(screen.row("verify").title, "Verify checksum");
    assert_eq!(screen.status("download"), "Failed");
    screen.apply(&serde_json::json!({"event": "step", "step": "check", "state": "done", "detail": "Signature is valid"}));
    assert_eq!(screen.row("check").state, "done");
    assert_eq!(screen.row("check").title, "Verify signature");
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
    assert_eq!(screen.row("download").title, "Download ISO");
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
    assert!(
        again_log.is_empty(),
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
    screen.show_log = true;
    assert_eq!(screen.page_rows(), ["download", "partition", "snapshot"]);
}

/// Reopen is a new process. The ISO is cached, so the worker is fast, then the window
/// builds Install again. A partition table is not a finished guest. The wizard runs the installer again.
/// A saved snapshot is the finished installer. Reopening that disk does not start it.
#[test]
fn reopening_quick_start_boots_the_installed_disk() {
    let (base, _) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let dir = dir();
    let (status, events) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{events:?}");
    let disk = dir.join("snapshots/debian/disk.img");
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
    assert_eq!(boot.headline.as_deref(), Some("Starting the installer."));
    assert!(!boot.installed, "a partition table is not a finished guest");
    assert!(boot.attaches_installer, "{boot:?}");
    assert!(
        !boot.captures_pointer,
        "quick start starts the installer with no pointer"
    );
    assert!(!boot.checks_ssh);
    assert!(boot.starts_vm);

    std::fs::write(disk.parent().unwrap().join("ssh-ok"), b"debian ok\n").unwrap();
    let boot = reopen_boot(disk.to_str().unwrap());
    assert_eq!(boot.headline.as_deref(), Some("Starting the installer."));
    assert!(
        !boot.installed,
        "a leftover login file is not a finished guest"
    );
    assert!(boot.attaches_installer, "{boot:?}");
    assert!(!boot.checks_ssh);
    assert!(
        !boot.captures_pointer,
        "quick start starts the installer with no pointer"
    );
    assert!(!boot.snapshot_saved);
    assert!(boot.starts_vm);

    std::fs::create_dir_all(dir.join("snapshots/debian")).unwrap();
    std::fs::write(dir.join("snapshots/debian/ready"), b"ssh-ok\n").unwrap();
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
    std::fs::write(dir.join("ssh/ssh-ok"), b"debian ok\n").unwrap();
    let (status, fresh) = wait(spawn(&base, &dir));
    assert_eq!(status, 0, "{fresh:?}");
    assert!(
        !dir.join("ssh/ssh-ok").exists(),
        "recreating disk.img must drop the login that belonged to the deleted disk"
    );
    let boot = reopen_boot(dir.join("snapshots/debian/disk.img").to_str().unwrap());
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
    paused: bool,
    pause_preview: bool,
    /// Disks attached for a fresh install. Empty unless the installer starts. The signed ISO is not one of them.
    attached: Vec<String>,
}

fn setup_steps() -> Vec<Step> {
    vec![
        Step {
            id: "partition",
            title: "Install guest",
            state: "wait".into(),
        },
        Step {
            id: "ssh",
            title: "Verify with host key",
            state: "wait".into(),
        },
        Step {
            id: "snapshot",
            title: "Create snapshot",
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
        paused: false,
        pause_preview: false,
        attached: Vec::new(),
    }
}

impl Setup {
    fn row(&self, id: &str) -> &Step {
        self.steps.iter().find(|step| step.id == id).unwrap()
    }

    /// A row has no Open button.
    fn row_buttons(&self, id: &str, dir: &Path) -> Vec<&'static str> {
        let _ = (id, dir);
        vec![]
    }

    /// The installer is drawn on Install guest, not above the list.
    fn thumbnail_row(&self) -> Option<&'static str> {
        if self.shows_installer() {
            Some("partition")
        } else {
            None
        }
    }

    fn shows_installer(&self) -> bool {
        !self.from_snapshot && !self.snapshot_ready && !self.show_window
    }

    fn captures_pointer(&self) -> bool {
        self.from_snapshot && !self.stopped
    }

    fn mark(&mut self, id: &str, state: &str) {
        let Some(index) = self.steps.iter().position(|step| step.id == id) else {
            return;
        };
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

    /// Deleting a file does not start a step again. Reset is the only restart.

    fn hold_clone(&mut self, disk: &str) {
        if self.started && !self.stopped {
            return;
        }
        self.started = true;
        self.stopped = true;
        self.installed = true;
        self.checking = false;
        self.paused = pause_ready(Path::new(disk));
        self.pause_preview = self.paused && Path::new(disk).parent().unwrap().join("paused.png").is_file();
        self.headline = Some(if self.paused {
            "Debian is paused.".into()
        } else {
            "Debian is stopped. The guest disk is the installed system.".into()
        });
    }

    fn hold_finished(&mut self, disk: &str) {
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
        self.mark("partition", "done");
        self.mark("ssh", "done");
        self.mark("snapshot", "done");
        let _ = disk;
    }

    /// Install.onAppear. A saved snapshot does not start. Running the wizard again starts the installer.
    fn appear(&mut self, iso: &str, disk: &str) {
        if self.snapshot_ready && !self.from_snapshot {
            self.hold_finished(disk);
        } else if self.from_snapshot {
            self.hold_clone(disk);
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
        // A partition table or a leftover login file is not a finished guest. Attach the installer again.
        self.steps = setup_steps();
        self.mark("partition", "now");
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
        if self.started && !self.stopped {
            return;
        }
        self.started = true;
        self.installed = true;
        self.logged_in = guest_logged_in(Path::new(disk)) || self.from_snapshot;
        self.stopped = false;
        self.error = None;
        if !self.from_snapshot && snapshot_ready(Path::new(disk)) {
            self.hold_finished(disk);
            return;
        }
        let _ = pointer;
        if self.from_snapshot && (self.paused || pause_ready(Path::new(disk))) {
            self.checking = false;
            self.paused = false;
            self.pause_preview = false;
            self.headline = Some("Debian is running from the guest disk.".into());
            return;
        }
        self.checking = !self.from_snapshot;
        self.paused = false;
        self.pause_preview = false;
        self.headline = Some(if self.from_snapshot {
            "Starting Debian.".into()
        } else {
            "Debian is already installed. Checking SSH.".into()
        });
        self.mark("partition", "done");
        self.mark("ssh", if self.checking { "now" } else { "done" });
    }

    fn buttons(&self) -> Vec<&'static str> {
        let mut buttons = Vec::new();
        if self.stopped {
            if !self.from_snapshot && !self.snapshot_ready {
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

    fn stop(&mut self) {
        self.stopped = true;
    }

    /// Install.onDisappear. The library keeps clone sessions, so this is a no-op for fromSnapshot.
    fn disappear(&mut self) {
        if self.from_snapshot {
            return;
        }
        self.stop();
    }
}


fn open_setup(screen: &Screen, dir: &Path) -> Setup {
    assert!(screen.installing());
    let disk = screen.disk_path.as_deref().unwrap();
    // A partition table or a leftover login file does not skip Install guest. The snapshot does.
    let saved = snapshot_ready(Path::new(disk));
    let mut page = setup_page(false);
    page.installed = saved;
    page.logged_in = guest_logged_in(Path::new(disk)) || saved;
    page.snapshot_ready = saved;
    if saved {
        page.stopped = true;
        page.headline =
            Some("Debian is installed and shut down. A snapshot is saved for new machines.".into());
        page.mark("partition", "done");
        page.mark("ssh", "done");
        page.mark("snapshot", "done");
    } else {
        page.headline = Some("Starting the installer.".into());
    }
    page.appear(screen.ready_path.as_deref().unwrap(), disk);
    let _ = dir;
    page
}

fn open_clone(disk: &str) -> Setup {
    let mut page = setup_page(true);
    page.snapshot_ready = false;
    page.stopped = true;
    page.headline = Some("Debian is stopped. The guest disk is the installed system.".into());
    page.appear("", disk);
    page
}

/// Same as Welcome.swift LibraryModel. The detail view is one machine; sessions keep the others.
struct Library {
    machines: Vec<String>,
    selected: Option<String>,
    sessions: HashMap<String, Setup>,
}

impl Library {
    fn refresh(&mut self, disks: &[String]) {
        self.machines = disks.to_vec();
        self.sessions.retain(|disk, session| {
            if disks.iter().any(|live| live == disk) {
                true
            } else {
                session.stop();
                false
            }
        });
        if self
            .selected
            .as_ref()
            .map_or(true, |sel| !disks.iter().any(|disk| disk == sel))
        {
            self.selected = disks.first().cloned();
        }
    }

    fn session(&mut self, disk: &str) -> &mut Setup {
        if !self.sessions.contains_key(disk) {
            let mut page = setup_page(true);
            page.hold_clone(disk);
            self.sessions.insert(disk.to_string(), page);
        }
        self.sessions.get_mut(disk).unwrap()
    }

    fn clone_new(&mut self, disk: String) {
        let mut disks = self.machines.clone();
        disks.push(disk.clone());
        self.refresh(&disks);
        self.selected = Some(disk);
    }

    fn show(&mut self) {
        let Some(disk) = self.selected.clone() else {
            return;
        };
        self.session(&disk).appear("", &disk);
    }

    fn hide(&mut self) {
        let Some(disk) = self.selected.clone() else {
            return;
        };
        self.session(&disk).disappear();
    }
}

#[test]
fn adding_a_library_machine_does_not_stop_a_running_clone() {
    let dir = dir();
    let first = dir.join("machines/debian/disk.img");
    let second = dir.join("machines/debian-2/disk.img");
    std::fs::create_dir_all(first.parent().unwrap()).unwrap();
    std::fs::create_dir_all(second.parent().unwrap()).unwrap();
    std::fs::write(&first, b"disk").unwrap();
    std::fs::write(&second, b"disk").unwrap();
    let first = first.to_str().unwrap().to_string();
    let second = second.to_str().unwrap().to_string();

    let mut library = Library {
        machines: Vec::new(),
        selected: None,
        sessions: HashMap::new(),
    };
    library.refresh(&[first.clone()]);
    library.show();
    library.session(&first).boot_disk(&first, true);
    assert!(!library.session(&first).stopped);
    assert!(library.session(&first).captures_pointer());

    library.hide();
    library.clone_new(second.clone());
    library.show();
    assert_eq!(library.selected.as_deref(), Some(second.as_str()));
    assert!(
        !library.session(&first).stopped,
        "the running clone must stay up after a new machine is added"
    );
    assert!(library.session(&first).captures_pointer());
    assert!(library.session(&second).stopped);
    assert!(!library.session(&second).captures_pointer());
    assert_eq!(
        library.session(&second).headline.as_deref(),
        Some("Debian is stopped. The guest disk is the installed system.")
    );

    library.hide();
    library.selected = Some(first.clone());
    library.show();
    assert!(!library.session(&first).stopped);
    assert_eq!(
        library.session(&first).headline.as_deref(),
        Some("Starting Debian.")
    );

    library.refresh(&[second.clone()]);
    assert!(!library.sessions.contains_key(&first));
    assert_eq!(library.selected.as_deref(), Some(second.as_str()));
}

#[test]
fn a_clone_view_does_not_stop_on_disappear() {
    let dir = dir();
    let disk = dir.join("machines/lab/disk.img");
    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
    std::fs::write(&disk, b"disk").unwrap();
    let disk = disk.to_str().unwrap();
    let mut clone = open_clone(disk);
    clone.boot_disk(disk, true);
    clone.disappear();
    assert!(!clone.stopped);
    assert!(clone.captures_pointer());

    let mut setup = setup_page(false);
    setup.start(
        dir.join("cache/iso").to_str().unwrap(),
        dir.join("snapshots/debian/disk.img").to_str().unwrap(),
    );
    assert!(!setup.stopped);
    setup.disappear();
    assert!(setup.stopped);

    // The worker's ready line is what opens Debian setup. That page still stops when it disappears.
    let (base, _) = serve(SUMS, SIGN, ISO, KEY, false, false, false);
    let (status, events, work) = run(&base);
    assert_eq!(status, 0, "{events:?}");
    let screen = finish(&events, status);
    let mut opened = open_setup(&screen, &work);
    assert!(!opened.stopped);
    assert!(opened.shows_installer());
    opened.disappear();
    assert!(opened.stopped);
    let mut again = open_clone(disk);
    again.boot_disk(disk, true);
    again.disappear();
    assert!(!again.stopped, "a started clone stays running after the view disappears");
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
    assert_eq!(setup.thumbnail_row(), Some("partition"));
    assert!(!setup.captures_pointer());
    assert_eq!(setup.buttons(), [] as [&str; 0]);
    assert_eq!(setup.row("partition").state, "now");
    assert_eq!(setup.row("partition").title, "Install guest");
    // The guest disk is already there. Open stays hidden until this step ticks.
    assert!(dir.join("snapshots/debian/disk.img").is_file());
    assert_eq!(setup.row_buttons("partition", &dir), [] as [&str; 0]);
    assert_eq!(setup.row_buttons("snapshot", &dir), [] as [&str; 0]);
    assert_eq!(setup.row_buttons("ssh", &dir), [] as [&str; 0]);
    setup.show_window = true;
    assert!(!setup.shows_installer(), "the open window holds the only guest view");
    assert_eq!(setup.thumbnail_row(), None);
    assert_eq!(setup.buttons(), [] as [&str; 0]);

    let disk = dir.join("snapshots/debian/disk.img");
    let mut file = std::fs::OpenOptions::new().write(true).open(&disk).unwrap();
    use std::io::{Seek, SeekFrom, Write};
    file.seek(SeekFrom::Start(510)).unwrap();
    file.write_all(&[0x55, 0xaa]).unwrap();
    drop(file);
    let written = open_setup(&screen, &dir);
    assert_eq!(written.headline.as_deref(), Some("Starting the installer."));
    assert!(written.shows_installer());
    assert_eq!(written.thumbnail_row(), Some("partition"));
    assert!(!written.checking, "a partition table does not skip Install guest");
    assert!(!written.stopped);
    assert_eq!(written.buttons(), [] as [&str; 0]);
    assert!(
        !written.attached.is_empty(),
        "running the wizard again attaches the installer"
    );

    std::fs::write(dir.join("ssh/ssh-ok"), b"debian ok\n").unwrap();
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
        ["New virtual machine"]
    );
    assert_eq!(done.row("partition").state, "done");
    assert_eq!(done.row("snapshot").state, "done");
    let reopened = open_setup(&screen, &dir);
    assert!(reopened.snapshot_ready);
    assert!(!reopened.shows_installer());
    assert_eq!(reopened.thumbnail_row(), None);
    assert!(!reopened.checking);
    assert!(reopened.stopped);
    assert_eq!(
        reopened.buttons(),
        ["New virtual machine"]
    );
    assert_eq!(
        reopened.headline.as_deref(),
        Some("Debian is installed and shut down. A snapshot is saved for new machines.")
    );
    assert_eq!(reopened.row("partition").state, "done");
    assert_eq!(
        reopened.row_buttons("partition", &dir),
        [] as [&str; 0],
        "Install guest has no Open"
    );
    assert_eq!(reopened.row_buttons("snapshot", &dir), [] as [&str; 0]);
    assert!(dir.join("snapshots/debian/ssh-ok").is_file());
    assert_eq!(reopened.row_buttons("ssh", &dir), [] as [&str; 0]);
}

/// A leftover guest disk and snapshot are not a finished step. Open appears only after the tick.
#[test]
fn open_waits_for_the_tick_even_when_the_file_is_already_there() {
    let dir = dir();
    let disk = dir.join("snapshots/debian/disk.img");
    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
    std::fs::write(&disk, b"disk").unwrap();
    std::fs::create_dir_all(dir.join("snapshots/debian")).unwrap();
    std::fs::write(dir.join("snapshots/debian/ready"), b"ssh-ok\n").unwrap();
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::write(dir.join("cache").join(iso_name()), b"iso").unwrap();

    let mut screen = Screen::new();
    screen.start();
    assert_eq!(screen.row("partition").state, "wait");
    assert_eq!(screen.row("snapshot").state, "wait");
    assert_eq!(screen.row_buttons("partition", &dir), [] as [&str; 0]);
    assert_eq!(screen.row_buttons("snapshot", &dir), [] as [&str; 0]);
    assert_eq!(screen.row_buttons("download", &dir), [] as [&str; 0]);

    screen.apply(&serde_json::json!({
        "event": "step",
        "step": "download",
        "state": "done"
    }));
    assert_eq!(screen.row("download").state, "done");
    assert_eq!(screen.row("download").title, "Download ISO");
    assert_eq!(screen.row_buttons("download", &dir), [] as [&str; 0]);
    assert_eq!(screen.row_buttons("partition", &dir), [] as [&str; 0]);
    assert_eq!(screen.row_buttons("snapshot", &dir), [] as [&str; 0]);

    screen.apply(&serde_json::json!({
        "event": "step",
        "step": "partition",
        "state": "now",
        "detail": "Installing"
    }));
    assert_eq!(screen.row_buttons("partition", &dir), [] as [&str; 0]);
    screen.apply(&serde_json::json!({
        "event": "step",
        "step": "partition",
        "state": "done",
        "detail": "Installed"
    }));
    assert_eq!(screen.row("partition").state, "done");
    assert_eq!(screen.row("partition").title, "Install guest");
    assert_eq!(screen.row_buttons("partition", &dir), [] as [&str; 0]);

    screen.apply(&serde_json::json!({"event": "step", "step": "disk", "state": "done"}));
    assert_eq!(screen.row_buttons("disk", &dir), [] as [&str; 0]);
    assert_eq!(screen.row("snapshot").title, "Create snapshot");
    screen.apply(&serde_json::json!({"event": "step", "step": "snapshot", "state": "done"}));
    assert_eq!(screen.row("snapshot").state, "done");
    assert_eq!(screen.row("snapshot").title, "Create snapshot");
    assert_eq!(screen.row_buttons("snapshot", &dir), [] as [&str; 0]);
    std::fs::remove_dir_all(dir.join("snapshots/debian")).unwrap();
    assert_eq!(screen.row_buttons("snapshot", &dir), [] as [&str; 0]);
    assert!(
        !screen.shows_thumbnail("partition", &dir, true, false),
        "a saved snapshot hides the installer"
    );
    assert!(
        screen.shows_thumbnail("partition", &dir, false, false),
        "removing the snapshot shows the installer on Install guest"
    );
    assert_eq!(screen.top_slot(false, false), "empty");
    assert!(!screen.shows_thumbnail("partition", &dir, false, true));
    screen.show_log = false;
    screen.apply(&serde_json::json!({"event": "step", "step": "partition", "state": "wait"}));
    assert!(
        screen.shows_thumbnail("partition", &dir, false, false),
        "a waiting install row still shows the installer"
    );
    assert_eq!(screen.top_slot(true, false), "empty");

    let mut setup = setup_page(false);
    setup.mark("partition", "now");
    assert_eq!(setup.row_buttons("partition", &dir), [] as [&str; 0]);
    assert_eq!(setup.row_buttons("snapshot", &dir), [] as [&str; 0]);
    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
    std::fs::write(&disk, b"disk").unwrap();
    setup.mark("partition", "done");
    assert_eq!(
        setup.row_buttons("partition", &dir),
        [] as [&str; 0],
        "Install guest has no Open"
    );
    assert_eq!(setup.row_buttons("snapshot", &dir), [] as [&str; 0]);
    setup.mark("snapshot", "done");
    std::fs::create_dir_all(dir.join("snapshots/debian")).unwrap();
    assert_eq!(setup.row_buttons("snapshot", &dir), [] as [&str; 0]);
    assert_eq!(setup.row_buttons("ssh", &dir), [] as [&str; 0]);
}

/// There is no Open button and no Open folder button.
#[test]
fn there_is_no_open_button() {
    let src = include_str!("../Sources/QuickStart.swift");
    assert!(!src.contains("Button(\"Open"));
    assert!(!src.contains("func openFolder"));
    assert!(!src.contains("func launchOpen"));
    assert!(!include_str!("../Sources/Install.swift").contains("Button(\"Open"));
}

/// Same choice as Install.onAppear after Quick start emits ready.
/// A partition table, a leftover login file, or a hash of the disk is not a finished guest.
/// Running the wizard again starts the installer. snapshots/debian/ready is the only skip.
fn reopen_boot(disk: &str) -> Reopen {
    let url = std::path::Path::new(disk);
    let snapshot_saved = snapshot_ready(url);
    let starts_vm = !snapshot_saved;
    Reopen {
        installed: snapshot_saved,
        attaches_installer: starts_vm,
        captures_pointer: false,
        checks_ssh: false,
        starts_vm,
        snapshot_saved,
        headline: Some(if snapshot_saved {
            "Debian is installed and shut down. A snapshot is saved for new machines.".into()
        } else {
            "Starting the installer.".into()
        }),
    }
}

fn support_of(disk: &std::path::Path) -> std::path::PathBuf {
    disk.parent()
        .and_then(|dir| dir.parent())
        .and_then(|dir| dir.parent())
        .unwrap()
        .to_path_buf()
}

fn pause_ready(disk: &std::path::Path) -> bool {
    let dir = disk.parent().unwrap();
    dir.join("paused").is_file() && dir.join("paused.vzvmsave").is_file()
}

fn snapshot_ready(disk: &std::path::Path) -> bool {
    let snap = support_of(disk).join("snapshots/debian");
    snap.join("disk.img").is_file()
        && std::fs::read_to_string(snap.join("ready"))
            .ok()
            .is_some_and(|text| text.starts_with("ssh-ok"))
}

fn guest_logged_in(disk: &std::path::Path) -> bool {
    let beside = disk.parent().unwrap().join("ssh-ok");
    let shared = support_of(disk).join("ssh/ssh-ok");
    [beside, shared].into_iter().any(|path| {
        std::fs::read_to_string(path)
            .ok()
            .is_some_and(|text| text.starts_with("debian") && text.contains("ok"))
    })
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
fn deleting_a_setup_file_does_not_rerun_that_step() {
    let dir = dir();
    let disk = dir.join("snapshots/debian/disk.img");
    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
    std::fs::write(&disk, b"disk").unwrap();
    std::fs::create_dir_all(dir.join("ssh")).unwrap();
    std::fs::create_dir_all(dir.join("installer")).unwrap();
    std::fs::write(dir.join("installer/preseed.img"), b"preseed").unwrap();
    std::fs::write(dir.join("installer/boot.iso"), b"boot").unwrap();
    std::fs::create_dir_all(dir.join("snapshots/debian")).unwrap();
    std::fs::write(dir.join("ssh/ssh-ok"), b"debian ok\n").unwrap();
    std::fs::write(disk.parent().unwrap().join("ssh-ok"), b"debian ok\n").unwrap();
    std::fs::write(dir.join("snapshots/debian/ready"), b"ssh-ok\n").unwrap();
    std::fs::write(&disk, b"disk").unwrap();

    let mut setup = setup_page(false);
    setup.snapshot_ready = true;
    setup.logged_in = true;
    setup.installed = true;
    setup.stopped = true;
    setup.mark("ssh", "done");
    setup.mark("snapshot", "done");
    assert_eq!(setup.row("partition").state, "done");
    assert!(!setup.shows_installer());
    let before = setup.steps.iter().map(|step| step.state.clone()).collect::<Vec<_>>();
    std::fs::remove_file(dir.join("snapshots/debian/disk.img")).unwrap();
    std::fs::remove_file(dir.join("ssh/ssh-ok")).unwrap();
    std::fs::remove_file(disk.parent().unwrap().join("ssh-ok")).unwrap();
    std::fs::remove_file(dir.join("snapshots/debian/ready")).unwrap();
    std::fs::remove_file(dir.join("installer/preseed.img")).unwrap();
    std::fs::remove_file(dir.join("installer/boot.iso")).unwrap();
    assert_eq!(
        setup.steps.iter().map(|step| step.state.clone()).collect::<Vec<_>>(),
        before,
        "deleting a setup file does not rerun that step"
    );
    assert!(setup.snapshot_ready);
    assert!(!setup.shows_installer());
}

/// Deleting snapshots/debian does not start the installer again. Reset does that.
#[test]
fn removing_the_snapshot_does_not_start_again() {
    let dir = dir();
    let disk = dir.join("snapshots/debian/disk.img");
    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
    std::fs::write(&disk, b"disk").unwrap();
    std::fs::write(dir.join("snapshots/debian/ready"), b"ssh-ok\n").unwrap();
    std::fs::create_dir_all(dir.join("ssh")).unwrap();
    std::fs::write(dir.join("ssh/ssh-ok"), b"debian ok\n").unwrap();
    std::fs::write(disk.parent().unwrap().join("ssh-ok"), b"debian ok\n").unwrap();
    std::fs::create_dir_all(dir.join("installer")).unwrap();
    std::fs::write(dir.join("installer/boot.iso"), b"boot").unwrap();
    std::fs::write(dir.join("installer/preseed.img"), b"preseed").unwrap();

    let mut setup = setup_page(false);
    setup.snapshot_ready = true;
    setup.logged_in = true;
    setup.installed = true;
    setup.stopped = true;
    setup.mark("partition", "done");
    setup.mark("ssh", "done");
    setup.mark("snapshot", "done");
    assert!(!setup.shows_installer());
    assert_eq!(setup.thumbnail_row(), None);
    let before = setup.steps.iter().map(|step| step.state.clone()).collect::<Vec<_>>();
    std::fs::remove_dir_all(dir.join("snapshots/debian")).unwrap();
    assert_eq!(
        setup.steps.iter().map(|step| step.state.clone()).collect::<Vec<_>>(),
        before,
        "deleting the snapshot does not rerun Create snapshot"
    );
    assert!(setup.snapshot_ready);
    assert_eq!(setup.row("partition").state, "done");
    assert!(!setup.shows_installer());

    let mut screen = Screen::new();
    screen.show_log = false;
    screen.apply(&serde_json::json!({"event": "step", "step": "partition", "state": "done", "detail": "Installed"}));
    screen.apply(&serde_json::json!({"event": "step", "step": "ssh", "state": "done", "detail": "Logged in"}));
    screen.apply(&serde_json::json!({"event": "step", "step": "snapshot", "state": "done"}));
    assert!(
        !screen.shows_thumbnail("partition", &dir, true, false),
        "the snapshot is still saved"
    );
    assert!(
        screen.shows_thumbnail("partition", &dir, false, false),
        "the installer is on Install guest, not above the list"
    );
    assert_eq!(screen.top_slot(false, false), "empty");
    screen.show_log = true;
    assert!(screen.shows_thumbnail("partition", &dir, false, false));
    assert!(!screen.shows_thumbnail("partition", &dir, false, true));
    assert_eq!(screen.top_slot(false, true), "empty");
    screen.downloading = true;
    assert_eq!(screen.top_slot(false, false), "empty", "the download bar is on Download ISO, not above the list");
    screen.downloading = false;
    assert_eq!(screen.top_slot(true, false), "empty", "nothing sits above the steps");
}

#[test]
fn quick_start_swift_gates_the_bar_on_a_real_download() {
    let src = include_str!("../Sources/QuickStart.swift");
    assert!(src.contains("if event.event == \"progress\", event.step == \"download\""));
    assert!(src.contains("if wizard.downloading {"));
    assert!(src.contains("steps = WizardStep.initial + WizardStep.preseed + WizardStep.setup"));
    assert!(!src.contains("showPreseed"));
    assert!(!src.contains("Show all"));
    assert!(!src.contains("Hide all"));
    assert!(!src.contains("func rowLabel"));
    assert!(!src.contains("WrappingPath(text: rowLabel"));
    assert!(src.contains("static func status"));
    assert!(src.contains("Text(WizardStep.status(step.id, state: step.state))"));
    assert!(include_str!("../Sources/Install.swift").contains("Text(WizardStep.status(step.id, state: step.state))"));
    assert!(!include_str!("../Sources/Install.swift").contains("WrappingPath(text: rowLabel"));
    assert!(src.contains("frame(minWidth: WindowMetrics.quickStartWidth, idealWidth: WindowMetrics.quickStartWidth, maxWidth: .infinity, minHeight: WindowMetrics.quickStartMinHeight, idealHeight: WindowMetrics.quickStart.height, maxHeight: .infinity, alignment: .topLeading)"));
    assert!(!src.contains("minHeight: 420"));
    assert!(include_str!("../Sources/App.swift").contains("Window(\"Quick start\", id: \"quick-start\")"));
    assert!(include_str!("../Sources/App.swift").contains("final class QuickStartModel"));
    assert!(include_str!("../Sources/App.swift").contains("QuickStart(wizard: quickStart.wizard, installSlot: quickStart.installSlot)"));
    assert!(include_str!("../Sources/Install.swift").contains("func pauseHostKey"));
    assert!(include_str!("../Sources/Install.swift").contains("func resumeHostKey"));
    assert!(include_str!("../Sources/Install.swift").contains("Checking SSH again."));
    assert!(include_str!("../Sources/App.swift").contains(".windowResizability(.automatic)"));
    assert!(include_str!("../Sources/App.swift").contains(".defaultSize(width: WindowMetrics.quickStart.width, height: WindowMetrics.quickStart.height)"));
    assert!(!src.contains(".frame(width: 440, alignment: .topLeading)"));
    assert!(!src.contains(".frame(width: 440, alignment: .topLeading)\n        .fixedSize(horizontal: false, vertical: true)"));
    assert!(src.contains(".fixedSize(horizontal: false, vertical: true)"));
    assert!(src.contains("struct FitWindowHeight"));
    assert!(src.contains("frame.height > visible.height"));
    assert!(src.contains("ScrollView {"));
    assert!(src.contains("private var showsInstaller: Bool"));
    assert!(src.contains("if step.id == \"download\""));
    assert!(src.contains("if step.id == \"partition\", showsInstaller"));
    assert!(src.contains("installerView"));
    assert!(src.contains("InstallGuest(session: installSlot.session!)"));
    assert!(!src.contains("Button(\"Show installer\")"));
    assert!(!src.contains("Button(\"Open"));
    assert!(src.contains(".frame(width: Guest.thumbnail.width, height: Guest.thumbnail.height)"));
    assert!(src.contains("installSlot.session?.showWindow = true"));
    assert!(src.contains("Button {"));
    assert!(!src.contains(".onTapGesture { installSlot.session?.showWindow = true }"));
    assert!(!src.contains("onTapGesture(count: 2)"));
    assert!(!include_str!("../Sources/Install.swift").contains("Button(\"Show installer\")"));
    assert!(!include_str!("../Sources/Install.swift").contains("onTapGesture(count: 2)"));
    assert!(include_str!("../Sources/Install.swift").contains("var needsInstallGuest"));
    assert!(!src.contains("enum StepPath"));
    assert!(!src.contains("func rowLabel"));
    assert!(src.contains("if step.id == \"download\", step.state == \"now\""));
    assert!(include_str!("../src/preseed.rs").contains("pub const SNAPSHOT: &str = \"snapshots/debian\""));
    assert!(src.contains("struct FitWindowHeight"));
    assert!(src.contains("A closed wizard proposes nothing"));
    assert!(include_str!("../Sources/Install.swift").contains("func sizeThatFits(_ proposal: ProposedViewSize, nsView: InstallScreen, context: Context) -> CGSize?"));
    assert!(include_str!("../Sources/Install.swift").contains("?? Guest.cloneGuest.width"));
    assert!(include_str!("../Sources/Install.swift").contains("override var intrinsicContentSize"));
    assert!(include_str!("../Sources/Install.swift").contains("Closing the wizard mid-install"));
    assert!(!src.contains("WrappingPath(text: rowLabel(step))"));
    assert!(!include_str!("../src/main.rs").contains("fn step_file"));
    assert!(!include_str!("../src/main.rs").contains("fn step_rel"));

    assert!(src.contains("Install(iso: iso, disk: disk, embedded: true, freshInstall: wizard.runID > 0, steps: $wizard.steps, slot: installSlot)"));
    assert!(src.contains("@ObservedObject var wizard: Wizard"));
    assert!(!src.contains("@StateObject private var wizard = Wizard()"));
    assert!(!src.contains("Button(\"Open"));
    assert!(src.contains("if step.id == \"partition\", showsInstaller"));
    assert!(!include_str!("../Sources/Install.swift").contains("detail: \"snapshots/debian/ssh-ok\""));
    assert!(!include_str!("../Sources/Install.swift").contains("detail: \"debian\""));
    assert!(src.contains("appendingPathComponent(\"snapshots/debian\", isDirectory: true)"));
    assert!(include_str!("../Sources/Install.swift").contains("appendingPathComponent(\"preseed.img\")"));
    assert!(include_str!("../Sources/Install.swift").contains("appendingPathComponent(\"boot.iso\")"));
    assert!(!src.contains("preseedSteps"));
    assert!(!include_str!("../Sources/Install.swift").contains("preseedSteps"));
    assert!(include_str!("../Sources/Install.swift").contains("let shown = WizardStep.visible(guest.setupSteps)"));
    assert!(src.contains("static let shownIDs = [\"download\", \"partition\", \"snapshot\"]"));
    assert!(src.contains("Button(\"Reset\")"));
    assert!(src.contains("func resetWizard"));
    assert!(src.contains("--rebuild"));
    assert!(src.contains("freshInstall: wizard.runID > 0"));
    assert!(include_str!("../Sources/Install.swift").contains("freshInstall: Bool = false"));
    assert!(!src.contains("--from"));
    assert!(!src.contains("firstMissing"));
    assert!(!include_str!("../Sources/Install.swift").contains("func noteMissingFiles"));
    assert!(!include_str!("../Sources/Install.swift").contains("missingFile"));
    assert!(include_str!("../Sources/Install.swift")
        .contains("appendingPathComponent(\"installer\", isDirectory: true)"));
    assert!(
        include_str!("../Sources/Install.swift").contains("SupportDir.snapshotReady")
    );
    assert!(include_str!("../Sources/Install.swift")
        .contains("GuestSession(fromSnapshot: fromSnapshot, diskWritten: false, snapshotSaved: snapshotSaved, loggedIn: loggedIn, paused: paused)"));
    assert!(!include_str!("../Sources/Install.swift").contains("GuestSession(fromSnapshot: loggedIn"));
    assert!(!src.contains("Button(\"Open"));
    assert!(!include_str!("../Sources/Install.swift").contains("SupportDir.open()"));
    assert!(!src.contains("Button(\"Back\""));
    assert!(!src.contains("Button(\"Next\""));
    assert!(src.contains("static let preseed"));
    assert!(include_str!("../Sources/Install.swift").contains("Debian setup"));
    assert!(src.contains("private var showsInstaller: Bool"));
    assert!(include_str!("../Sources/Install.swift").contains("if !fromSnapshot && !guest.snapshotReady {"));
    assert!(include_str!("../Sources/Install.swift").contains("GuestMirror"));
    assert!(
        include_str!("../Sources/Install.swift").contains("func attach(_ screen: InstallScreen)")
    );
    assert!(include_str!("../Sources/Install.swift").contains(".nonactivatingPanel"));
    assert!(include_str!("../Sources/Install.swift").contains("becomesKeyOnlyIfNeeded = true"));
    assert!(include_str!("../Sources/Install.swift").contains("struct InstallerWindow"));
    assert!(!include_str!("../Sources/Install.swift").contains(".popover(isPresented:"));
    assert!(include_str!("../Sources/Install.swift").contains("frame(width: setup ? WindowMetrics.quickStartWidth : Guest.cloneGuest.width + WindowMetrics.pagePad * 2"));
    assert!(include_str!("../Sources/Install.swift").contains("let guestSize = Guest.scanoutSize"));
    assert!(!include_str!("../Sources/Install.swift").contains("titlebarAppearsTransparent = true"));
    assert!(!include_str!("../Sources/Install.swift").contains("titleVisibility = .hidden"));
    assert!(include_str!("../Sources/Install.swift").contains("panel.title = \"Installer\""));
    assert!(!include_str!("../Sources/Install.swift").contains(".padding(16)"));
    assert!(!include_str!("../Sources/Install.swift").contains("guestSize.width + 32"));
    assert!(include_str!("../Sources/Install.swift").contains(".resizable"));
    assert!(include_str!("../Sources/Install.swift").contains("panel.contentMinSize"));
    assert!(include_str!("../Sources/Install.swift").contains("minWidth: Guest.installerMin.width"));
    assert!(!include_str!("../Sources/Install.swift").contains("frame(width: Guest.scanoutSize.width, height: Guest.scanoutSize.height)"));
    assert!(!include_str!("../Sources/QuickStart.swift").contains("frame(width: Guest.scanoutSize.width, height: Guest.scanoutSize.height)"));
    assert!(!include_str!("../Sources/Install.swift").contains("min(1280, max(640, screen.width - 32))"));
    assert!(include_str!("../Sources/Guest.swift").contains("static let scanout = (width: 1280, height: 800)"));
    assert!(include_str!("../Sources/Guest.swift").contains("static let scanoutSize = CGSize(width: CGFloat(scanout.width), height: CGFloat(scanout.height))"));
    assert!(include_str!("../Sources/Guest.swift").contains("static let thumbnail = CGSize(width: CGFloat(scanout.width) / 8, height: CGFloat(scanout.height) / 8)"));
    assert!(include_str!("../Sources/Guest.swift").contains("static let installerMin = CGSize(width: CGFloat(scanout.width) / 2, height: CGFloat(scanout.height) / 2)"));
    assert!(!include_str!("../Sources/Guest.swift").contains("static let scanoutSize = CGSize(width: 1280, height: 800)"));
    assert!(!include_str!("../Sources/Install.swift").contains("frame(height: 96)"));
    assert!(include_str!("../Sources/Install.swift")
        .contains("private var capturePointer: Bool { fromSnapshot && !guest.stopped }"));
    assert!(include_str!("../Sources/Install.swift").contains("private var captureKeyboard: Bool { fromSnapshot && !guest.stopped }"));
    assert!(include_str!("../Sources/Install.swift").contains("keyboard: captureKeyboard"));
    assert!(include_str!("../Sources/Install.swift").contains("pointer: false"));
    assert!(include_str!("../Sources/Guest.swift").contains("pointer: Bool = false"));
    assert!(!include_str!("../debian.cfg").contains("keyboard-configuration"));
    assert!(include_str!("../debian.cfg").contains("tasksel tasksel/first multiselect xfce-desktop"));
    assert!(!include_str!("../debian.cfg").contains("partman-auto/method"));
    assert!(!include_str!("../debian.cfg").contains("partman-auto/method string regular"));
    assert!(!include_str!("../debian.cfg").contains("tmp.mount"));
    assert!(!include_str!("../debian.cfg").contains("size=8G"));
    assert!(include_str!("../debian.cfg").contains("d-i pkgsel/include string openssh-server spice-vdagent"));
    assert!(!include_str!("../debian.cfg").contains("apt-install"));
    assert!(!include_str!("../debian.cfg").contains("expert_recipe"));
    assert!(!include_str!("../debian.cfg").contains("preseed/early_command"));
    assert!(include_str!("../Sources/Guest.swift").contains("config.keyboards = extra.isEmpty && keyboard"));
    assert!(!include_str!("../Sources/Install.swift").contains("InstalledDisk.looksInstalled"));
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
    assert!(include_str!("../src/ssh.rs").contains("192.168.64."));
    assert!(include_str!("../src/ssh.rs").contains("% 253"));
    assert!(include_str!("../src/ssh.rs").contains("ConnectTimeout=1"));
    assert!(include_str!("../Sources/QuickStart.swift").contains("terminationHandler"));
    assert!(include_str!("../Sources/QuickStart.swift").contains("[\"ssh\", \"--disk\", disk.path]"));
    assert!(include_str!("../Sources/Install.swift").contains("watchSSH()"));
    assert!(include_str!("../Sources/Install.swift").contains("A saved snapshot is the artifact"));
    assert!(!include_str!("../Sources/Install.swift").contains("login file was removed"));
    assert!(!include_str!("../Sources/Install.swift").contains("machine?.state == .running"));
    assert!(include_str!("../Sources/Guest.swift").contains("func forget(beside disk: URL)"));
    assert!(include_str!("../Sources/Install.swift")
        .contains("SSH login succeeded. Saving a snapshot."));
    assert!(include_str!("../Sources/Install.swift").contains("New virtual machine"));
    assert!(include_str!("../Sources/Guest.swift").contains("GuestSSH"));
    assert!(
        include_str!("../Sources/Guest.swift").contains("snapshots/debian/ssh-ok")
            || include_str!("../Sources/Guest.swift").contains("sshPort: UInt16 = 22")
    );
    assert!(
        include_str!("../Sources/QuickStart.swift").contains("[\"snapshot\", \"--dir\", dir.path]")
    );
    assert!(!include_str!("../Sources/Welcome.swift").contains("New Debian"));
    assert!(include_str!("../Sources/Welcome.swift").contains("Quick start"));
    assert!(include_str!("../Sources/Welcome.swift").contains("Text(\"Welcome\")"));
    assert!(include_str!("../Sources/Welcome.swift").contains("Download Debian 13 for this Mac."));
    assert!(include_str!("../Sources/Welcome.swift").contains("desktopcomputer"));
    assert!(include_str!("../Sources/Welcome.swift").contains("library.snapshotReady"));
    assert!(include_str!("../Sources/Welcome.swift").contains("NavigationSplitView"));
    assert!(include_str!("../Sources/Welcome.swift").contains("Worker.cloneMachine"));
    assert!(include_str!("../Sources/Welcome.swift").contains("fromSnapshot: true"));
    assert!(include_str!("../Sources/Welcome.swift").contains("GuestPause.isPaused"));
    assert!(include_str!("../Sources/Guest.swift").contains("func preview(beside"));
    assert!(!include_str!("../Sources/Welcome.swift").contains("Image(nsImage: image)"));
    assert!(!include_str!("../Sources/Welcome.swift").contains("Text(\"Paused\")"));
    assert!(include_str!("../Sources/Welcome.swift").contains("libraryDidChange"));
    assert!(!include_str!("../Sources/App.swift").contains("id: \"debian\""));
    assert!(include_str!("../Sources/Welcome.swift").contains("func offerFirstName()"));
    assert!(include_str!("../Sources/Welcome.swift").contains("func cloneNew()"));
    assert!(include_str!("../Sources/Welcome.swift").contains("Name this virtual machine"));
    assert!(include_str!("../Sources/Welcome.swift").contains("draftName = \"My Debian\""));
    assert!(include_str!("../Sources/Welcome.swift").contains("Worker.machineName"));
    assert!(include_str!("../Sources/Welcome.swift").contains("func checkDraft()"));
    assert!(include_str!("../Sources/QuickStart.swift").contains("[\"name\", \"--dir\", dir.path, \"--name\", name]"));
    assert!(include_str!("../Sources/Install.swift").contains("showLibrary(select: \"name\")"));
    assert!(include_str!("../Sources/QuickStart.swift").contains("[\"--name\", name]"));
    assert!(include_str!("../Sources/Welcome.swift").contains("New virtual machine"));
    assert!(include_str!("../Sources/Welcome.swift").contains("func session(for"));
    assert!(include_str!("../Sources/Welcome.swift").contains("session: library.session(for: disk)"));
    assert!(include_str!("../Sources/Install.swift").contains("if fromSnapshot { return }"));
    assert!(!include_str!("../Sources/Install.swift").contains("keepRunning"));
    assert!(include_str!("../Sources/Install.swift").contains("openWindow(id: \"main\")"));
    assert!(include_str!("../Sources/Install.swift").contains("var fill = false"));
    assert!(include_str!("../Sources/Install.swift").contains("private var filled"));
    assert!(include_str!("../Sources/Install.swift").contains("guest.pausePreview"));
    assert!(include_str!("../Sources/Install.swift").contains("GuestView stays mounted under that frame"));
    assert!(include_str!("../Sources/Install.swift").contains("func snapshot()"));
    assert!(include_str!("../Sources/Install.swift").contains("cacheDisplay"));
    assert!(!include_str!("../Sources/Install.swift").contains("Text(\"Paused\")"));
    assert!(include_str!("../Sources/Install.swift").contains("Button(\"Stop\") { guest.halt() }"));
    assert!(include_str!("../Sources/Install.swift").contains("Button(\"Pause\") { guest.pauseGuest() }"));
    assert!(include_str!("../Sources/Install.swift").contains("func publishLibrary"));
    assert!(include_str!("../Sources/Install.swift").contains("self.publishLibrary()"));
    assert!(include_str!("../Sources/Install.swift").contains("func halt()"));
    assert!(include_str!("../Sources/Install.swift").contains("func pauseGuest()"));
    assert!(include_str!("../Sources/Install.swift").contains("func holdClone"));
    assert!(include_str!("../Sources/Install.swift").contains("restoreMachineStateFrom"));
    assert!(include_str!("../Sources/Guest.swift").contains("enum GuestPause"));
    assert!(include_str!("../Sources/Install.swift").contains("view.snapshot()"));
    assert!(include_str!("../Sources/Install.swift").contains("pausePreview = frame"));
    assert!(include_str!("../Sources/Guest.swift").contains("func preview(beside"));
    assert!(include_str!("../src/pause.rs").contains("paused.png"));
    assert!(include_str!("../src/pause.rs").contains("preview(disk)"));
    assert!(include_str!("../Sources/Guest.swift").contains("VZGenericMachineIdentifier"));
    assert!(include_str!("../Sources/Guest.swift").contains("machine-id"));
    assert!(include_str!("../Sources/QuickStart.swift").contains("[\"pause\", \"--disk\", disk.path]"));
    assert!(include_str!("../Sources/QuickStart.swift").contains("[\"unpause\", \"--disk\", disk.path]"));
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
