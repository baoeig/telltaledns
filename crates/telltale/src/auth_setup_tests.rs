// REQ: moved out of `auth_setup.rs` so the test code is a file of its own (CodeQL ignores test paths).
use super::*;

fn cfg(dir: &Path) -> Config {
    let mut c = Config::default();
    c.node.data_dir = telltale_config::SafeString::new(dir.to_string_lossy()).unwrap();
    c
}

// REQ: API-003 — the setup token survives restarts until the first admin exists.
#[test]
fn api_003_setup_token_is_kept_private_and_reused_across_restarts() {
    let dir = std::env::temp_dir().join(format!("tt-auth-{}", crypto::random_id()));
    let c = cfg(&dir);
    let a = open(&c).unwrap();
    let t1 = a.setup_token().unwrap().unwrap();
    let file = dir.join(SETUP_TOKEN_FILE);
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&file).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o600);
    drop(a);
    let a = open(&c).unwrap();
    assert_eq!(
        a.setup_token().unwrap().as_deref(),
        Some(t1.as_str()),
        "same token after a restart"
    );
    a.setup(&t1, "root", "correct horse battery", now())
        .unwrap();
    assert!(!file.exists());
    drop(a);
    let a = open(&c).unwrap();
    assert!(!a.setup_required().unwrap());
    assert!(!file.exists(), "no new token once set up");
    let _ = std::fs::remove_dir_all(&dir);
}

/// What a test subscriber saw: each event's level and message, in order.
#[derive(Clone, Default)]
struct Seen(Arc<std::sync::Mutex<Vec<(tracing::Level, String)>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Seen {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut m = Message(String::new());
        event.record(&mut m);
        if let Ok(mut v) = self.0.lock() {
            v.push((*event.metadata().level(), m.0));
        }
    }
}

/// Runs `open(c)` and returns the lines about password sign-in that it logged (the start-up
/// notice is the only one that says "password sign-in"), plus whether it logged that the
/// provider could not be set up.
fn start_up(c: &Config) -> (Vec<(tracing::Level, String)>, bool) {
    use tracing_subscriber::layer::SubscriberExt as _;
    let seen = Seen::default();
    let sub = tracing_subscriber::registry().with(seen.clone());
    let auth = tracing::subscriber::with_default(sub, || open(c).unwrap());
    drop(auth);
    let all = seen.0.lock().unwrap().clone();
    let provider_failed = all.iter().any(|(_, l)| l.contains("OIDC sign-in disabled"));
    let notices = all
        .into_iter()
        .filter(|(_, l)| l.contains("password sign-in"))
        .collect();
    (notices, provider_failed)
}

fn cidrs(list: &[&str]) -> Vec<Cidr> {
    list.iter().map(|s| Cidr::parse(s).unwrap()).collect()
}

/// A provider that sets up (nothing is contacted until someone signs in), or one whose secret
/// file is missing.
fn provider(secret_file: Option<&str>) -> telltale_config::OidcProvider {
    let s = |v: &str| telltale_config::SafeString::new(v).unwrap();
    telltale_config::OidcProvider {
        id: s("kc"),
        name: s("Keycloak"),
        issuer: s("https://auth.example.com/realms/home"),
        client_id: s("telltale"),
        client_secret: secret_file.is_none().then(|| s("s3cret")),
        client_secret_file: secret_file.map(s),
        scopes: vec![s("openid")],
        username_claim: s("preferred_username"),
        groups_claim: s("groups"),
        role: Vec::new(),
        default_role: None,
        require_verified_email: false,
    }
}

/// A config in `dir` with local sign-in off and these break-glass networks, and a provider.
fn break_glass(dir: &Path, networks: Vec<Cidr>, secret_file: Option<&str>) -> Config {
    let mut c = cfg(dir);
    c.auth.oidc.disable_local_login = true;
    c.auth.oidc.allowed_admin_networks = networks;
    c.auth.oidc.provider = vec![provider(secret_file)];
    c
}

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("tt-auth-{}", crypto::random_id()))
}

// REQ: API-004 (issue #63) — with the flag on and a list that reaches beyond this machine,
// password sign-in is still open to those networks, so start-up warns (it used to be an INFO
// line that read "password sign-in is off"), and says which networks and that it is the default.
#[test]
fn api_004_start_up_warns_when_break_glass_reaches_beyond_loopback() {
    let default = Config::default().auth.oidc.allowed_admin_networks;
    for (what, networks) in [
        ("the default list", default.clone()),
        (
            "loopback and one LAN host",
            cidrs(&["127.0.0.0/8", "192.168.1.5/32"]),
        ),
        ("every address", cidrs(&["0.0.0.0/0"])),
    ] {
        let dir = temp_dir();
        let (lines, _) = start_up(&break_glass(&dir, networks.clone(), None));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(lines.len(), 1, "{what}: {lines:?}");
        let (level, line) = &lines[0];
        assert_eq!(*level, tracing::Level::WARN, "{what}: {line}");
        for n in &networks {
            assert!(line.contains(&n.to_string()), "{what}: {line} names {n}");
        }
        assert!(line.contains("still open"), "{what}: {line}");
        assert!(line.contains("allowed_admin_networks"), "{what}: {line}");
        assert_eq!(
            line.contains("default"),
            networks == default,
            "{what}: only the default list is called that: {line}"
        );
        assert!(
            line.contains("[]"),
            "{what}: says how to turn it off: {line}"
        );
    }
}

// REQ: API-004 (issue #63) — a list that stays on this machine is only information, and the line
// says so; behind a proxy on this host it can't know, so it points at `trusted_proxies`.
#[test]
fn api_004_start_up_is_quiet_about_a_loopback_only_list() {
    for networks in [
        cidrs(&["127.0.0.0/8", "::1/128"]),
        cidrs(&["127.0.0.1/32"]),
        cidrs(&["::1/128"]),
    ] {
        let dir = temp_dir();
        let (lines, _) = start_up(&break_glass(&dir, networks.clone(), None));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let (level, line) = &lines[0];
        assert_eq!(*level, tracing::Level::INFO, "{networks:?}: {line}");
        assert!(line.contains("restricted to loopback"), "{line}");
        assert!(
            line.contains("trusted_proxies"),
            "a proxy on this host would make every browser look like loopback: {line}"
        );
    }
    // With the proxy listed there is nothing to add.
    let dir = temp_dir();
    let mut c = break_glass(&dir, cidrs(&["127.0.0.0/8"]), None);
    c.api.trusted_proxies = cidrs(&["127.0.0.1/32"]);
    let (lines, _) = start_up(&c);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].0, tracing::Level::INFO);
    assert!(lines[0].1.contains("restricted to loopback"), "{lines:?}");
    assert!(!lines[0].1.contains("trusted_proxies"), "{lines:?}");
}

// REQ: API-004 (issue #63) — a provider that can't be set up (a missing secret file) is exactly
// when break-glass matters, so the line is logged anyway, and the gate is in force all the same.
#[test]
fn api_004_start_up_says_it_even_when_the_provider_fails() {
    let dir = temp_dir();
    let c = break_glass(
        &dir,
        Config::default().auth.oidc.allowed_admin_networks,
        Some("/nonexistent/tt-oidc-secret"),
    );
    let (lines, provider_failed) = start_up(&c);
    assert!(provider_failed, "the provider did fail to set up");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].0, tracing::Level::WARN, "{lines:?}");
    assert!(lines[0].1.contains("still open"), "{lines:?}");
    // The gate does not depend on the provider: a public address is still refused.
    let auth = open(&c).unwrap();
    assert!(!auth.local_login_allowed("203.0.113.9".parse().unwrap()));
    assert!(auth.local_login_allowed("192.168.1.5".parse().unwrap()));
    drop(auth);
    let _ = std::fs::remove_dir_all(&dir);
}

// REQ: API-004 (issue #63, ADR-034) — pins the default: with `disable_local_login` on and the list
// left alone, the sign-in form is still offered to a client in a private network or on loopback
// (`/auth/status` says `localLogin` from this same check), and not to the Internet. Settings come
// from the default `Config` through `settings()`, so a change to the default list shows here.
#[test]
fn api_004_default_list_leaves_the_form_on_in_a_private_network() {
    let dir = temp_dir();
    let mut c = cfg(&dir);
    c.auth.oidc.disable_local_login = true;
    let auth = open(&c).unwrap();
    for ip in [
        "10.1.2.3",
        "172.17.0.1",
        "192.168.1.50",
        "100.64.0.9",
        "169.254.1.1",
        "127.0.0.1",
        "::1",
        "fd00::5",
        "fe80::1",
        "::ffff:192.168.1.5",
    ] {
        assert!(auth.local_login_allowed(ip.parse().unwrap()), "{ip}");
    }
    for ip in [
        "8.8.8.8",
        "203.0.113.9",
        "172.32.0.1",
        "100.128.0.1",
        "2001:db8::1",
    ] {
        assert!(!auth.local_login_allowed(ip.parse().unwrap()), "{ip}");
    }
    drop(auth);
    let _ = std::fs::remove_dir_all(&dir);
}

// REQ: API-004 (issue #63) — an empty list keeps its warning, whoever the provider is, and with
// the flag off there is nothing to say. (These two already held; they pin behaviour the change
// to the start-up line must not lose.)
#[test]
fn api_004_start_up_notice_follows_the_flag_and_the_list() {
    let dir = temp_dir();
    let (lines, _) = start_up(&break_glass(&dir, Vec::new(), None));
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].0, tracing::Level::WARN);
    assert!(
        lines[0].1.contains("nobody can sign in with a password"),
        "{lines:?}"
    );

    let mut off = break_glass(&dir, Vec::new(), None);
    off.auth.oidc.disable_local_login = false;
    let (lines, _) = start_up(&off);
    assert!(lines.is_empty(), "flag off: {lines:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// REQ: API-004 (issue #63), CLU-009 (ADR-120) — a Helm resolver pod has no way to sign in (its API
// has no Service port), so whatever the list, it gets information, not a warning per pod start.
#[test]
fn api_004_start_up_does_not_warn_a_resolver_pod() {
    for networks in [
        Config::default().auth.oidc.allowed_admin_networks,
        Vec::new(),
    ] {
        let dir = temp_dir();
        let mut pod = break_glass(&dir, networks.clone(), None);
        pod.cluster.ephemeral = true;
        let (lines, _) = start_up(&pod);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(lines.len(), 1, "{networks:?}: {lines:?}");
        assert_eq!(lines[0].0, tracing::Level::INFO, "{networks:?}: {lines:?}");
    }
}

// REQ: API-004 (issue #63) — the notice names every network and says when it is the default list;
// only a list wholly inside loopback counts as "this machine" (anything wider, an IPv4-mapped
// form, or a /7 around 127.0.0.0 is not).
#[test]
fn api_004_start_up_says_where_password_sign_in_is_still_open() {
    let default = Config::default().auth.oidc.allowed_admin_networks;
    assert!(
        !default.is_empty(),
        "the default break-glass list is not empty"
    );
    let (warning, line) = break_glass_notice(&default, &[]);
    assert!(warning, "the default list reaches beyond this machine");
    for n in &default {
        assert!(line.contains(&n.to_string()), "{line} should name {n}");
    }
    assert!(
        line.contains("the default allowed_admin_networks"),
        "{line}"
    );
    let (warning, line) = break_glass_notice(&[], &[]);
    assert!(warning, "no break-glass network at all is a warning");
    assert!(line.contains("nobody can sign in with a password"));
    for (list, only_this_machine) in [
        ("127.0.0.1/32", true),
        ("127.0.0.0/8, ::1/128", true),
        ("127.1.2.3/32", true),
        ("127.0.0.0/7", false),
        ("0.0.0.0/0", false),
        ("::1/64", false),
        ("::/0", false),
        ("::ffff:127.0.0.1/128", false),
        ("127.0.0.0/8, 10.0.0.0/8", false),
        ("192.168.1.10/32", false),
    ] {
        let nets = cidrs(&list.split(", ").collect::<Vec<_>>());
        let (warning, line) = break_glass_notice(&nets, &[]);
        assert_eq!(!warning, only_this_machine, "{list}: {line}");
    }
}
