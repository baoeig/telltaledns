// REQ: T8.2, API-002 (ADR-181) — routers made through the API and the UI, with keys pasted and
// saved as files on the node that took the write, driven through the real routes, sign-in and
// backend. The router consoles here are loopback fakes; nothing leaves the machine.

use std::sync::Arc;

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

/// Loopback addresses nothing listens on (no test here polls a router).
const HERE: &str = "http://127.0.0.1:1";
const ELSEWHERE: &str = "http://127.0.0.2:1";

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
