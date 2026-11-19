// REQ: OBS-010, API-002 (ADR-179) — credential files (`token_file`, `password_file`) on alert
// destinations made through the API, driven through the real routes, sign-in and backend: only an
// administrator sets or changes a destination that names one.

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

/// One node with a state database, an admin, an operator, and agent tokens.
struct Node {
    app: axum::Router,
    auth: Arc<Auth>,
    /// Bearer tokens.
    admin: String,
    operator: String,
    /// Agent tokens with `config:write:alerts`: an operator's, and an admin's.
    agent: String,
    admin_agent: String,
    /// The data directory lives as long as the node.
    _dir: tempfile::TempDir,
}

fn token(auth: &Auth, user: i64, scope: &str, agent_scopes: Option<&str>) -> String {
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
        auth.state().set_token_agent(&id, s, None, None).unwrap();
    }
    format!("tt_{id}_{secret}")
}

/// A node whose data directory is a fresh temporary directory. Applying a change rebuilds the
/// running configuration from the files and `state.db`, as the main loop does.
fn node() -> Node {
    let dir = tempfile::tempdir().unwrap();
    let mut s = crate::http::tests::sources_in(dir.path());
    let mut file = telltale_config::Config::default();
    file.node.data_dir = telltale_config::SafeString::new(dir.path().to_str().unwrap()).unwrap();
    s.file_config = arc_swap::ArcSwap::from_pointee(file.clone());
    s.config = arc_swap::ArcSwap::from_pointee(file);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<bool>>(4);
    s.reload = tx;
    let state = Arc::new(telltale_store::state::State::open(&dir.path().join("state.db")).unwrap());
    let auth = Arc::new(Auth::new(state, auth::Settings::default()));
    let src = Arc::new(s);
    let _ = src.auth.set(Arc::clone(&auth));
    let applier = Arc::clone(&src);
    tokio::spawn(async move {
        while let Some(done) = rx.recv().await {
            apply(&applier);
            let _ = done.send(true);
        }
    });
    let root = auth
        .create_user("root", "correct horse battery", Role::Admin, false, now())
        .unwrap();
    let olga = auth
        .create_user("olga", "an operator password", Role::Operator, false, now())
        .unwrap();
    let admin = token(&auth, root.id, "admin", None);
    let operator = token(&auth, olga.id, "write", None);
    let agent = token(&auth, olga.id, "write", Some("config:write:alerts"));
    let admin_agent = token(&auth, root.id, "write", Some("config:write:alerts"));
    let app = telltale_api::router(
        Arc::new(ApiBackend {
            src: Arc::clone(&src),
        }),
        Arc::clone(&auth),
    );
    Node {
        app,
        auth,
        admin,
        operator,
        agent,
        admin_agent,
        _dir: dir,
    }
}

/// The running configuration from the files and `state.db`.
fn apply(src: &Sources) {
    let file = src.file_config.load_full();
    if let Some(a) = src.auth.get()
        && let Ok(c) = crate::managed::merge(&file, &crate::managed::entries(a.state()))
    {
        src.config.store(Arc::new(c));
    }
}

impl Node {
    async fn send(
        &self,
        method: &str,
        uri: &str,
        bearer: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"))
            // Agents say why (AGT-005); people may.
            .header("x-telltale-reason", "credential file tests");
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let resp = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
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
        bearer: &str,
        dry: bool,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let uri = format!(
            "/api/v1/alerts/destinations/{name}{}",
            if dry { "?dryRun=true" } else { "" }
        );
        self.send("PUT", &uri, bearer, Some(body)).await
    }
}

fn ntfy(token_file: &str) -> serde_json::Value {
    serde_json::json!({ "type": "ntfy", "url": "https://ntfy.sh/home-dns", "token_file": token_file })
}

fn email(password_file: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "email", "url": "smtp://smtp.example.com:587", "from": "dns@example.com",
        "to": ["me@example.com"], "username": "dns@example.com", "password_file": password_file,
    })
}

/// REQ: OBS-010, API-002 (ADR-179) — a destination that names a credential file is set by an
/// administrator: an operator, an operator's agent token, and an admin's agent token are refused
/// (dry runs too), with a hint; an admin's own sign-in or token is not. A destination without one
/// is anyone's with the operator role.
#[tokio::test]
async fn obs_010_only_an_admin_sets_a_credential_file() {
    let n = node();
    for (who, bearer) in [
        ("operator", &n.operator),
        ("operator's agent", &n.agent),
        ("admin's agent", &n.admin_agent),
    ] {
        for (field, body) in [
            ("token_file", ntfy("/run/secrets/ntfy-token")),
            ("password_file", email("/run/secrets/smtp-password")),
        ] {
            for dry in [false, true] {
                let (s, v) = n.put("phone", bearer, dry, body.clone()).await;
                assert_eq!(s, StatusCode::FORBIDDEN, "{who} {field} dry={dry}: {v}");
                assert_eq!(v["code"], "forbidden", "{v}");
                assert!(
                    v["detail"].as_str().unwrap_or("").contains(field),
                    "{who}: the refusal names the field: {v}"
                );
                assert!(
                    v["hint"]
                        .as_str()
                        .unwrap_or("")
                        .contains("needs an administrator"),
                    "{v}"
                );
            }
        }
    }
    assert!(
        crate::managed::entries(n.auth.state())
            .alert_destinations
            .is_empty(),
        "nothing was stored"
    );
    // Without a credential file, an operator and an operator's agent may.
    let plain = serde_json::json!({ "type": "webhook", "url": "https://hooks.example.com/dns" });
    for bearer in [&n.operator, &n.agent] {
        let (s, v) = n.put("hook", bearer, true, plain.clone()).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    let (s, v) = n.put("hook", &n.operator, false, plain).await;
    assert_eq!(
        (s, v["applied"].as_bool()),
        (StatusCode::OK, Some(true)),
        "{v}"
    );
    // An admin may, dry run or not.
    let (s, v) = n
        .put("phone", &n.admin, true, ntfy("/run/secrets/ntfy-token"))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = n
        .put("phone", &n.admin, false, ntfy("/run/secrets/ntfy-token"))
        .await;
    assert_eq!(
        (s, v["applied"].as_bool()),
        (StatusCode::OK, Some(true)),
        "{v}"
    );
    let (s, v) = n
        .put(
            "inbox",
            &n.admin,
            false,
            email("/run/secrets/smtp-password"),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// REQ: OBS-010, API-002 (ADR-179) — a destination that already names a credential file is an
/// administrator's to change, whatever the change (its address, its account, or leaving the file
/// out); an operator may still delete it, and keeps every destination without one.
#[tokio::test]
async fn obs_010_only_an_admin_changes_a_destination_with_a_credential_file() {
    let n = node();
    for (name, body) in [
        ("phone", ntfy("/run/secrets/ntfy-token")),
        ("inbox", email("/run/secrets/smtp-password")),
    ] {
        let (s, v) = n.put(name, &n.admin, false, body).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    let (s, _) = n
        .put(
            "hook",
            &n.admin,
            false,
            serde_json::json!({ "type": "webhook", "url": "https://hooks.example.com/a" }),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let mut moved = ntfy("/run/secrets/ntfy-token");
    moved["url"] = "https://ntfy.example.net/elsewhere".into();
    let mut no_file = ntfy("/run/secrets/ntfy-token");
    no_file.as_object_mut().unwrap().remove("token_file");
    no_file["url"] = "https://ntfy.example.net/elsewhere".into();
    let mut account = email("/run/secrets/smtp-password");
    account["username"] = "someone@example.com".into();
    for (bearer, who) in [(&n.operator, "operator"), (&n.agent, "agent")] {
        for (name, body, what) in [
            ("phone", moved.clone(), "a new address"),
            ("phone", no_file.clone(), "the file left out"),
            ("inbox", account.clone(), "a new account"),
        ] {
            for dry in [true, false] {
                let (s, v) = n.put(name, bearer, dry, body.clone()).await;
                assert_eq!(s, StatusCode::FORBIDDEN, "{who}, {what}, dry={dry}: {v}");
                assert!(
                    v["hint"]
                        .as_str()
                        .unwrap_or("")
                        .contains("needs an administrator"),
                    "{v}"
                );
            }
        }
    }
    // Nothing moved.
    let stored = crate::managed::entries(n.auth.state()).alert_destinations;
    let phone = stored
        .iter()
        .find_map(|(k, o)| match o {
            crate::managed::Ovr::Set(d) if k == "phone" => Some(d.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(phone.url.as_str(), "https://ntfy.sh/home-dns");
    // A destination without a credential file is an operator's to change.
    let (s, v) = n
        .put(
            "hook",
            &n.operator,
            false,
            serde_json::json!({ "type": "webhook", "url": "https://hooks.example.com/b" }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Deleting only takes something away: an operator may.
    for name in ["phone", "inbox"] {
        let (s, v) = n
            .send(
                "DELETE",
                &format!("/api/v1/alerts/destinations/{name}"),
                &n.operator,
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{name}: {v}");
    }
    // The admin changes a destination with a credential file as before.
    let (s, v) = n
        .put("phone", &n.admin, false, ntfy("/run/secrets/ntfy-token"))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = n.put("phone", &n.admin, false, moved).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}
