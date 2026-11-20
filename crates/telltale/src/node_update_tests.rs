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
    let node = here(dir.path(), "native", "0.7.0", true);
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
    let h = here(d.path(), "native", "0.7.0", true);
    request(&h, "0.8.0-edge.393", "alice", NOW).unwrap();
    let served = serve_request(&h.request_path(), &h.result, "0.7.0", NOW, |_| {
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

#[test]
fn ops_014_a_version_picks_its_channel() {
    assert_eq!(channel_for("0.8.0"), crate::selfupdate::Channel::Stable);
    assert_eq!(
        channel_for("0.8.0-edge.393"),
        crate::selfupdate::Channel::Edge
    );
}
