// REQ: T8.2, API-002 (ADR-181) — routers made through the API and the UI, with keys pasted and
// saved as files on the node that took the write, driven through the real routes, sign-in and
// backend. The router consoles here are loopback fakes; nothing leaves the machine.

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use telltale_api::auth::{self, Auth, Role};
use tower::ServiceExt as _;

use super::*;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// How a caller signs in: the headers it sends.
type Creds = Vec<(&'static str, String)>;

fn bearer(t: &str) -> Creds {
    vec![("authorization", format!("Bearer {t}"))]
}

fn token(auth: &Auth, user: i64, scope: &str, agent_scopes: Option<&str>) -> Creds {
    let (id, secret) = (auth::crypto::random_id(), auth::crypto::random_secret());
    auth.state()
        .create_token(
            &id,
            user,
            "tests",
            &auth::crypto::secret_hash(&secret),
            scope,
            None,
            now(),
        )
        .unwrap();
    if let Some(s) = agent_scopes {
        // Stored as given: a token an earlier build made may hold a scope this one doesn't offer.
        auth.state().set_token_agent(&id, s, None, None).unwrap();
    }
    bearer(&format!("tt_{id}_{secret}"))
}

/// The running configuration from the files and `state.db`, as a reload builds it.
fn apply(src: &Sources) {
    let file = src.file_config.load_full();
    if let Some(a) = src.auth.get()
        && let Ok(c) = crate::managed::merge(&file, &crate::managed::entries(a.state()))
    {
        src.config.store(Arc::new(c));
    }
}

/// What the test's reload does when a write asks for one.
enum Reload {
    /// Applies the change (the files plus `state.db`), as the main loop does.
    Applies,
    /// Answers that it couldn't apply it.
    Fails,
    /// Says on `asked` that a reload was asked for, then waits for `go` before applying.
    Gated {
        asked: tokio::sync::mpsc::UnboundedSender<()>,
        go: Arc<tokio::sync::Notify>,
    },
}

/// One node with a state database and callers of every kind.
struct Node {
    src: Arc<Sources>,
    app: axum::Router,
    auth: Arc<Auth>,
    /// An admin's API token, and the same admin's signed-in session.
    admin: Creds,
    admin_session: Creds,
    operator: Creds,
    viewer: Creds,
    /// Agent tokens: an operator's with every write area, and an admin's with the scope an
    /// earlier build gave routers (and `cluster:admin`).
    operator_agent: Creds,
    admin_agent: Creds,
    dir: tempfile::TempDir,
}

impl Node {
    fn new(reload: Reload) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut s = crate::http::tests::sources_in(dir.path());
        let mut file = telltale_config::Config::default();
        file.node.data_dir =
            telltale_config::SafeString::new(dir.path().to_str().unwrap()).unwrap();
        s.file_config = arc_swap::ArcSwap::from_pointee(file.clone());
        s.config = arc_swap::ArcSwap::from_pointee(file);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<bool>>(4);
        s.reload = tx;
        let state =
            Arc::new(telltale_store::state::State::open(&dir.path().join("state.db")).unwrap());
        let auth = Arc::new(Auth::new(state, auth::Settings::default()));
        let src = Arc::new(s);
        let _ = src.auth.set(Arc::clone(&auth));
        let applier = Arc::clone(&src);
        tokio::spawn(async move {
            while let Some(done) = rx.recv().await {
                match &reload {
                    Reload::Applies => {
                        apply(&applier);
                        let _ = done.send(true);
                    }
                    Reload::Fails => {
                        let _ = done.send(false);
                    }
                    Reload::Gated { asked, go } => {
                        let _ = asked.send(());
                        go.notified().await;
                        apply(&applier);
                        let _ = done.send(true);
                    }
                }
            }
        });
        let root = auth
            .create_user("root", "correct horse battery", Role::Admin, false, now())
            .unwrap();
        let olga = auth
            .create_user("olga", "an operator password", Role::Operator, false, now())
            .unwrap();
        let vic = auth
            .create_user("vic", "a viewer's password", Role::Viewer, false, now())
            .unwrap();
        let session = auth
            .start_session(&root, "127.0.0.1".parse().unwrap(), now())
            .unwrap();
        let admin_session = vec![
            (
                "cookie",
                format!("{}={}", telltale_api::auth::routes::COOKIE, session.cookie),
            ),
            ("x-csrf-token", session.csrf),
        ];
        let app = telltale_api::router(
            Arc::new(ApiBackend {
                src: Arc::clone(&src),
            }),
            Arc::clone(&auth),
        );
        let every_write = "config:write:clients config:write:records config:write:forwards config:write:rules config:write:upstreams config:write:lists config:write:alerts config:write:groups config:read";
        Self {
            admin: token(&auth, root.id, "admin", None),
            operator: token(&auth, olga.id, "write", None),
            viewer: token(&auth, vic.id, "read", None),
            operator_agent: token(&auth, olga.id, "write", Some(every_write)),
            admin_agent: token(
                &auth,
                root.id,
                "write",
                Some("config:write:routers cluster:admin config:read"),
            ),
            admin_session,
            src,
            app,
            auth,
            dir,
        }
    }

    fn request(
        method: &str,
        uri: &str,
        creds: &Creds,
        body: Option<&serde_json::Value>,
    ) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            // Agents say why (AGT-005); people may.
            .header("x-telltale-reason", "router tests");
        for (k, v) in creds {
            req = req.header(*k, v.as_str());
        }
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        req.body(body).unwrap()
    }

    async fn send(
        &self,
        method: &str,
        uri: &str,
        creds: &Creds,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let resp = self
            .app
            .clone()
            .oneshot(Self::request(method, uri, creds, body.as_ref()))
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    async fn put(
        &self,
        name: &str,
        creds: &Creds,
        dry: bool,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let uri = format!(
            "/api/v1/routers/{name}{}",
            if dry { "?dryRun=true" } else { "" }
        );
        self.send("PUT", &uri, creds, Some(body)).await
    }

    /// The stored API entry for `name`.
    fn stored(&self, name: &str) -> Option<telltale_config::RouterConfig> {
        crate::managed::entries(self.auth.state())
            .routers
            .into_iter()
            .find_map(|(n, o)| match o {
                crate::managed::Ovr::Set(r) if n == name => Some(r),
                _ => None,
            })
    }

    /// Stores a router as an earlier build, a backup or a hand edit would have.
    fn store(&self, name: &str, body: &serde_json::Value) {
        self.auth
            .state()
            .put_managed(
                crate::managed::ROUTER,
                name,
                &body.to_string(),
                None,
                None,
                now(),
                "an older version",
            )
            .unwrap();
    }

    /// The routers in `GET /api/v1/config/entries?kind=router`, as `creds` sees them.
    async fn routers(&self, creds: &Creds) -> Vec<serde_json::Value> {
        let (s, v) = self
            .send("GET", "/api/v1/config/entries?kind=router", creds, None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["items"].as_array().cloned().unwrap_or_default()
    }

    /// The key file the stored entry for `name` names.
    fn key_file(&self, name: &str) -> Option<String> {
        self.stored(name)
            .and_then(|r| r.api_key_file.map(|f| f.to_string()))
    }

    fn data(&self) -> String {
        self.dir.path().to_str().unwrap().to_owned()
    }

    /// Every file under `<data_dir>/secrets/` but the API's own list.
    fn secret_files(&self) -> Vec<String> {
        let dir = self.dir.path().join("secrets");
        let mut v: Vec<String> = std::fs::read_dir(&dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.starts_with(".written"))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }
}

/// A UniFi router with a pasted API key at `url`.
fn unifi(url: &str, key: &str) -> serde_json::Value {
    serde_json::json!({ "type": "unifi", "url": url, "api_key": key, "offline_days": 0 })
}

fn marker(tag: &str) -> String {
    format!("{tag}-{}", auth::crypto::random_id())
}

/// Loopback addresses nothing listens on, for tests that don't poll a router.
const HERE: &str = "http://127.0.0.1:1";
const ELSEWHERE: &str = "http://127.0.0.2:1";

/// Waits (at most `secs`) until `f` holds.
async fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---- T1: who may write a router.

/// REQ: API-002, T8.2 (ADR-181, as ADR-179) — a router is added, changed or removed only by an
/// administrator's own sign-in or an admin API token: an operator, a viewer, an operator's agent
/// token with every write area, and an admin's agent token holding the scope an earlier build
/// gave routers are all refused, dry run or not, PUT or DELETE, and nothing is stored or written.
#[tokio::test]
async fn api_002_only_an_admin_writes_a_router() {
    let n = Node::new(Reload::Applies);
    let body = unifi("https://192.0.2.1", "a-key");
    for (who, creds) in [
        ("operator", &n.operator),
        ("viewer", &n.viewer),
        ("operator's agent", &n.operator_agent),
        ("admin's agent", &n.admin_agent),
    ] {
        for dry in [true, false] {
            let (s, v) = n.put("udm", creds, dry, body.clone()).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{who} dry={dry}: {v}");
        }
        let (s, v) = n.send("DELETE", "/api/v1/routers/udm", creds, None).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{who} DELETE: {v}");
    }
    assert!(n.stored("udm").is_none(), "nothing was stored");
    assert!(n.secret_files().is_empty(), "{:?}", n.secret_files());
    // An administrator's session and token may.
    let (s, v) = n.put("udm", &n.admin_session, true, body.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = n.put("udm", &n.admin, false, body).await;
    assert_eq!(
        (s, v["applied"].as_bool()),
        (StatusCode::OK, Some(true)),
        "{v}"
    );
    // An admin's agent token can't change one that is there, even without a key in the body,
    // nor remove it.
    let (s, v) = n
        .put(
            "udm",
            &n.admin_agent,
            false,
            serde_json::json!({"type": "unifi", "url": "https://192.0.2.1", "offline_days": 0, "interval_secs": 60}),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, v) = n
        .send("DELETE", "/api/v1/routers/udm", &n.admin_agent, None)
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(n.stored("udm").is_some());
    let (s, v) = n
        .send("DELETE", "/api/v1/routers/udm", &n.admin_session, None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// REQ: API-002, T8.2 (ADR-181, as ADR-179) — a router write forwarded from another node is
/// planned on this node (the primary) with the same rule: without the entry node's word that an
/// administrator made it (a write from a node that doesn't say is not one), it is refused, dry run
/// or not, PUT or DELETE, and nothing is stored or written; with it, it is planned and applied.
#[tokio::test]
async fn api_002_a_forwarded_router_write_needs_an_admin() {
    let n = Node::new(Reload::Applies);
    let local: telltale_api::Shared = Arc::new(ApiBackend {
        src: Arc::clone(&n.src),
    });
    let write = |by_admin: bool, dry_run: bool, body: Option<serde_json::Value>| {
        crate::forward::Write::Managed {
            kind: ManagedKind::Router,
            name: "udm".into(),
            body,
            dry_run,
            expect: None,
            by: "olga".into(),
            simulate: None,
            by_admin,
        }
    };
    for dry_run in [false, true] {
        for body in [Some(unifi("https://192.0.2.1", "a-key")), None] {
            let a = crate::forward::apply(
                &n.src,
                &local,
                "node-b",
                "site-b",
                write(false, dry_run, body),
            )
            .await;
            let a = serde_json::to_value(a).unwrap();
            assert_eq!(a["err"]["code"], "forbidden", "dry_run={dry_run}: {a}");
        }
    }
    assert!(n.stored("udm").is_none(), "nothing was stored");
    assert!(n.secret_files().is_empty(), "{:?}", n.secret_files());
    let a = crate::forward::apply(
        &n.src,
        &local,
        "node-b",
        "site-b",
        write(true, true, Some(unifi("https://192.0.2.1", "a-key"))),
    )
    .await;
    let a = serde_json::to_value(a).unwrap();
    assert_eq!(a["ok"]["applied"], false, "planned as a dry run: {a}");
    let a = crate::forward::apply(
        &n.src,
        &local,
        "node-b",
        "site-b",
        write(true, false, Some(unifi("https://192.0.2.1", "a-key"))),
    )
    .await;
    let a = serde_json::to_value(a).unwrap();
    assert_eq!(a["ok"]["applied"], true, "{a}");
}

// ---- T2: a body carries a key, never a path.

/// REQ: API-002, T8.2 (ADR-181) — F2: a router can't name another router's key file (here with
/// another address) and so have the node send that key there. Refused with 422 and a hint to
/// paste the key, dry run or not; nothing is stored.
#[tokio::test]
async fn api_002_router_cannot_name_another_routers_key() {
    let n = Node::new(Reload::Applies);
    let (s, v) = n
        .put("udm", &n.admin, false, unifi(HERE, &marker("udm-key")))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let udm_file = n.key_file("udm").unwrap();
    for dry in [true, false] {
        let (s, v) = n
            .put(
                "evil",
                &n.admin,
                dry,
                serde_json::json!({"type": "unifi", "url": ELSEWHERE, "api_key_file": udm_file, "offline_days": 0}),
            )
            .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "dry={dry}: {v}");
        assert!(
            v["hint"].as_str().unwrap_or("").contains("Paste the key"),
            "{v}"
        );
    }
    assert!(n.stored("evil").is_none(), "nothing was stored");
}

/// REQ: API-002, T8.2 (ADR-181) — every key-file field in an API body is refused unless it is the
/// file the router already has and its endpoint is unchanged: `..`, a path outside the data
/// directory, a relative path, a link inside `secrets/`, a file someone put in `secrets/`, and
/// another router's file, for each of `api_key_file`, `api_secret_file` and `password_file`; and
/// the router's own file with a new address. Keeping the router's own file at the same address,
/// or leaving it out, is fine.
#[tokio::test]
async fn api_002_router_credential_paths_are_refused() {
    let n = Node::new(Reload::Applies);
    let d = n.data();
    let (s, v) = n
        .put("udm", &n.admin, false, unifi(HERE, &marker("udm-key")))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = n
        .put("nas", &n.admin, false, unifi(HERE, &marker("nas-key")))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let udm_file = n.key_file("udm").unwrap();
    let other_routers_file = n.key_file("nas").unwrap();
    std::fs::write(format!("{d}/secrets/mine"), "someone's file").unwrap();
    let mut paths = vec![
        format!("{d}/secrets/../state.db"),
        "/etc/passwd".to_owned(),
        "secrets/mine".to_owned(),
        format!("{d}/secrets/mine"),
        other_routers_file.clone(),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&other_routers_file, format!("{d}/secrets/link")).unwrap();
        paths.push(format!("{d}/secrets/link"));
    }
    let bodies = |p: &str| {
        [
            serde_json::json!({"type": "unifi", "url": HERE, "api_key_file": p, "offline_days": 0}),
            serde_json::json!({"type": "opnsense", "url": HERE, "api_key": "k", "api_secret_file": p}),
            serde_json::json!({"type": "unifi", "url": HERE, "username": "telltale", "password_file": p, "offline_days": 0}),
        ]
    };
    for p in &paths {
        for b in bodies(p) {
            for name in ["udm", "fresh"] {
                let (s, v) = n.put(name, &n.admin, true, b.clone()).await;
                assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{name} {b}: {v}");
                assert!(
                    v["hint"].as_str().unwrap_or("").contains("Paste the key"),
                    "{name} {b}: {v}"
                );
            }
        }
    }
    // The router's own file, at a new address.
    let (s, v) = n
        .put(
            "udm",
            &n.admin,
            true,
            serde_json::json!({"type": "unifi", "url": ELSEWHERE, "api_key_file": udm_file, "offline_days": 0}),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    // Its own file at the same address, or none named: kept.
    for b in [
        serde_json::json!({"type": "unifi", "url": HERE, "api_key_file": udm_file, "offline_days": 0, "interval_secs": 60}),
        serde_json::json!({"type": "unifi", "url": HERE, "offline_days": 0, "interval_secs": 90}),
    ] {
        let (s, v) = n.put("udm", &n.admin, false, b.clone()).await;
        assert_eq!(s, StatusCode::OK, "{b}: {v}");
        assert_eq!(n.key_file("udm").as_deref(), Some(udm_file.as_str()));
    }
    assert!(n.stored("fresh").is_none());
}

// ---- T7 (a): a pasted key goes over https.

/// REQ: API-002, T8.2 (ADR-181) — a key pasted through the API goes to a router reached over
/// `https://` or a loopback address; `http://` to anything else (`localhost` included: a name can
/// be made to resolve elsewhere) is refused with 422 and a hint. Dry runs only: nothing is polled.
#[tokio::test]
async fn api_002_a_pasted_key_goes_only_over_https() {
    let n = Node::new(Reload::Applies);
    for url in [
        "http://192.168.1.1",
        "http://unifi.example.test:8443",
        "http://localhost:8080",
    ] {
        let (s, v) = n.put("udm", &n.admin, true, unifi(url, "k")).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{url}: {v}");
        assert!(
            v["detail"].as_str().unwrap_or("").contains("https://"),
            "{url}: {v}"
        );
    }
    for url in [
        "https://192.0.2.1",
        "https://unifi.example.test:8443",
        "http://127.0.0.1:1",
        "http://127.0.0.2:1",
        "http://[::1]:1",
    ] {
        let (s, v) = n.put("udm", &n.admin, true, unifi(url, "k")).await;
        assert_eq!(s, StatusCode::OK, "{url}: {v}");
    }
}

// ---- T4: a saved key is kept.

/// REQ: API-002, T8.2 (ADR-181) — F3: once the entry naming a new key file is stored, the file is
/// kept even when the reload then fails: the stored entry never names a file that was removed.
#[tokio::test]
async fn api_002_router_key_kept_when_saved_but_not_applied() {
    let n = Node::new(Reload::Fails);
    let (s, v) = n.put("udm", &n.admin, false, unifi(HERE, "the-key")).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{v}");
    assert!(
        v["detail"]
            .as_str()
            .unwrap_or("")
            .contains("saved but couldn't be applied"),
        "{v}"
    );
    let f = n.key_file("udm").expect("the entry is stored");
    assert_eq!(
        std::fs::read_to_string(&f).ok().as_deref().map(str::trim),
        Some("the-key"),
        "the file the stored entry names is there: {f}"
    );
}

/// REQ: API-002, T8.2 (ADR-181) — F3: a client that goes away after the change is stored doesn't
/// undo it half way: the key file stays, and the change is still applied.
#[tokio::test]
async fn api_002_router_key_kept_when_the_client_goes_away() {
    let (asked, mut reload_asked) = tokio::sync::mpsc::unbounded_channel();
    let go = Arc::new(tokio::sync::Notify::new());
    let n = Node::new(Reload::Gated {
        asked,
        go: Arc::clone(&go),
    });
    let req = Node::request(
        "PUT",
        "/api/v1/routers/udm",
        &n.admin,
        Some(&unifi(HERE, "the-key")),
    );
    let app = n.app.clone();
    let client = tokio::spawn(async move { app.oneshot(req).await });
    // The change is stored and its reload asked for: the client goes away.
    reload_asked.recv().await.unwrap();
    client.abort();
    let _ = client.await;
    let f = n.key_file("udm").expect("the entry is stored");
    assert!(Path::new(&f).exists(), "the key stays: {f}");
    go.notify_one();
    wait_until("the change applied", 10, || {
        n.src
            .config
            .load()
            .router
            .iter()
            .any(|r| r.name.as_str() == "udm")
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(&f).ok().as_deref().map(str::trim),
        Some("the-key")
    );
}

/// A UniFi console on loopback. It answers `stat/sta` for any site with one device, and keeps,
/// for every request, the site and the API key it came with.
struct Console {
    url: String,
    asked: Arc<Mutex<Vec<(String, String)>>>,
}

impl Console {
    fn sites(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| s.clone())
            .collect()
    }

    fn key_for(&self, site: &str) -> Option<String> {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .find(|(s, _)| s == site)
            .map(|(_, k)| k.clone())
    }
}

fn console() -> Console {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let asked = Arc::new(Mutex::new(Vec::new()));
    let keep = Arc::clone(&asked);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut c) = conn else { continue };
            let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                match c.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => buf.extend_from_slice(&chunk[..k]),
                }
            }
            let head = String::from_utf8_lossy(&buf).to_string();
            let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
            let key = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("x-api-key")
                        .then(|| v.trim().to_owned())
                })
                .unwrap_or_default();
            let site = path
                .strip_prefix("/proxy/network/api/s/")
                .and_then(|r| r.strip_suffix("/stat/sta"))
                .unwrap_or("")
                .to_owned();
            keep.lock().unwrap().push((site.clone(), key));
            let body = serde_json::json!({"meta": {"rc": "ok"}, "data": [
                {"mac": "02:00:00:00:07:01", "ip": "192.168.7.20", "name": format!("{site} device")}
            ]})
            .to_string();
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    Console { url, asked }
}

/// The router task, polling the routers in effect until the returned sender says stop.
fn poll(
    n: &Node,
) -> (
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
) {
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let view = Arc::new(arc_swap::ArcSwap::from_pointee(
        crate::devices::Leases::new(),
    ));
    let task = tokio::spawn(crate::routers::run(
        Arc::clone(&n.src),
        view,
        stopped,
        false,
    ));
    (stop, task)
}

// ---- T3: what a node polls with.

/// REQ: API-002, T8.2 (ADR-181) — at use time, a router in effect that names a key file is polled
/// only if this node's own config files define it with that file for the same endpoint, or the
/// file is one this node wrote for that router (`.written`) under its `secrets/`. A stored entry
/// that names another router's key (with another site), or a file outside `secrets/`, isn't
/// polled; the files' router, a stored override of it that keeps its file and endpoint, and a
/// router with its own pasted key are.
#[tokio::test]
async fn api_002_a_stored_router_naming_another_routers_key_is_not_polled() {
    let n = Node::new(Reload::Applies);
    let d = n.data();
    let fake = console();
    let udm_key = marker("udm-key");
    let (s, v) = n
        .put("udm", &n.admin, false, unifi(&fake.url, &udm_key))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let udm_file = n.key_file("udm").unwrap();
    std::fs::create_dir_all(format!("{d}/outside")).unwrap();
    for (f, k) in [
        ("key", "outside-key"),
        ("key2", "file-key"),
        ("key3", "override-key"),
    ] {
        std::fs::write(format!("{d}/outside/{f}"), k).unwrap();
    }
    // The files' routers.
    let mut file = (*n.src.file_config.load_full()).clone();
    for (name, site, key) in [
        ("c-file", "file", "key2"),
        ("d-override", "override", "key3"),
    ] {
        file.router.push(
            serde_json::from_value(serde_json::json!({
                "name": name, "type": "unifi", "url": fake.url, "site": site,
                "api_key_file": format!("{d}/outside/{key}"), "offline_days": 0,
            }))
            .unwrap(),
        );
    }
    n.src.file_config.store(Arc::new(file));
    // An override of a files' router that keeps its file and endpoint, and entries stored as an
    // earlier build, a backup or a hand edit could have.
    n.store(
        "d-override",
        &serde_json::json!({"name": "d-override", "type": "unifi", "url": fake.url, "site": "override",
            "api_key_file": format!("{d}/outside/key3"), "offline_days": 0, "interval_secs": 60}),
    );
    n.store(
        "a-evil",
        &serde_json::json!({"name": "a-evil", "type": "unifi", "url": fake.url, "site": "evil",
            "api_key_file": udm_file, "offline_days": 0}),
    );
    n.store(
        "b-outside",
        &serde_json::json!({"name": "b-outside", "type": "unifi", "url": fake.url, "site": "outside",
            "api_key_file": format!("{d}/outside/key"), "offline_days": 0}),
    );
    apply(&n.src);
    let order: Vec<String> = n
        .src
        .config
        .load()
        .router
        .iter()
        .map(|r| r.name.to_string())
        .collect();
    assert_eq!(
        order,
        ["c-file", "a-evil", "b-outside", "d-override", "udm"],
        "the held ones come before the last one polled"
    );
    let (stop, task) = poll(&n);
    wait_until("udm, the files' router and the override polled", 20, || {
        let s = fake.sites();
        ["default", "file", "override"]
            .iter()
            .all(|w| s.iter().any(|x| x == w))
    })
    .await;
    let _ = stop.send(true);
    let _ = task.await;
    let sites = fake.sites();
    assert!(
        !sites.iter().any(|s| s == "evil" || s == "outside"),
        "the held routers weren't polled: {:?}",
        fake.asked.lock().unwrap()
    );
    assert_eq!(fake.key_for("default").as_deref(), Some(udm_key.as_str()));
    assert_eq!(fake.key_for("file").as_deref(), Some("file-key"));
    assert_eq!(fake.key_for("override").as_deref(), Some("override-key"));
    // Through the API too: an administrator's override of the files' router that keeps its file
    // (outside secrets/) and its endpoint is accepted, since it is what the node uses already.
    let (s, v) = n
        .put(
            "c-file",
            &n.admin,
            true,
            serde_json::json!({"type": "unifi", "url": fake.url, "site": "file",
                "api_key_file": format!("{d}/outside/key2"), "offline_days": 0, "interval_secs": 60}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

// ---- Decision 1: the status needs no secret to read.

/// REQ: API-002, T8.2 (ADR-181) — a router's credentials have a status anyone who may read the
/// configuration sees, read without the secret: `saved`, `missing` once the file is gone (after a
/// restore elsewhere, or a failover: enter it again), and `not_used` for a stored entry naming a
/// file this node won't send for it. Entering the key again makes it `saved`.
#[tokio::test]
async fn api_002_a_router_whose_key_file_is_gone_says_key_missing() {
    let n = Node::new(Reload::Applies);
    let (s, v) = n.put("udm", &n.admin, false, unifi(HERE, "k1")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let status = |items: &[serde_json::Value], name: &str| {
        items
            .iter()
            .find(|e| e["name"] == name)
            .map(|e| e["credentials"]["api_key"].clone())
            .unwrap_or_default()
    };
    for creds in [&n.admin, &n.viewer] {
        assert_eq!(status(&n.routers(creds).await, "udm"), "saved");
    }
    let f = n.key_file("udm").unwrap();
    std::fs::remove_file(&f).unwrap();
    for creds in [&n.admin, &n.viewer] {
        assert_eq!(status(&n.routers(creds).await, "udm"), "missing");
    }
    let (s, v) = n.put("udm", &n.admin, false, unifi(HERE, "k2")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    n.store(
        "a-evil",
        &serde_json::json!({"name": "a-evil", "type": "unifi", "url": HERE, "site": "evil",
            "api_key_file": n.key_file("udm").unwrap(), "offline_days": 0}),
    );
    apply(&n.src);
    let items = n.routers(&n.viewer).await;
    assert_eq!(status(&items, "udm"), "saved");
    assert_eq!(status(&items, "a-evil"), "not_used");
}

// ---- T5: the sweep.

/// REQ: API-002, T8.2 (ADR-181) — F5: each node removes the key files it wrote that nothing names
/// any more: at start (an entry gone without the API: a crash before the prune, a restored
/// `state.db`), and after a change (a replica whose primary now names another file). Never a file
/// the API didn't write, and never one outside `<data_dir>/secrets/`.
#[tokio::test]
async fn api_002_orphaned_router_key_files_removed() {
    let n = Node::new(Reload::Applies);
    let d = n.data();
    let fake = console();
    for (name, key) in [("udm", "k1"), ("tmp", "k2")] {
        let (s, v) = n.put(name, &n.admin, false, unifi(&fake.url, key)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    let (udm_file, tmp_file) = (n.key_file("udm").unwrap(), n.key_file("tmp").unwrap());
    // `tmp` goes without the API: its key file is an orphan.
    n.auth
        .state()
        .delete_managed(crate::managed::ROUTER, "tmp", None)
        .unwrap();
    apply(&n.src);
    // A file the API didn't write, and a record (as this version keeps them) for a file outside
    // secrets/.
    std::fs::write(format!("{d}/secrets/mine"), "keep").unwrap();
    std::fs::write(format!("{d}/outside-file"), "keep").unwrap();
    let mut written = std::fs::OpenOptions::new()
        .append(true)
        .open(format!("{d}/secrets/.written"))
        .unwrap();
    writeln!(
        written,
        "{}",
        serde_json::json!({"router": "tmp", "field": "api_key", "file": format!("{d}/outside-file")})
    )
    .unwrap();
    drop(written);
    let (stop, task) = poll(&n);
    wait_until("the orphan removed at start", 20, || {
        !Path::new(&tmp_file).exists()
    })
    .await;
    assert!(Path::new(&udm_file).exists(), "udm's key is named");
    // The routers in effect stop naming udm's file (a replica, after the key was entered again on
    // the primary), and this node's own stored entry doesn't name it either.
    let mut cfg = (*n.src.config.load_full()).clone();
    if let Some(r) = cfg.router.iter_mut().find(|r| r.name.as_str() == "udm") {
        r.api_key_file =
            Some(telltale_config::SafeString::new(format!("{d}/secrets/elsewhere")).unwrap());
    }
    n.auth
        .state()
        .delete_managed(crate::managed::ROUTER, "udm", None)
        .unwrap();
    n.src.config.store(Arc::new(cfg));
    wait_until("udm's old key removed after the change", 20, || {
        !Path::new(&udm_file).exists()
    })
    .await;
    let _ = stop.send(true);
    let _ = task.await;
    assert!(Path::new(&format!("{d}/secrets/mine")).exists());
    assert!(Path::new(&format!("{d}/outside-file")).exists());
}

/// REQ: API-002, T8.2 (ADR-181) — the sweep keeps a file the API wrote while this node's own stored
/// entry still names it, though the configuration in effect doesn't (a reload that fell back to
/// the config files, a pinned version, a primary that was demoted and may be promoted again).
#[tokio::test]
async fn api_002_a_key_its_own_stored_entry_names_is_kept() {
    let n = Node::new(Reload::Applies);
    let fake = console();
    for (name, key) in [("udm", "k1"), ("tmp", "k2")] {
        let (s, v) = n.put(name, &n.admin, false, unifi(&fake.url, key)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    let (udm_file, tmp_file) = (n.key_file("udm").unwrap(), n.key_file("tmp").unwrap());
    // The configuration in effect is the files' alone; `tmp` is gone from state.db.
    n.auth
        .state()
        .delete_managed(crate::managed::ROUTER, "tmp", None)
        .unwrap();
    n.src.config.store(n.src.file_config.load_full());
    let (stop, task) = poll(&n);
    wait_until("the orphan removed", 20, || !Path::new(&tmp_file).exists()).await;
    let _ = stop.send(true);
    let _ = task.await;
    assert!(Path::new(&udm_file).exists(), "named by the stored entry");
}
