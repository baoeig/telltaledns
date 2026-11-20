// REQ: FLT-004, OBS-024, OBS-010, API-002 (ADR-180) — a local file named through the API: only an
// administrator points a list at one; what reading one went wrong with reaches a caller below
// admin only as a generic reason; and a simulation by such a caller reads no file the
// configuration in effect doesn't already name. Driven through the real routes, sign-in and
// backend (the harness of `local_access_tests`).

use std::sync::Arc;

use axum::http::StatusCode;
use telltale_api::auth;

use super::local_access_tests::{Creds, NO_PROGRAM, NO_SOCKET, Node, now};

fn marker() -> String {
    format!("marker{}", auth::crypto::random_id())
}

/// The running configuration and the files' are `cfg` (with this node's data directory).
fn configure(n: &Node, mut cfg: telltale_config::Config) {
    cfg.node.data_dir = telltale_config::SafeString::new(n.data()).unwrap();
    n.src.file_config.store(Arc::new(cfg.clone()));
    n.src.config.store(Arc::new(cfg));
}

fn file_list(path: &str) -> serde_json::Value {
    serde_json::json!({ "path": path, "kind": "allow" })
}

/// REQ: FLT-004, API-002 (ADR-180) — a list read from a local file is added, or pointed at another
/// file, only by an administrator (their session or an admin token, not an agent token): an
/// operator and agent tokens get 403 with a hint, dry run or not, and nothing is stored. An
/// operator keeps URL lists and inline rules, may change the administrator's file list without
/// choosing another file, and may delete it.
#[tokio::test]
async fn flt_004_only_an_admin_names_a_list_file() {
    let n = Node::new(telltale_config::Config::default());
    let path = "/etc/telltale/allow.txt";
    for (who, creds) in [
        ("operator", &n.operator),
        ("operator's agent", &n.operator_agent),
        ("admin's agent", &n.admin_agent),
    ] {
        for dry in [true, false] {
            let (s, v) = n
                .put("/api/v1/lists/family-allow", creds, dry, file_list(path))
                .await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{who}, dry={dry}: {v}");
            assert_eq!(v["code"], "forbidden", "{v}");
            assert!(
                v["detail"].as_str().unwrap_or("").contains("`path`"),
                "{who}: the field is named: {v}"
            );
            assert!(
                v["hint"]
                    .as_str()
                    .unwrap_or("")
                    .contains("needs an administrator"),
                "{who}: {v}"
            );
        }
    }
    assert!(
        crate::managed::entries(n.auth.state()).lists.is_empty(),
        "nothing was stored"
    );
    // An administrator may, with a token or a session (the documented place for such a file).
    for (who, creds) in [
        ("admin token", &n.admin),
        ("admin session", &n.admin_session),
    ] {
        let (s, v) = n
            .put("/api/v1/lists/family-allow", creds, true, file_list(path))
            .await;
        assert_eq!(s, StatusCode::OK, "{who}: {v}");
    }
    let (s, v) = n
        .put(
            "/api/v1/lists/family-allow",
            &n.admin_session,
            false,
            file_list(path),
        )
        .await;
    assert_eq!(
        (s, v["applied"].as_bool()),
        (StatusCode::OK, Some(true)),
        "{v}"
    );
    // An operator keeps every other kind of list.
    for (name, body, creds) in [
        (
            "ads",
            serde_json::json!({ "url": "https://lists.example.com/hosts.txt" }),
            &n.operator,
        ),
        (
            "manual",
            serde_json::json!({ "rules": ["||ads.example^"] }),
            &n.operator,
        ),
        (
            "agent-made",
            serde_json::json!({ "rules": ["||tracker.example^"] }),
            &n.operator_agent,
        ),
    ] {
        let (s, v) = n
            .put(&format!("/api/v1/lists/{name}"), creds, false, body)
            .await;
        assert_eq!(s, StatusCode::OK, "{name}: {v}");
    }
    // ...and may change the administrator's file list while it reads the same file.
    let mut off = file_list(path);
    off["enabled"] = false.into();
    let (s, v) = n
        .put("/api/v1/lists/family-allow", &n.operator, false, off)
        .await;
    assert_eq!(s, StatusCode::OK, "the same file: {v}");
    // Another file is the administrator's choice.
    let (s, v) = n
        .put(
            "/api/v1/lists/family-allow",
            &n.operator,
            true,
            file_list("/etc/telltale/other.txt"),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "another file: {v}");
    // Deleting only takes something away: an operator may.
    let (s, v) = n
        .send("DELETE", "/api/v1/lists/family-allow", &n.operator, None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

fn error_of<'a>(v: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    v["items"]
        .as_array()?
        .iter()
        .find(|l| l["name"] == name)?
        .get("error")?
        .as_str()
}

/// REQ: FLT-004, API-002 (ADR-180) — what went wrong reading a list from a local file reaches a
/// caller below admin as a generic reason, never a line of the file; an administrator sees the
/// whole message (the node's log has it too). A URL list's error is shown to everyone as before.
#[tokio::test]
async fn flt_004_a_local_list_error_is_generic_below_admin() {
    use telltale_filter::fetch::{
        Client, FetchSettings, Fetcher, ListMeta, ListSpec, Outcome, Store, SystemResolver,
    };
    let n = Node::new(telltale_config::Config::default());
    let data = n.data();
    let mark = marker();
    let file = format!("{data}/hosts-copy.txt");
    std::fs::write(
        &file,
        format!("0.0.0.0 bad!{mark}\n0.0.0.0 also!{mark}\n0.0.0.0 more!{mark}\n"),
    )
    .unwrap();
    let cfg: telltale_config::Config = toml::from_str(&format!(
        r#"
        [[list]]
        name = "local"
        path = "{file}"
        [[list]]
        name = "web"
        url = "https://lists.example.com/hosts.txt"
        "#
    ))
    .unwrap();
    configure(&n, cfg.clone());
    let store = Store::open(&std::path::Path::new(&data).join("lists")).unwrap();
    store
        .save_meta(
            "web",
            &ListMeta {
                source: "https://lists.example.com/hosts.txt".into(),
                last_attempt: Some(5_000),
                last_error: Some("HTTP 404".into()),
                consecutive_failures: 2,
                ..ListMeta::default()
            },
        )
        .unwrap();
    let fetcher = Arc::new(Fetcher::new(
        store,
        Client::new(Arc::new(SystemResolver), &[]).unwrap(),
        FetchSettings::from_config(&cfg),
    ));
    let specs = ListSpec::from_config(&cfg);
    let _ = fetcher.due(&specs, now());
    let local = specs.iter().find(|s| s.name == "local").unwrap();
    let outcome = fetcher.refresh_one(local).await;
    assert!(
        matches!(&outcome, Outcome::Failed(e) if e.contains(&mark)),
        "the parser's reason quotes the line: {outcome:?}"
    );
    n.src.lists.store(Some(Arc::new(crate::lists::ListsShared {
        fetcher,
        compiled: std::sync::Mutex::new(None),
    })));
    for (who, creds) in [
        ("viewer", &n.viewer),
        ("operator", &n.operator),
        ("operator's agent", &n.operator_agent),
        ("admin's agent", &n.admin_agent),
    ] {
        let (s, v) = n.send("GET", "/api/v1/lists", creds, None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert!(!v.to_string().contains(&mark), "{who}: {v}");
        let reason = error_of(&v, "local").unwrap_or_default();
        assert!(
            reason.contains("administrator"),
            "{who}: a generic reason: {v}"
        );
        assert_eq!(
            error_of(&v, "web"),
            Some("HTTP 404"),
            "{who}: a URL list's error is as it was"
        );
    }
    for (who, creds) in [
        ("admin token", &n.admin),
        ("admin session", &n.admin_session),
    ] {
        let (s, v) = n.send("GET", "/api/v1/lists", creds, None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert!(
            error_of(&v, "local").unwrap_or_default().contains(&mark),
            "{who}: an administrator sees the whole message: {v}"
        );
    }
}

/// REQ: API-002 (ADR-180) — a pre-save check whose TLS file can't be used tells a caller below
/// admin only that, the same whether the file is there or not; an administrator sees why.
#[tokio::test]
async fn api_002_a_tls_file_error_in_a_check_is_generic_below_admin() {
    let n = Node::new(telltale_config::Config::default());
    let d = n.data();
    let bad = format!("{d}/ca-bad.pem");
    let missing = format!("{d}/ca-missing.pem");
    std::fs::write(&bad, format!("-----BEGIN {}\n", marker())).unwrap();
    let body = |ca: &str| serde_json::json!({ "url": "tls://192.0.2.53", "tls_server_name": "dns.example", "tls_ca": ca });
    for (who, creds) in [
        ("operator", &n.operator),
        ("operator's agent", &n.operator_agent),
        ("admin's agent", &n.admin_agent),
    ] {
        let (s1, v1) = n
            .send("POST", "/api/v1/checks/upstream", creds, Some(body(&bad)))
            .await;
        let (s2, v2) = n
            .send(
                "POST",
                "/api/v1/checks/upstream",
                creds,
                Some(body(&missing)),
            )
            .await;
        assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK), "{v1} {v2}");
        assert_eq!(v1["ok"], false, "{v1}");
        for v in [&v1, &v2] {
            let text = v.to_string();
            assert!(
                !text.contains("illegal section start")
                    && !text.contains("os error")
                    && !text.contains("No such file"),
                "{who}: {v}"
            );
            assert!(
                v["error"].as_str().unwrap_or("").contains("administrator"),
                "{who}: a generic reason: {v}"
            );
        }
        assert_eq!(v1, v2, "{who}: a file that is there and one that isn't");
    }
    for (who, creds) in [
        ("admin token", &n.admin),
        ("admin session", &n.admin_session),
    ] {
        let (_, v) = n
            .send("POST", "/api/v1/checks/upstream", creds, Some(body(&bad)))
            .await;
        assert!(
            v["error"]
                .as_str()
                .unwrap_or("")
                .contains("illegal section start"),
            "{who}: {v}"
        );
        let (_, v) = n
            .send(
                "POST",
                "/api/v1/checks/upstream",
                creds,
                Some(body(&missing)),
            )
            .await;
        assert!(
            v["error"].as_str().unwrap_or("").contains("os error"),
            "{who}: {v}"
        );
    }
}

/// REQ: OBS-010 (ADR-180) — a test send whose private CA file (`tls_ca`) can't be used tells a
/// caller below admin only that, the same whether the file is there or not; an administrator
/// sees why.
#[tokio::test]
async fn obs_010_a_tls_file_error_in_a_test_send_is_generic_below_admin() {
    let n = Node::new(telltale_config::Config::default());
    let d = n.data();
    let bad = format!("{d}/mail-ca-bad.pem");
    let missing = format!("{d}/mail-ca-missing.pem");
    std::fs::write(&bad, format!("-----BEGIN {}\n", marker())).unwrap();
    for (name, ca) in [("mail-bad", &bad), ("mail-missing", &missing)] {
        let (s, v) = n
            .put(
                &format!("/api/v1/alerts/destinations/{name}"),
                &n.operator,
                false,
                serde_json::json!({
                    "type": "email", "url": "smtp://127.0.0.1:9", "from": "dns@example.com",
                    "to": ["me@example.com"], "tls_ca": ca,
                }),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{name}: {v}");
    }
    let send_test = |name: &str| format!("/api/v1/alerts/destinations/{name}/test");
    for (who, creds) in [
        ("operator", &n.operator),
        ("operator's agent", &n.operator_agent),
    ] {
        let (s1, v1) = n.send("POST", &send_test("mail-bad"), creds, None).await;
        let (s2, v2) = n
            .send("POST", &send_test("mail-missing"), creds, None)
            .await;
        assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK), "{v1} {v2}");
        for v in [&v1, &v2] {
            assert_eq!(v["ok"], false, "{v}");
            let text = v.to_string();
            assert!(
                !text.contains("illegal section start")
                    && !text.contains("os error")
                    && !text.contains("No such file"),
                "{who}: {v}"
            );
            assert!(
                v["error"].as_str().unwrap_or("").contains("administrator"),
                "{who}: a generic reason: {v}"
            );
        }
        assert_eq!(v1, v2, "{who}: a file that is there and one that isn't");
    }
    for (who, creds) in [
        ("admin token", &n.admin),
        ("admin session", &n.admin_session),
    ] {
        let (_, v) = n.send("POST", &send_test("mail-bad"), creds, None).await;
        assert!(
            v["error"]
                .as_str()
                .unwrap_or("")
                .contains("illegal section start"),
            "{who}: {v}"
        );
        let (_, v) = n
            .send("POST", &send_test("mail-missing"), creds, None)
            .await;
        assert!(
            v["error"].as_str().unwrap_or("").contains("os error"),
            "{who}: {v}"
        );
    }
}

/// A request that may run a simulation, again while another simulation in this test binary holds
/// the node's one slot (409), within a bound.
async fn while_busy(
    n: &Node,
    method: &str,
    uri: &str,
    creds: &Creds,
    body: &serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    for _ in 0..400 {
        let r = n.send(method, uri, creds, Some(body.clone())).await;
        if r.0 != StatusCode::CONFLICT {
            return r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the simulation slot stayed busy");
}

/// `POST /api/v1/simulate` with `config` as the candidate.
async fn simulate(
    n: &Node,
    creds: &Creds,
    config: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    while_busy(
        n,
        "POST",
        "/api/v1/simulate",
        creds,
        &serde_json::json!({ "config": config }),
    )
    .await
}

/// REQ: OBS-024 (ADR-180) — a simulation run by a caller below admin names no local file,
/// program or socket that the configuration in effect doesn't already name: a candidate with a
/// new zone file, hosts file, list file, upstream TLS file or plugin upstream is refused before
/// anything is read (alike whether the file is there or not), and nothing from the file comes
/// back. A candidate that names only the configuration's own files runs as before, and so does an
/// administrator's.
#[tokio::test]
async fn obs_024_a_simulation_below_admin_names_no_new_local_file() {
    let n = Node::new(telltale_config::Config::default());
    let data = n.data();
    let mark = marker();
    let path = |f: &str| format!("{data}/{f}");
    std::fs::write(path("zone-new.db"), format!("www IN A {mark}\n")).unwrap();
    std::fs::write(path("zone-ok.db"), "www IN A 192.0.2.5\n").unwrap();
    std::fs::write(path("hosts-new"), format!("192.0.2.9 {mark}.example\n")).unwrap();
    std::fs::write(path("list-new.txt"), format!("{mark}.example\n")).unwrap();
    std::fs::write(path("ca-new.pem"), format!("-----BEGIN {mark}\n")).unwrap();
    let missing = path("not-there");
    // The configuration in effect reads one zone file of its own.
    let cfg: telltale_config::Config = toml::from_str(&format!(
        "[[zone]]\nname = \"corp.example\"\nfile = \"{}\"\n",
        path("zone-ok.db")
    ))
    .unwrap();
    configure(&n, cfg);
    let own_zone = serde_json::json!([{ "name": "corp.example", "file": path("zone-ok.db") }]);
    let zone = |f: &str| serde_json::json!({ "zone": [{ "name": "corp.example", "file": f }] });
    let hosts = |f: &str| serde_json::json!({ "zone": own_zone, "local": { "hosts_files": [f] } });
    let list =
        |f: &str| serde_json::json!({ "zone": own_zone, "list": [{ "name": "new", "path": f }] });
    let tls = |f: &str| {
        serde_json::json!({
            "zone": own_zone,
            "upstream": [{ "name": "u", "url": "tls://192.0.2.53", "tls_server_name": "dns.example", "tls_ca": f }],
            "upstream_group": [{ "name": "default", "members": ["u"] }],
        })
    };
    let plugin = |url: String| {
        serde_json::json!({
            "zone": own_zone,
            "upstream": [{ "name": "p", "url": url }],
            "upstream_group": [{ "name": "default", "members": ["p"] }],
        })
    };
    let candidates = [
        ("a zone file", zone(&path("zone-new.db")), zone(&missing)),
        ("a hosts file", hosts(&path("hosts-new")), hosts(&missing)),
        ("a list file", list(&path("list-new.txt")), list(&missing)),
        ("a TLS file", tls(&path("ca-new.pem")), tls(&missing)),
        (
            "a program or a socket",
            plugin(format!("exec://{NO_PROGRAM}")),
            plugin(format!("unix://{NO_SOCKET}")),
        ),
    ];
    for (who, creds) in [
        ("viewer", &n.viewer),
        ("operator", &n.operator),
        ("operator's agent", &n.operator_agent),
        ("admin's agent", &n.admin_agent),
    ] {
        for (what, there, other) in &candidates {
            let (s1, v1) = simulate(&n, creds, there.clone()).await;
            let (s2, v2) = simulate(&n, creds, other.clone()).await;
            for (s, v) in [(s1, &v1), (s2, &v2)] {
                let text = v.to_string();
                assert!(
                    !text.contains(&mark) && !text.contains("illegal section start"),
                    "{who}, {what}: nothing from the file comes back: {v}"
                );
                assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{who}, {what}: {v}");
                assert!(
                    v["hint"].as_str().unwrap_or("").contains("administrator"),
                    "{who}, {what}: {v}"
                );
            }
            assert_eq!(v1, v2, "{who}, {what}: there or not, the same answer");
        }
    }
    // The configuration's own zone file, and a new inline list: not refused.
    let (s, v) = simulate(
        &n,
        &n.viewer,
        serde_json::json!({ "zone": own_zone, "list": [{ "name": "new", "rules": ["||x.example^"] }] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // An administrator's candidate is read as before: the new zone file's mistake is named.
    let (s, v) = simulate(&n, &n.admin, zone(&path("zone-new.db"))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v.to_string().contains(&mark), "{v}");
}

/// REQ: OBS-024 (ADR-180) — a dry run's simulation is the same door: an operator's change that
/// names a new TLS file is planned as before, and its simulation says it wasn't run, without
/// reading the file; an administrator's runs as before.
#[tokio::test]
async fn obs_024_a_dry_run_simulation_below_admin_reads_no_new_local_file() {
    let n = Node::new(telltale_config::Config::default());
    let d = n.data();
    let ca = format!("{d}/ca-new.pem");
    std::fs::write(&ca, format!("-----BEGIN {}\n", marker())).unwrap();
    let body = serde_json::json!({ "url": "tls://192.0.2.53", "tls_server_name": "dns.example", "tls_ca": ca });
    let uri = "/api/v1/upstreams/private?dryRun=true&simulate=true";
    let (s, v) = while_busy(&n, "PUT", uri, &n.operator, &body).await;
    assert!(!v.to_string().contains("illegal section start"), "{v}");
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["applied"], false, "{v}");
    assert_eq!(v["simulation"]["available"], false, "{v}");
    assert_eq!(v["simulation"]["reason"], "local_file", "{v}");
    let (s, v) = while_busy(&n, "PUT", uri, &n.admin, &body).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v.to_string().contains("illegal section start"), "{v}");
}
