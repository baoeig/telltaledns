//! REQ: OPS-007, OPS-009 — a signal that arrives while the node is still starting (DNS already
//! answers; the API, the state database and the rest are still being set up) is acted on once
//! the node is up: SIGTERM drains and exits cleanly, SIGHUP reloads. Before, the signal's
//! default action ended the process (review round 3, F2). The start-up is made a little longer
//! with a bootstrap administrator (a password hash), and each signal is sent at DNS's first
//! answer.

#![allow(clippy::expect_used)] // test helpers

use std::net::UdpSocket;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind");
    s.local_addr().expect("addr").port()
}

/// An A query for nas.signals.test; true when an answer comes back.
fn answers(port: u16) -> bool {
    let Ok(s) = UdpSocket::bind("127.0.0.1:0") else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(300)));
    let mut q = vec![0x43, 0x21, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["nas", "signals", "test"] {
        q.push(u8::try_from(label.len()).unwrap_or(0));
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    if s.send_to(&q, ("127.0.0.1", port)).is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    matches!(s.recv(&mut buf), Ok(n) if n > 12 && buf[0..2] == [0x43, 0x21] && buf[7] == 1)
}

/// Starts a node in `dir` and returns it, and its DNS port, once DNS first answers (asked every
/// 2 ms, so the signal that follows lands in the start-up).
fn start(dir: &Path) -> (Child, u16) {
    let (dns, api, metrics) = (free_port(), free_port(), free_port());
    let cfg = dir.join("t.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"[node]
data_dir = "{}"
drain_delay_secs = 2

[[listen]]
proto = "udp"
addr = "127.0.0.1:{dns}"

[[upstream]]
name = "nowhere"
url = "udp://127.0.0.1:9"

[[upstream_group]]
name = "default"
members = ["nowhere"]

[[record]]
name = "nas.signals.test"
type = "A"
value = "192.168.1.10"

[telemetry.metrics]
listen = "127.0.0.1:{metrics}"

[api]
listen = "127.0.0.1:{api}"
"#,
            dir.join("data").display()
        ),
    )
    .expect("write config");
    let log = dir.join("node.log");
    let child = Command::new(env!("CARGO_BIN_EXE_telltale"))
        .args(["run", "-c"])
        .arg(&cfg)
        .env("TELLTALE_BOOTSTRAP_ADMIN_USER", "admin")
        .env(
            "TELLTALE_BOOTSTRAP_ADMIN_PASSWORD",
            "a start-up signal test password",
        )
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).expect("log file"))
        .spawn()
        .expect("start");
    let started = Instant::now();
    while !answers(dns) {
        if started.elapsed() > Duration::from_secs(60) {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let tail: Vec<&str> = text.lines().rev().take(15).collect();
            panic!(
                "never answered; the node's log, last lines first:\n{}",
                tail.join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    (child, dns)
}

fn signal(child: &Child, sig: &str) {
    let status = Command::new("kill")
        .args([sig, &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(status.success());
}

#[test]
fn ops_007_a_stop_during_start_up_drains_and_exits_cleanly() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (mut child, dns) = start(tmp.path());
    signal(&child, "-TERM");
    let sent = Instant::now();
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        answers(dns),
        "a stop during start-up ended the node instead of draining it"
    );
    let code = child.wait().expect("wait");
    assert!(code.success(), "clean exit: {code:?}");
    assert!(
        sent.elapsed() >= Duration::from_secs(2),
        "exited before the drain delay: {:?}",
        sent.elapsed()
    );
}

#[test]
fn ops_009_a_reload_during_start_up_reloads() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (mut child, dns) = start(tmp.path());
    signal(&child, "-HUP");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "a reload during start-up ended the node"
    );
    assert!(answers(dns), "answers after the reload");
    signal(&child, "-TERM");
    let code = child.wait().expect("wait");
    assert!(code.success(), "clean exit: {code:?}");
}
