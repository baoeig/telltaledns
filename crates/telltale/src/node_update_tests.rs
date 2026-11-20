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

/// A test's install: what the signed self-update would return for a version.
type Install<'a> = Box<dyn FnOnce(&str) -> Result<Outcome, String> + 'a>;

/// The install a test gives the update service, with a node that restarts and is healthy.
struct Installs<'a>(Option<Install<'a>>);

impl Host for Installs<'_> {
    fn install(&mut self, to: &str) -> Result<Outcome, String> {
        self.0
            .take()
            .map_or_else(|| Err("asked twice".to_owned()), |f| f(to))
    }
    fn restart(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn healthy(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn restore(&mut self, _: &Path) -> Result<(), String> {
        Ok(())
    }
}

/// The update service's run with `install`, for a node that restarts and comes up healthy.
fn serve<'a>(
    request: &Path,
    result: &Path,
    current: &str,
    now: u64,
    install: impl FnOnce(&str) -> Result<Outcome, String> + 'a,
) -> Served {
    serve_request(
        request,
        result,
        current,
        now,
        true,
        &mut Installs(Some(Box::new(install))),
    )
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
    // REQ: OPS-014 (ADR-182) — in its own folder, `<data_dir>/update/request.json`: the only
    // place of the data directory the update service may write.
    let written = dir.path().join("update").join("request.json");
    assert_eq!(node.request_path(), written);
    let file: RequestFile = serde_json::from_slice(&std::fs::read(&written).unwrap()).unwrap();
    assert_eq!(file.id, asked.id);
    assert_eq!(file.requested_ms, NOW);
    // Nothing else in the data directory, and no temporary file left behind.
    let names = |p: &Path| -> Vec<String> {
        std::fs::read_dir(p)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(names(dir.path()), vec!["update".to_owned()]);
    assert_eq!(
        names(&dir.path().join("update")),
        vec!["request.json".to_owned()]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&written), 0o640);
        assert_eq!(mode(&dir.path().join("update")), 0o750);
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
    let served = serve(&h.request_path(), &h.result, "0.7.0", NOW + 1000, |to| {
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
            state: "updated".into()
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
    let again = serve(
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
    let mut host = Scripted {
        install: Some(Err("SHA256SUMS: the signature does not verify".into())),
        ..Scripted::default()
    };
    let served = serve_request(
        &h.request_path(),
        &h.result,
        "0.7.0-edge.400",
        NOW,
        true,
        &mut host,
    );
    assert_eq!(
        served,
        Served::Done {
            state: "failed".into()
        }
    );
    assert_eq!(host.calls, ["install 0.8.0-edge.393"], "never restarted");
    let last = status(&h).last.unwrap();
    assert_eq!(last.state, "failed");
    assert!(last.error.unwrap().contains("does not verify"));
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
        let served = serve(&h.request_path(), &h.result, "0.7.0", NOW, never);
        assert_eq!(
            served,
            Served::Done {
                state: "refused".into()
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
    let served = serve(&h.request_path(), &h.result, "dev", NOW, never);
    assert_eq!(
        served,
        Served::Done {
            state: "refused".into()
        }
    );
    // The same version: up to date, nothing installed.
    plant(&h, &ok("1a-2b", "0.7.0", NOW));
    let served = serve(&h.request_path(), &h.result, "0.7.0", NOW, never);
    assert_eq!(
        served,
        Served::Done {
            state: "up_to_date".into()
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
    std::fs::create_dir_all(h.request_path().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&secret, h.request_path()).unwrap();
    let served = serve(&h.request_path(), &h.result, "0.7.0", NOW, never);
    assert_eq!(
        served,
        Served::Done {
            state: "refused".into()
        }
    );
    assert!(secret.exists(), "the link is removed, not its target");
    assert!(!h.request_path().exists());
    let last = status(&h).last.unwrap();
    assert!(last.error.unwrap().contains("regular file"));
    // A hard link to another file (two names) is refused as well.
    std::fs::hard_link(&secret, h.request_path()).unwrap();
    let served = serve(&h.request_path(), &h.result, "0.7.0", NOW, never);
    assert_eq!(
        served,
        Served::Done {
            state: "refused".into()
        }
    );
    // And anything over 4 KiB.
    plant(&h, &" ".repeat(5000));
    let served = serve(&h.request_path(), &h.result, "0.7.0", NOW, never);
    assert_eq!(
        served,
        Served::Done {
            state: "refused".into()
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
    serve(&h.request_path(), &h.result, "0.7.0", NOW, never);
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
    }
}

/// Makes a FIFO at `p`, in this process: a child process (`mkfifo`) forked while another test
/// writes a binary it is about to run (the pinned self-update tests) can make that `exec` fail
/// with "Text file busy".
#[cfg(unix)]
fn mkfifo(p: &Path) {
    use rustix::fs::{CWD, FileType, Mode};
    rustix::fs::mknodat(CWD, p, FileType::Fifo, Mode::from_raw_mode(0o600), 0)
        .unwrap_or_else(|e| panic!("mkfifo {}: {e}", p.display()));
}

/// The update service's run on a thread of its own, waited for at most 20 s: a run that blocks
/// (an open of a FIFO with no writer) fails the test instead of hanging it.
#[cfg(unix)]
fn serve_bounded(h: &Here, current: &'static str) -> Served {
    let (request, result) = (h.request_path(), h.result.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    drop(std::thread::spawn(move || {
        let _ = tx.send(serve(&request, &result, current, NOW, |_| {
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
        let served = serve(&h.request_path(), &h.result, "0.8.0", NOW, |_| {
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
    let served = serve(&stable.request_path(), &stable.result, "0.8.0", NOW, |_| {
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

/// REQ: OPS-014 (ADR-182) — the request's folder must be the node's own real directory: one
/// swapped for a link to another folder, or one anyone may write, is refused and nothing in it is
/// read or removed (root writes only in the node's `update/`, never through a link).
#[cfg(unix)]
#[test]
fn ops_014_a_request_folder_that_isnt_the_nodes_is_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.7.0", true);
    let elsewhere = d.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let body = serde_json::json!({"id": "1a-2b", "to": "0.8.0", "by": "a", "requestedMs": NOW});
    std::fs::write(elsewhere.join("request.json"), body.to_string()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, d.path().join("update")).unwrap();
    assert_eq!(serve_bounded(&h, "0.7.0"), refused());
    assert!(
        elsewhere.join("request.json").exists(),
        "nothing is removed through the link"
    );
    assert_eq!(
        status(&h).last.unwrap().error.as_deref(),
        Some(NOT_A_REQUEST)
    );
    std::fs::remove_file(d.path().join("update")).unwrap();
    plant(&h, &body.to_string());
    std::fs::set_permissions(
        h.request_path().parent().unwrap(),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert_eq!(serve_bounded(&h, "0.7.0"), refused());
    assert!(
        h.request_path().exists(),
        "a folder anyone may write isn't the node's: nothing in it is touched"
    );
}

/// REQ: OPS-014 (ADR-182) — the shipped units agree with the code and keep root's reach small:
/// the path unit watches `<data_dir>/update/request.json`; the service reads it there and may
/// write only that folder, the binary's directory and its own state, with one capability and the
/// node unit's own sandbox; `systemctl` is run by its absolute path.
#[test]
fn ops_014_the_units_watch_and_write_only_the_request_folder() {
    let path = include_str!("../../../deploy/systemd/telltale-update.path");
    let svc = include_str!("../../../deploy/systemd/telltale-update.service");
    // `Here::request_path` (ops_014_a_request_is_written_...) for the default data directory.
    let request = "/var/lib/telltale/update/request.json".to_owned();
    let lines = |unit: &'static str, key: &str| -> Vec<&'static str> {
        unit.lines()
            .filter_map(|l| l.strip_prefix(key).and_then(|l| l.strip_prefix('=')))
            .collect()
    };
    assert_eq!(lines(path, "PathExists"), [request.as_str()]);
    assert_eq!(
        lines(svc, "ExecStart"),
        [format!("/usr/local/bin/telltale self-update --request {request} --restart").as_str()]
    );
    assert_eq!(
        lines(svc, "ReadWritePaths"),
        ["/usr/local/bin -/var/lib/telltale/update"]
    );
    for (key, want) in [
        ("CapabilityBoundingSet", "CAP_DAC_OVERRIDE"),
        ("NoNewPrivileges", "yes"),
        ("ProtectSystem", "strict"),
        ("PrivateTmp", "yes"),
        ("ProtectProc", "invisible"),
        ("MemoryDenyWriteExecute", "yes"),
        ("RemoveIPC", "yes"),
        ("SystemCallFilter", "@system-service"),
        ("RestrictAddressFamilies", "AF_INET AF_INET6 AF_UNIX"),
        ("Type", "oneshot"),
    ] {
        assert_eq!(lines(svc, key), [want], "{key}");
    }
    assert!(lines(svc, "User").is_empty() && lines(svc, "Environment").len() == 2);
    // `systemctl` is never looked up through PATH (the service may write /usr/local/bin).
    for (file, src) in [
        ("main.rs", include_str!("main.rs")),
        ("selfupdate.rs", include_str!("selfupdate.rs")),
    ] {
        assert!(
            !src.contains("Command::new(\"systemctl\")"),
            "{file} runs systemctl through PATH"
        );
    }
}

/// REQ: OPS-014 (ADR-182) — one at a time per node: a second request while the first waits for
/// the update service gets `update_in_progress` (409) and the first is kept as it was; so does
/// one while a run says `installing`, but not once that run is older than the update service's
/// limit (it died), nor once the first request is older than the service would take.
#[test]
fn ops_014_a_second_request_waits_for_the_first() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.7.0", true);
    let first = request(&h, "0.8.0", "alice", NOW).unwrap().pending.unwrap();
    let code_of = |e: &Problem| serde_json::to_value(e.code).unwrap();
    let e = request(&h, "0.8.1", "bob", NOW + 1000).unwrap_err();
    assert_eq!(
        (code_of(&e), e.status),
        (serde_json::json!("update_in_progress"), 409),
        "{e:?}"
    );
    let kept: RequestFile =
        serde_json::from_slice(&std::fs::read(h.request_path()).unwrap()).unwrap();
    assert_eq!(
        (kept.id.as_str(), kept.to.as_str()),
        (first.id.as_str(), "0.8.0")
    );
    // A request the update service would refuse for its age is replaced.
    let later = NOW + STALE_REQUEST_MS + 1;
    assert_eq!(
        request(&h, "0.8.1", "bob", later)
            .unwrap()
            .pending
            .unwrap()
            .to,
        "0.8.1"
    );
    // A run under way holds a new request off; one that has said `installing` for longer than
    // the update service may run doesn't.
    std::fs::remove_file(h.request_path()).unwrap();
    let installing = |at_ms: u64| ResultFile {
        id: "1a-2b".into(),
        to: "0.8.1".into(),
        from: "0.7.0".into(),
        state: "installing".into(),
        error: None,
        by: "bob".into(),
        at_ms,
    };
    std::fs::create_dir_all(h.result.parent().unwrap()).unwrap();
    std::fs::write(&h.result, serde_json::to_vec(&installing(later)).unwrap()).unwrap();
    let e = request(&h, "0.8.2", "carol", later + 60_000).unwrap_err();
    assert_eq!(
        code_of(&e),
        serde_json::json!("update_in_progress"),
        "{e:?}"
    );
    assert!(!h.request_path().exists());
    let after = later + 15 * 60 * 1000 + 1;
    assert_eq!(
        request(&h, "0.8.2", "carol", after)
            .unwrap()
            .state
            .as_deref(),
        Some("requested")
    );
}

/// REQ: OPS-014, AGT-002 (ADR-182) — a dry run makes the node's checks and says what would
/// happen, and writes nothing: `would_request`, `up_to_date`, or the same refusal a request
/// gets.
#[test]
fn ops_014_a_dry_run_writes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.7.0", true);
    assert_eq!(
        dry_run(&h, "0.8.0", NOW).unwrap().state.as_deref(),
        Some("would_request")
    );
    assert_eq!(
        dry_run(&h, "0.7.0", NOW).unwrap().state.as_deref(),
        Some("up_to_date")
    );
    assert_eq!(
        dry_run(&h, "0.6.0", NOW).unwrap_err().code,
        Code::UpdateDowngrade
    );
    assert_eq!(
        dry_run(&h, "0.8.0-edge.1", NOW).unwrap_err().code,
        Code::UpdateChannel
    );
    assert!(
        std::fs::read_dir(d.path()).unwrap().next().is_none(),
        "nothing written"
    );
    request(&h, "0.8.0", "alice", NOW).unwrap();
    assert_eq!(
        dry_run(&h, "0.8.1", NOW).unwrap_err().code,
        Code::UpdateInProgress
    );
}

/// A host whose steps are scripted: the install's outcome, each health check's answer in turn,
/// and whether the previous binary can be put back. `calls` has every step, in order.
#[derive(Default)]
struct Scripted {
    install: Option<Result<Outcome, String>>,
    health: Vec<Result<(), String>>,
    restore_fails: bool,
    calls: Vec<String>,
}

impl Host for Scripted {
    fn install(&mut self, to: &str) -> Result<Outcome, String> {
        self.calls.push(format!("install {to}"));
        self.install
            .take()
            .unwrap_or_else(|| Err("asked twice".into()))
    }
    fn restart(&mut self) -> Result<(), String> {
        self.calls.push("restart".into());
        Ok(())
    }
    fn healthy(&mut self) -> Result<(), String> {
        self.calls.push("healthy".into());
        if self.health.is_empty() {
            Ok(())
        } else {
            self.health.remove(0)
        }
    }
    fn restore(&mut self, previous: &Path) -> Result<(), String> {
        self.calls.push(format!("restore {}", previous.display()));
        if self.restore_fails {
            Err("no such file".into())
        } else {
            Ok(())
        }
    }
}

/// The install's outcome when it swapped the new build in.
fn installed() -> Outcome {
    Outcome::Updated {
        previous: PathBuf::from("/usr/local/bin/telltale.old"),
    }
}

/// REQ: OPS-014 (ADR-182) — the health gate: after the restart the new build must come up
/// healthy. One that doesn't is rolled back once: the previous binary put back, the node restarted
/// and checked again, and the run recorded as `rolled_back` with why (`ctl` stops on it, and
/// systemd sees a failed run). A healthy one is `updated` with no rollback.
#[test]
fn ops_014_a_build_that_does_not_come_up_is_rolled_back() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.7.0", true);
    let asked = request(&h, "0.8.0", "alice", NOW).unwrap().pending.unwrap();
    let mut host = Scripted {
        install: Some(Ok(installed())),
        health: vec![Err("status 503".into()), Ok(())],
        ..Scripted::default()
    };
    let served = serve_request(&h.request_path(), &h.result, "0.7.0", NOW, true, &mut host);
    assert_eq!(
        served,
        Served::Done {
            state: "rolled_back".into()
        }
    );
    assert_eq!(
        host.calls,
        [
            "install 0.8.0",
            "restart",
            "healthy",
            "restore /usr/local/bin/telltale.old",
            "restart",
            "healthy"
        ]
    );
    let last = status(&h).last.unwrap();
    assert_eq!(
        (last.id.as_str(), last.state.as_str()),
        (asked.id.as_str(), "rolled_back")
    );
    let why = last.error.unwrap();
    assert!(
        why.contains("status 503") && why.contains("0.7.0 is back"),
        "{why}"
    );
    assert!(!served_ok("rolled_back"), "systemd sees a failed run");
    // Healthy at once: updated, nothing put back.
    request(&h, "0.8.0", "alice", NOW).unwrap();
    let mut host = Scripted {
        install: Some(Ok(installed())),
        ..Scripted::default()
    };
    let served = serve_request(&h.request_path(), &h.result, "0.7.0", NOW, true, &mut host);
    assert_eq!(
        served,
        Served::Done {
            state: "updated".into()
        }
    );
    assert_eq!(host.calls, ["install 0.8.0", "restart", "healthy"]);
    assert!(served_ok("updated") && served_ok("refused"));
}

/// REQ: OPS-014 (ADR-182) — one attempt only: if the rolled-back node isn't healthy either, or
/// the previous binary can't be put back, the run records `failed` with both reasons and stops
/// (no second rollback, no loop, no restart onto a binary that isn't there).
#[test]
fn ops_014_a_failed_rollback_is_recorded_and_stops() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.7.0", true);
    request(&h, "0.8.0", "alice", NOW).unwrap();
    let mut host = Scripted {
        install: Some(Ok(installed())),
        health: vec![Err("status 503".into()), Err("connection refused".into())],
        ..Scripted::default()
    };
    let served = serve_request(&h.request_path(), &h.result, "0.7.0", NOW, true, &mut host);
    assert_eq!(
        served,
        Served::Done {
            state: "failed".into()
        }
    );
    assert_eq!(
        host.calls,
        [
            "install 0.8.0",
            "restart",
            "healthy",
            "restore /usr/local/bin/telltale.old",
            "restart",
            "healthy"
        ]
    );
    let why = status(&h).last.unwrap().error.unwrap();
    assert!(
        why.contains("status 503") && why.contains("connection refused"),
        "{why}"
    );
    // The previous binary can't be put back: failed, with no restart after it.
    request(&h, "0.8.0", "alice", NOW).unwrap();
    let mut host = Scripted {
        install: Some(Ok(installed())),
        health: vec![Err("status 503".into())],
        restore_fails: true,
        ..Scripted::default()
    };
    let served = serve_request(&h.request_path(), &h.result, "0.7.0", NOW, true, &mut host);
    assert_eq!(
        served,
        Served::Done {
            state: "failed".into()
        }
    );
    assert_eq!(
        host.calls,
        [
            "install 0.8.0",
            "restart",
            "healthy",
            "restore /usr/local/bin/telltale.old"
        ]
    );
    assert!(!served_ok("failed"));
}

/// REQ: OPS-014 (ADR-182) — an edge build main has since passed can't be fetched: the update
/// service records `unavailable`, with the build the channel serves (so `ctl upgrade` can tell it
/// from a failure), installs nothing and doesn't restart the node; systemd sees a failed run.
#[test]
fn ops_014_an_edge_build_no_longer_served_is_unavailable() {
    let d = tempfile::tempdir().unwrap();
    let h = here(d.path(), "native", "0.8.0-edge.1", true);
    request(&h, "0.8.0-edge.5", "hook", NOW).unwrap();
    let mut host = Scripted {
        install: Some(Ok(Outcome::Unavailable {
            serves: "0.8.0-edge.9".into(),
            commit: "abc1234".into(),
        })),
        ..Scripted::default()
    };
    let served = serve_request(
        &h.request_path(),
        &h.result,
        "0.8.0-edge.1",
        NOW,
        true,
        &mut host,
    );
    assert_eq!(
        served,
        Served::Done {
            state: "unavailable".into()
        }
    );
    assert_eq!(host.calls, ["install 0.8.0-edge.5"], "nothing restarted");
    let last = status(&h).last.unwrap();
    assert_eq!(last.state, "unavailable");
    assert!(
        last.error.unwrap().contains("now serves 0.8.0-edge.9"),
        "the build the channel serves"
    );
    assert!(!served_ok("unavailable"));
}

#[test]
fn ops_014_a_version_picks_its_channel() {
    assert_eq!(channel_for("0.8.0"), crate::selfupdate::Channel::Stable);
    assert_eq!(
        channel_for("0.8.0-edge.393"),
        crate::selfupdate::Channel::Edge
    );
}
