// REQ: OPS-014 — updates driven from the cluster: what a node agrees to, the request it writes,
// and what the root update service does with it.

use std::cell::Cell;

use super::*;
use crate::selfupdate::Outcome;

const NOW: u64 = 1_795_000_000_000;

fn here(dir: &Path, install: &str, version: &str, enabled: bool) -> Here {
    Here {
        node: "dns-02".into(),
        version: version.into(),
        install: install.into(),
        enabled,
        data_dir: dir.to_path_buf(),
        result: dir.join("service").join("result.json"),
    }
}

fn code(r: Result<NodeUpdate, Problem>) -> Code {
    r.unwrap_err().code
}

#[test]
fn ops_014_a_node_takes_only_newer_versions_it_can_install() {
    let d = tempfile::tempdir().unwrap();
    let native = here(d.path(), "native", "0.7.0", true);
    assert_eq!(
        code(request(
            &here(d.path(), "helm", "0.7.0", true),
            "0.8.0",
            "a",
            NOW
        )),
        Code::UpdateUnsupported
    );
    assert_eq!(
        code(request(
            &here(d.path(), "container", "0.7.0", true),
            "0.8.0",
            "a",
            NOW
        )),
        Code::UpdateUnsupported
    );
    assert_eq!(
        code(request(
            &here(d.path(), "native", "dev", true),
            "0.8.0",
            "a",
            NOW
        )),
        Code::UpdateUnsupported
    );
    assert_eq!(
        code(request(&native, "0.6.9", "a", NOW)),
        Code::UpdateDowngrade
    );
    assert_eq!(
        code(request(&native, "0.7.0-edge.400", "a", NOW)),
        Code::UpdateDowngrade
    );
    assert_eq!(
        code(request(&native, "latest", "a", NOW)),
        Code::UpdateInvalid
    );
    assert_eq!(
        code(request(
            &here(d.path(), "native", "0.7.0", false),
            "0.8.0",
            "a",
            NOW
        )),
        Code::UpdateNotEnabled
    );
    // Nothing was written for any of them.
    assert!(!d.path().join(REQUEST_FILE).exists());
    // The same version is up to date without a request.
    let same = request(&native, "0.7.0", "a", NOW).unwrap();
    assert_eq!(same.state.as_deref(), Some("up_to_date"));
    assert!(!d.path().join(REQUEST_FILE).exists());
}

#[test]
fn ops_014_a_request_is_written_for_the_update_service_and_shown_as_pending() {
    let dir = tempfile::tempdir().unwrap();
    // An edge node (a stable one never takes an edge build: ops_014_a_request_never_moves_...).
    let node = here(dir.path(), "native", "0.7.0-edge.400", true);
    let res = request(&node, "0.8.0-edge.393", "alice via k3s", NOW).unwrap();
    assert_eq!(res.state.as_deref(), Some("requested"));
    let asked = res.pending.unwrap();
    assert_eq!(
        (asked.to.as_str(), asked.by.as_str()),
        ("0.8.0-edge.393", "alice via k3s")
    );
    let file: RequestFile =
        serde_json::from_slice(&std::fs::read(dir.path().join(REQUEST_FILE)).unwrap()).unwrap();
    assert_eq!(file.id, asked.id);
    assert_eq!(file.requested_ms, NOW);
    // No temporary file is left behind.
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![REQUEST_FILE.to_owned()]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join(REQUEST_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
    }
    // A read shows it, and no run yet.
    let read = status(&node);
    assert_eq!(read.pending.unwrap().id, asked.id);
    assert!(read.last.is_none() && read.state.is_none());
}

#[test]
fn ops_014_the_update_service_installs_a_newer_version_once_and_records_it() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.7.0", true);
    let asked = request(&h, "0.8.0", "alice", NOW).unwrap().pending.unwrap();
    let calls = Cell::new(0);
    let served = serve_request(&h.request_path(), &h.result, "0.7.0", NOW + 1000, |to| {
        calls.set(calls.get() + 1);
        assert_eq!(to, "0.8.0");
        // While it installs, the node sees `installing`.
        assert_eq!(status(&h).last.unwrap().state, "installing");
        Ok(Outcome::Updated {
            previous: PathBuf::from("/usr/local/bin/telltale.old"),
        })
    });
    assert_eq!(
        served,
        Served::Done {
            state: "updated".into(),
            restart: true
        }
    );
    assert_eq!(calls.get(), 1);
    let s = status(&h);
    assert!(s.pending.is_none(), "the request is consumed");
    let last = s.last.unwrap();
    assert_eq!(
        (
            last.id.as_str(),
            last.state.as_str(),
            last.from.as_str(),
            last.to.as_str()
        ),
        (asked.id.as_str(), "updated", "0.7.0", "0.8.0")
    );
    // Run again: nothing to do.
    let again = serve_request(
        &h.request_path(),
        &h.result,
        "0.8.0",
        NOW,
        |_| unreachable!(),
    );
    assert_eq!(again, Served::Nothing);
}

#[test]
fn ops_014_a_failed_install_is_recorded_and_does_not_restart() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.7.0-edge.400", true);
    request(&h, "0.8.0-edge.393", "alice", NOW).unwrap();
    let served = serve_request(&h.request_path(), &h.result, "0.7.0-edge.400", NOW, |_| {
        Err("the edge channel now serves 0.8.0-edge.394".into())
    });
    assert_eq!(
        served,
        Served::Done {
            state: "failed".into(),
            restart: false
        }
    );
    let last = status(&h).last.unwrap();
    assert_eq!(last.state, "failed");
    assert!(last.error.unwrap().contains("edge.394"));
}

/// Writes `body` as the request file, as the node's user would.
fn plant(h: &Here, body: &str) {
    if let Some(dir) = h.request_path().parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(h.request_path(), body).unwrap();
}

#[test]
fn ops_014_the_update_service_trusts_nothing_in_the_request() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.7.0", true);
    let never = |_: &str| -> Result<Outcome, String> { panic!("must not install") };
    let ok = |id: &str, to: &str, at: u64| {
        format!(r#"{{"id":"{id}","to":"{to}","by":"alice","requestedMs":{at}}}"#)
    };
    for (body, why) in [
        (ok("1a-2b", "0.6.0", NOW), "older than the installed"),
        (ok("1a-2b", "0.7.0-edge.1", NOW), "older than the installed"),
        (ok("1a-2b", "0.8.0;reboot", NOW), "isn't a release"),
        (ok("../x", "0.8.0", NOW), "id is malformed"),
        (
            ok("1a-2b", "0.8.0", NOW - STALE_REQUEST_MS - 1),
            "more than an hour old",
        ),
        (ok("1a-2b", "0.8.0", NOW + 3_600_000), "from the future"),
        (
            r#"{"id":"1a","to":"0.8.0","by":"a","requestedMs":1,"url":"http://evil"}"#.to_owned(),
            "expected shape",
        ),
        ("not json".to_owned(), "expected shape"),
    ] {
        plant(&h, &body);
        let served = serve_request(&h.request_path(), &h.result, "0.7.0", NOW, never);
        assert_eq!(
            served,
            Served::Done {
                state: "refused".into(),
                restart: false
            },
            "{body}"
        );
        let last = status(&h).last.unwrap();
        assert!(
            last.error.as_deref().unwrap().contains(why),
            "{body}: {last:?}"
        );
        assert!(
            !h.request_path().exists(),
            "a refused request is removed too: {body}"
        );
    }
    // A local build never installs from a request.
    plant(&h, &ok("1a-2b", "0.8.0", NOW));
    let served = serve_request(&h.request_path(), &h.result, "dev", NOW, never);
    assert_eq!(
        served,
        Served::Done {
            state: "refused".into(),
            restart: false
        }
    );
    // The same version: up to date, nothing installed.
    plant(&h, &ok("1a-2b", "0.7.0", NOW));
    let served = serve_request(&h.request_path(), &h.result, "0.7.0", NOW, never);
    assert_eq!(
        served,
        Served::Done {
            state: "up_to_date".into(),
            restart: false
        }
    );
}

#[cfg(unix)]
#[test]
fn ops_014_the_update_service_reads_no_symlink_or_large_file() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.7.0", true);
    let never = |_: &str| -> Result<Outcome, String> { panic!("must not install") };
    let secret = d.path().join("secret");
    std::fs::write(
        &secret,
        r#"{"id":"1a","to":"0.8.0","by":"a","requestedMs":1795000000000}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(&secret, h.request_path()).unwrap();
    let served = serve_request(&h.request_path(), &h.result, "0.7.0", NOW, never);
    assert_eq!(
        served,
        Served::Done {
            state: "refused".into(),
            restart: false
        }
    );
    assert!(secret.exists(), "the link is removed, not its target");
    assert!(!h.request_path().exists());
    let last = status(&h).last.unwrap();
    assert!(last.error.unwrap().contains("regular file"));
    // A hard link to another file (two names) is refused as well.
    std::fs::hard_link(&secret, h.request_path()).unwrap();
    let served = serve_request(&h.request_path(), &h.result, "0.7.0", NOW, never);
    assert_eq!(
        served,
        Served::Done {
            state: "refused".into(),
            restart: false
        }
    );
    // And anything over 4 KiB.
    plant(&h, &" ".repeat(5000));
    let served = serve_request(&h.request_path(), &h.result, "0.7.0", NOW, never);
    assert_eq!(
        served,
        Served::Done {
            state: "refused".into(),
            restart: false
        }
    );
    // A symlink planted where the result goes is replaced, not written through.
    let outside = d.path().join("outside");
    std::fs::write(&outside, "keep").unwrap();
    std::fs::create_dir_all(h.result.parent().unwrap()).unwrap();
    std::fs::remove_file(&h.result).unwrap();
    std::os::unix::fs::symlink(&outside, &h.result).unwrap();
    plant(
        &h,
        r#"{"id":"1a","to":"0.7.0","by":"a","requestedMs":1795000000000}"#,
    );
    serve_request(&h.request_path(), &h.result, "0.7.0", NOW, never);
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
    assert!(
        !std::fs::symlink_metadata(&h.result)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

/// The one error the update service records for anything at the request that isn't a small
/// regular file of the node's own (the result is world-readable: it says no more than that).
const NOT_A_REQUEST: &str = "the request isn't a small regular file written by the node: ignored";

fn refused() -> Served {
    Served::Done {
        state: "refused".into(),
        restart: false,
    }
}

/// Makes a FIFO at `p` (`mkfifo`, from coreutils).
#[cfg(unix)]
fn mkfifo(p: &Path) {
    let ok = std::process::Command::new("mkfifo")
        .arg(p)
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "mkfifo {}", p.display());
}

/// The update service's run on a thread of its own, waited for at most 20 s: a run that blocks
/// (an open of a FIFO with no writer) fails the test instead of hanging it.
#[cfg(unix)]
fn serve_bounded(h: &Here, current: &'static str) -> Served {
    let (request, result) = (h.request_path(), h.result.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    drop(std::thread::spawn(move || {
        let _ = tx.send(serve_request(&request, &result, current, NOW, |_| {
            Err("must not install".to_owned())
        }));
    }));
    rx.recv_timeout(std::time::Duration::from_secs(20))
        .expect("the update service blocked on the request")
}

/// REQ: OPS-014 (ADR-182) — a link to a FIFO at the request is refused without the update
/// service blocking on it (the open never follows a link and never waits for a writer); the
/// link goes, the FIFO it named stays, and the result says only that it wasn't a request.
#[cfg(unix)]
#[test]
fn ops_014_a_link_to_a_fifo_is_refused_without_blocking() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.7.0", true);
    let fifo = d.path().join("fifo");
    mkfifo(&fifo);
    std::fs::create_dir_all(h.request_path().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&fifo, h.request_path()).unwrap();
    assert_eq!(serve_bounded(&h, "0.7.0"), refused());
    assert!(
        std::fs::symlink_metadata(h.request_path()).is_err(),
        "the link is removed"
    );
    assert!(fifo.exists(), "the FIFO it named is left alone");
    assert_eq!(
        status(&h).last.unwrap().error.as_deref(),
        Some(NOT_A_REQUEST)
    );
}

/// REQ: OPS-014 (ADR-182) — a FIFO or an empty directory at the request is refused and removed
/// (so the path unit isn't started again for it), with the same one error. A directory with
/// something in it can't be removed without root reaching into the node's files: it is refused
/// each time (the path unit's start limit then stops it: ADR-182, the feature switched off).
#[cfg(unix)]
#[test]
fn ops_014_a_fifo_or_directory_at_the_request_is_refused_and_removed() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.7.0", true);
    std::fs::create_dir_all(h.request_path().parent().unwrap()).unwrap();
    mkfifo(&h.request_path());
    assert_eq!(serve_bounded(&h, "0.7.0"), refused());
    assert!(
        std::fs::symlink_metadata(h.request_path()).is_err(),
        "the FIFO is removed"
    );
    assert_eq!(
        status(&h).last.unwrap().error.as_deref(),
        Some(NOT_A_REQUEST)
    );
    std::fs::create_dir(h.request_path()).unwrap();
    assert_eq!(serve_bounded(&h, "0.7.0"), refused());
    assert!(
        std::fs::symlink_metadata(h.request_path()).is_err(),
        "the empty directory is removed"
    );
    assert_eq!(
        status(&h).last.unwrap().error.as_deref(),
        Some(NOT_A_REQUEST)
    );
    std::fs::create_dir(h.request_path()).unwrap();
    std::fs::write(h.request_path().join("x"), "x").unwrap();
    assert_eq!(serve_bounded(&h, "0.7.0"), refused());
    assert!(
        h.request_path().join("x").exists(),
        "nothing inside is touched"
    );
}

/// REQ: OPS-014 (ADR-182) — the update service acts only on a version written exactly as a
/// release or edge build is: padded, with a newline, or with a leading zero is refused before
/// anything is fetched (it used to pass the downgrade check unparsed). The node never asks for
/// one either.
#[test]
fn ops_014_a_padded_or_unparseable_version_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.8.0", true);
    for to in [
        "0.9.0\n",
        " 0.9.0",
        "0.9.0 ",
        "0.09.0",
        "00.9.0",
        "0.9.0-edge.01",
    ] {
        let body = serde_json::json!({"id": "1a-2b", "to": to, "by": "alice", "requestedMs": NOW});
        plant(&h, &body.to_string());
        let asked = Cell::new(false);
        let served = serve_request(&h.request_path(), &h.result, "0.8.0", NOW, |_| {
            asked.set(true);
            Err("not here".to_owned())
        });
        assert!(!asked.get(), "{to:?}: nothing is fetched");
        assert_eq!(served, refused(), "{to:?}");
        assert!(
            status(&h)
                .last
                .unwrap()
                .error
                .unwrap()
                .contains("isn't a release or edge build version"),
            "{to:?}"
        );
    }
    for to in ["0.09.0", "00.9.0", "0.9.0-edge.01"] {
        assert_eq!(
            code(request(&h, to, "alice", NOW)),
            Code::UpdateInvalid,
            "{to:?}"
        );
    }
    assert!(!h.request_path().exists());
}

/// REQ: OPS-014 (ADR-182) — a request never changes a node's channel: a stable node takes only a
/// release (`update_channel`, 409, at the node: the same check answers the API and the cluster
/// RPC), and the update service refuses it again; an edge node takes an edge build or a newer
/// release.
#[test]
fn ops_014_a_request_never_moves_a_stable_node_to_edge() {
    let d = tempfile::tempdir().unwrap();
    let stable = here(d.path(), "native", "0.8.0", true);
    let e = request(&stable, "0.9.0-edge.1", "alice", NOW).unwrap_err();
    assert_eq!(
        (serde_json::to_value(e.code).unwrap(), e.status),
        (serde_json::json!("update_channel"), 409),
        "{e:?}"
    );
    assert!(
        e.hint
            .as_deref()
            .is_some_and(|h| h.contains("by hand with self-update")),
        "{e:?}"
    );
    assert!(!stable.request_path().exists(), "nothing is written");
    // An edge node: an edge build, or a newer release.
    let edge_dir = tempfile::tempdir().unwrap();
    let edge = here(edge_dir.path(), "native", "0.8.0-edge.5", true);
    assert_eq!(
        request(&edge, "0.8.0-edge.9", "alice", NOW)
            .unwrap()
            .state
            .as_deref(),
        Some("requested")
    );
    std::fs::remove_file(edge.request_path()).unwrap();
    assert_eq!(
        request(&edge, "0.8.0", "alice", NOW)
            .unwrap()
            .state
            .as_deref(),
        Some("requested")
    );
    // The update service, on a stable node, refuses an edge build however it was asked.
    let body =
        serde_json::json!({"id": "1a-2b", "to": "0.9.0-edge.1", "by": "alice", "requestedMs": NOW});
    plant(&stable, &body.to_string());
    let asked = Cell::new(false);
    let served = serve_request(&stable.request_path(), &stable.result, "0.8.0", NOW, |_| {
        asked.set(true);
        Err("not here".to_owned())
    });
    assert!(!asked.get(), "nothing is fetched");
    assert_eq!(served, refused());
    assert!(
        status(&stable)
            .last
            .unwrap()
            .error
            .unwrap()
            .contains("stable channel"),
    );
}

#[test]
fn ops_014_a_version_picks_its_channel() {
    assert_eq!(channel_for("0.8.0"), crate::selfupdate::Channel::Stable);
    assert_eq!(
        channel_for("0.8.0-edge.393"),
        crate::selfupdate::Channel::Edge
    );
}
