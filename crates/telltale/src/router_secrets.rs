//! REQ: T8.2, API-002 (ADR-181) — the credentials of a router set through the API or the UI.
//!
//! A router body may carry `api_key`, `api_secret`, or `password`. Each pasted value is written
//! to a new file under `<data_dir>/secrets/` (0600, in a 0700 directory, created exclusively,
//! so nothing is ever overwritten) and only that file's path is stored, listed, audited, or
//! returned. The rules, from the reviews:
//! - **Paste only.** A body never names a key file: the poll sends a file's content to the
//!   router's address, so a named file would let a writer send another router's key, or any
//!   file the process can read, to an address of their choosing. The one exception is the file
//!   the router already has, at the same endpoint, sent back unchanged (the UI does). A file
//!   elsewhere (a Docker or Kubernetes secret) is set in `telltale.toml`.
//! - **A new address needs the key again.** When the type, address, site, or TLS settings
//!   change, a credential that would be reused (the same file as now) is refused: the key
//!   was entered for the old endpoint and isn't sent to a new one without being typed again.
//! - **Over https.** A pasted key goes to a router reached over `https://`, or at a loopback
//!   address.
//! - **Written as new files, switched to after the change applies.** A failed or conflicting
//!   change removes what it wrote; the key in use is never replaced in place.
//! - **Only what the API wrote is removed.** `secrets/.written` lists the files the API
//!   created; a router's old files are removed once nothing in effect names them, and only if
//!   they are on that list.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use telltale_api::problem::{Code, Problem};
use telltale_api::{ManagedKind, ManagedWrite};
use telltale_config::RouterConfig;

use crate::http::Sources;

/// The fields of a router body that carry a secret, and the `[[router]]` field naming the file
/// each is written to.
pub(crate) const FIELDS: [(&str, &str); 3] = [
    ("api_key", "api_key_file"),
    ("api_secret", "api_secret_file"),
    ("password", "password_file"),
];

/// The list of files the API wrote, in the secrets directory.
const WRITTEN: &str = ".written";

/// Where pasted secrets are written: `<data_dir>/secrets/`.
pub(crate) fn secrets_dir(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("secrets")
}

/// A router's credential files, as configured.
fn files_of(r: &RouterConfig) -> impl Iterator<Item = PathBuf> + '_ {
    [&r.api_key_file, &r.api_secret_file, &r.password_file]
        .into_iter()
        .flatten()
        .map(|f| PathBuf::from(f.as_str()))
}

/// The configured file for one credential field.
fn file_of<'a>(r: &'a RouterConfig, file_field: &str) -> Option<&'a str> {
    match file_field {
        "api_key_file" => r.api_key_file.as_ref(),
        "api_secret_file" => r.api_secret_file.as_ref(),
        _ => r.password_file.as_ref(),
    }
    .map(telltale_config::SafeString::as_str)
}

/// A new file name for a pasted secret: `secrets/router-<name>.<field>.<random>` (the name made
/// safe for a file name, with a hash when that changed it).
fn new_secret_file(data_dir: &str, name: &str, field: &str) -> PathBuf {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let slug = if slug == name {
        slug
    } else {
        let hash = blake3::hash(name.as_bytes()).to_hex();
        format!("{slug}-{}", hash.as_str().get(..8).unwrap_or_default())
    };
    let mut r = [0u8; 8];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut r);
    let tag = r.iter().fold(String::new(), |mut t, b| {
        let _ = std::fmt::Write::write_fmt(&mut t, format_args!("{b:02x}"));
        t
    });
    secrets_dir(data_dir).join(format!("router-{slug}.{field}.{tag}"))
}

/// The hint of every refusal that asks for the key: the body carries the key, never a path.
const PASTE: &str = "Paste the key; the node saves it. A key file elsewhere (a Docker or Kubernetes secret) is named in telltale.toml.";

/// REQ: API-002, T8.2 (ADR-181) — F2: a body named a key file other than the router's own.
fn paste_only(field: &str, file_field: &str) -> Problem {
    Problem::new(
        Code::InvalidConfig,
        format!(
            "`{file_field}`: a router set through the UI or the API names no key file but the one it has; paste the {field} instead"
        ),
    )
    .hint(PASTE)
}

/// REQ: API-002, T8.2 (ADR-181) — a pasted key is sent only over `https://`, or to a loopback
/// address: over `http://` it can be read on the way, and anyone who can change where the name
/// resolves (a local name, on a node whose own resolver is TelltaleDNS) can choose where it
/// goes. `localhost` counts as a name. A URL that is neither is left to the validation.
fn https_only(o: &serde_json::Map<String, serde_json::Value>) -> Result<(), Problem> {
    let url = o
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match url.strip_prefix("http://") {
        Some(rest) if !loopback(rest) => Err(Problem::new(
            Code::InvalidConfig,
            "`url`: a pasted key is sent only over https:// (or to a loopback address); over http:// it could be read or redirected on the way",
        )
        .hint("Use the router's https:// address. A console with a self-signed certificate: save it as tls_ca, with tls_server_name (see the routers section of the guide).")),
        _ => Ok(()),
    }
}

/// Whether the authority at the start of `rest` (what follows `http://`) is a loopback address,
/// written as one (`127.0.0.1`, `[::1]`, with or without a port).
fn loopback(rest: &str) -> bool {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Whether the body sends credentials somewhere other than `current` does: another type,
/// address, site, or TLS setting.
fn endpoint_changed(
    current: &RouterConfig,
    o: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    let s = |k: &str| {
        o.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let cur = |v: &Option<telltale_config::SafeString>| v.as_ref().map(ToString::to_string);
    let kind = serde_json::to_value(current.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned));
    s("type") != kind
        || s("url").as_deref() != Some(current.url.as_str())
        || s("site") != cur(&current.site)
        || s("tls_ca") != cur(&current.tls_ca)
        || s("tls_server_name") != cur(&current.tls_server_name)
        || o.get("tls_insecure_skip_verify")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            != current.tls_insecure_skip_verify
}

/// A router write's credentials, prepared: the body already names the files.
#[derive(Debug, Default)]
pub(crate) struct Prepared {
    data_dir: String,
    /// New files and what goes in them.
    writes: Vec<(PathBuf, String)>,
    /// The router's files before the change (removed after it, if the API wrote them and
    /// nothing in effect names them).
    old: Vec<PathBuf>,
}

/// REQ: T8.2, API-002 (ADR-181) — prepares a router write (nothing for other kinds): each pasted
/// secret is swapped for a new file's path in the body, a key file the body names is refused
/// unless it's the router's own at the same endpoint, and a credential the body leaves out keeps
/// the current file unless the endpoint changed. "The router's own" is the router as the write
/// is planned against: the config files plus the API's entries (`state`), as `plan_override`
/// sees them. Blocking (it reads `state.db`).
pub(crate) fn prepare(
    src: &Sources,
    state: &telltale_store::state::State,
    w: &mut ManagedWrite,
) -> Result<Prepared, Problem> {
    if w.kind != ManagedKind::Router {
        return Ok(Prepared::default());
    }
    let file = src.file_config.load_full();
    let before = crate::managed::merge(&file, &crate::managed::entries(state))
        .unwrap_or_else(|_| (*file).clone());
    let data_dir = file.node.data_dir.to_string();
    let name = w.name.trim().to_owned();
    let current = before.router.iter().find(|r| r.name.as_str() == name);
    let old = current.map(|c| files_of(c).collect()).unwrap_or_default();
    let writes = match w.body.as_mut() {
        Some(b) => prepare_body(&data_dir, &name, current, b)?,
        None => Vec::new(),
    };
    Ok(Prepared {
        data_dir,
        writes,
        old,
    })
}

/// The body half of [`prepare`]: returns the files to write.
pub(crate) fn prepare_body(
    data_dir: &str,
    name: &str,
    current: Option<&RouterConfig>,
    body: &mut serde_json::Value,
) -> Result<Vec<(PathBuf, String)>, Problem> {
    use serde_json::Value;
    let Some(o) = body.as_object_mut() else {
        return Ok(Vec::new());
    };
    let moved = current.is_some_and(|c| endpoint_changed(c, o));
    let mut out = Vec::new();
    for (field, file_field) in FIELDS {
        let secret = match o.remove(field) {
            Some(Value::String(v)) if !v.trim().is_empty() => Some(v.trim().to_owned()),
            Some(Value::String(_) | Value::Null) | None => None,
            Some(_) => {
                return Err(Problem::new(
                    Code::InvalidConfig,
                    format!("`{field}` must be a string"),
                ));
            }
        };
        let now = current.and_then(|c| file_of(c, file_field));
        // REQ: API-002 (ADR-181) — F2, paste only: the one file a body may name is the router's
        // own, sent back unchanged (it is kept, or replaced by a pasted value below).
        match o.get(file_field) {
            None | Some(Value::Null) => {}
            Some(Value::String(v)) if v.trim().is_empty() || Some(v.as_str()) == now => {}
            Some(_) => return Err(paste_only(field, file_field)),
        }
        let file = match secret {
            // A pasted value: a new file, never one that exists (created exclusively).
            Some(v) => {
                let f = new_secret_file(data_dir, name, field);
                let path = f.to_string_lossy().into_owned();
                out.push((f, v));
                Some(path)
            }
            // ADR-181 (review) — a key isn't sent to an endpoint it wasn't entered for.
            None if moved && now.is_some() => {
                return Err(Problem::new(
                    Code::InvalidConfig,
                    format!(
                        "`{field}`: enter it again: the type, address, site, or TLS settings changed, and the saved {field} isn't sent to a new endpoint"
                    ),
                )
                .hint(PASTE));
            }
            None => now.map(str::to_owned),
        };
        match file {
            Some(f) => {
                o.insert(file_field.into(), Value::String(f));
            }
            None => {
                o.remove(file_field);
            }
        }
    }
    if !out.is_empty() {
        https_only(o)?;
    }
    Ok(out)
}

impl Prepared {
    /// Writes the new files (never on a dry run). They're removed again when the returned
    /// guard is dropped without [`Written::keep`]: the change failed or conflicted.
    pub(crate) fn write(&self) -> Result<Written, Problem> {
        let mut done = Written {
            data_dir: self.data_dir.clone(),
            files: Vec::new(),
            kept: false,
        };
        for (path, value) in &self.writes {
            write_new(path, value).map_err(|e| {
                Problem::internal(format!(
                    "{}: couldn't write the secret: {e}",
                    path.display()
                ))
            })?;
            done.files.push(path.clone());
            note_written(&self.data_dir, path, true);
        }
        Ok(done)
    }

    /// After the change applied: the router's old files the API wrote, that nothing in effect
    /// names any more, are removed (best effort: a file left behind is only untidy).
    pub(crate) fn prune(&self, src: &Sources) {
        if self.old.is_empty() {
            return;
        }
        let cfg = src.config.load();
        let written = read_written(&self.data_dir);
        for f in &self.old {
            let used = cfg.router.iter().any(|r| files_of(r).any(|x| &x == f));
            if !used && written.contains(f) {
                let _ = std::fs::remove_file(f);
                note_written(&self.data_dir, f, false);
            }
        }
    }
}

/// The files a write created, removed on drop unless kept.
pub(crate) struct Written {
    data_dir: String,
    files: Vec<PathBuf>,
    kept: bool,
}

impl Written {
    pub(crate) fn keep(mut self) {
        self.kept = true;
    }
}

impl Drop for Written {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        for f in &self.files {
            let _ = std::fs::remove_file(f);
            note_written(&self.data_dir, f, false);
        }
    }
}

/// Creates `path` (it must not exist) with `value`, readable only by this user, in a 0700
/// directory. A partial file is removed.
fn write_new(path: &Path, value: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        o.mode(0o600);
    }
    let mut f = o.open(path)?;
    let r = f.write_all(value.as_bytes()).and_then(|()| f.sync_all());
    if r.is_err() {
        let _ = std::fs::remove_file(path);
    }
    r
}

/// The files the API wrote (`secrets/.written`, one path a line).
fn read_written(data_dir: &str) -> Vec<PathBuf> {
    std::fs::read_to_string(secrets_dir(data_dir).join(WRITTEN))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Adds `path` to, or removes it from, the list of files the API wrote. Configuration writes
/// run one at a time, so this needs no lock of its own.
fn note_written(data_dir: &str, path: &Path, add: bool) {
    let mut list = read_written(data_dir);
    list.retain(|p| p != path);
    if add {
        list.push(path.to_path_buf());
    }
    let text = list.iter().fold(String::new(), |mut t, p| {
        t.push_str(&p.to_string_lossy());
        t.push('\n');
        t
    });
    let file = secrets_dir(data_dir).join(WRITTEN);
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router(v: serde_json::Value) -> RouterConfig {
        serde_json::from_value(v).unwrap()
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("telltale-secrets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// REQ: API-002, T8.2 (ADR-181) — a pasted key goes to a new file under secrets/, with a
    /// fresh name; the router's own file sent back unchanged, or left out, keeps it (same
    /// endpoint); a pasted key replaces it with a new file.
    #[test]
    fn api_002_pasted_router_secrets_become_new_files() {
        let mut b =
            serde_json::json!({"type": "unifi", "url": "https://192.168.1.1", "api_key": " k1 "});
        let w = prepare_body("/data", "udm", None, &mut b).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].1, "k1");
        let f = w[0].0.to_string_lossy().into_owned();
        assert!(f.starts_with("/data/secrets/router-udm.api_key."), "{f}");
        assert_eq!(b["api_key_file"], f.as_str());
        assert!(b.get("api_key").is_none());

        let mut b = serde_json::json!({"type": "opnsense", "url": "https://fw", "api_key": "k",
            "api_secret": "s"});
        let w = prepare_body("/data", "fw", None, &mut b).unwrap();
        assert_eq!(w.len(), 2);
        assert!(
            b["api_secret_file"]
                .as_str()
                .unwrap()
                .starts_with("/data/secrets/router-fw.api_secret."),
            "{b}"
        );

        // Left out, or sent back unchanged: the current file stays (same endpoint).
        let current = router(serde_json::json!({"name": "udm", "type": "unifi",
            "url": "https://192.168.1.1", "api_key_file": "/data/secrets/router-udm.api_key.1"}));
        for mut b in [
            serde_json::json!({"type": "unifi", "url": "https://192.168.1.1", "api_key": "", "interval_secs": 60}),
            serde_json::json!({"type": "unifi", "url": "https://192.168.1.1", "interval_secs": 60,
                "api_key_file": "/data/secrets/router-udm.api_key.1"}),
        ] {
            assert_eq!(
                prepare_body("/data", "udm", Some(&current), &mut b).unwrap(),
                Vec::new()
            );
            assert_eq!(b["api_key_file"], "/data/secrets/router-udm.api_key.1");
        }
        // A pasted key with the own file sent back: a new file.
        let mut b = serde_json::json!({"type": "unifi", "url": "https://192.168.1.1", "api_key": "k2",
            "api_key_file": "/data/secrets/router-udm.api_key.1"});
        let w = prepare_body("/data", "udm", Some(&current), &mut b).unwrap();
        assert_eq!(w.len(), 1);
        assert_ne!(b["api_key_file"], "/data/secrets/router-udm.api_key.1");
    }

    /// REQ: API-002, T8.2 (ADR-181) — F2: a body names no key file but the router's own: any
    /// other path (absolute, relative, `..`, the folder itself, another file in it) is refused
    /// with a hint to paste the key, pasted value or not; so is a non-string value, and reusing
    /// a saved key for a new endpoint. A pasted key goes only over https:// or to a loopback
    /// address.
    #[test]
    fn api_002_router_bodies_name_no_key_file() {
        let current = router(serde_json::json!({"name": "udm", "type": "unifi",
            "url": "https://192.168.1.1", "api_key_file": "/data/secrets/k"}));
        for (bad, named) in [
            (serde_json::json!({"api_key_file": "/etc/passwd"}), true),
            (serde_json::json!({"password_file": "/etc/shadow"}), true),
            (
                serde_json::json!({"api_key": "k", "api_key_file": "secrets/../telltale.toml"}),
                true,
            ),
            (serde_json::json!({"api_key_file": "secrets"}), true),
            (
                serde_json::json!({"api_key_file": "/data/secrets/other"}),
                true,
            ),
            (
                serde_json::json!({"api_secret_file": "/data/secrets/k"}),
                true,
            ),
            (serde_json::json!({"api_key_file": 5}), true),
            (serde_json::json!({"api_key": 5}), false),
        ] {
            for cur in [None, Some(&current)] {
                let mut b = bad.clone();
                b["type"] = "unifi".into();
                b["url"] = "https://192.168.1.1".into();
                let e = prepare_body("/data", "udm", cur, &mut b).unwrap_err();
                if named {
                    assert!(
                        e.hint.as_deref().unwrap_or("").contains("Paste the key"),
                        "{bad}: {e:?}"
                    );
                }
            }
        }
        for url in [
            "http://192.168.1.1",
            "HTTP://unifi.lan:8443/x",
            "http://localhost",
            "http://a@127.0.0.1",
        ] {
            let mut b = serde_json::json!({"type": "unifi", "url": url, "api_key": "k"});
            let e = prepare_body("/data", "udm", None, &mut b).unwrap_err();
            assert!(e.detail.contains("https://"), "{url}: {e:?}");
        }
        for url in [
            "https://192.168.1.1",
            "http://127.0.0.1:8443",
            "http://127.9.9.9/",
            "http://[::1]:80",
        ] {
            let mut b = serde_json::json!({"type": "unifi", "url": url, "api_key": "k"});
            assert!(prepare_body("/data", "udm", None, &mut b).is_ok(), "{url}");
        }
        // Without a pasted key, the scheme is the validation's (a config-file router is unchanged).
        let mut b =
            serde_json::json!({"type": "unifi", "url": "http://192.168.1.1", "username": "u"});
        assert!(prepare_body("/data", "udm", None, &mut b).is_ok());
        // A new endpoint with the saved key: enter it again.
        for moved in [
            serde_json::json!({"type": "unifi", "url": "https://203.0.113.9"}),
            serde_json::json!({"type": "unifi", "url": "https://203.0.113.9", "api_key_file": "/data/secrets/k"}),
            serde_json::json!({"type": "unifi", "url": "https://192.168.1.1", "tls_insecure_skip_verify": true}),
            serde_json::json!({"type": "opnsense", "url": "https://192.168.1.1"}),
        ] {
            let mut b = moved;
            let e = prepare_body("/data", "udm", Some(&current), &mut b).unwrap_err();
            assert!(e.detail.contains("enter it again"), "{}", e.detail);
        }
        // With the key entered again, it's fine.
        let mut b =
            serde_json::json!({"type": "unifi", "url": "https://203.0.113.9", "api_key": "k2"});
        assert_eq!(
            prepare_body("/data", "udm", Some(&current), &mut b)
                .unwrap()
                .len(),
            1
        );
    }

    /// REQ: T8.2 (ADR-181) — new files are private and never overwrite one; a failed change
    /// removes what it wrote; only files the API wrote are pruned.
    #[cfg(unix)]
    #[test]
    fn t8_2_secret_files_private_and_undone() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = tmp("files");
        let dd = d.to_string_lossy().into_owned();
        let f = secrets_dir(&dd).join("router-udm.api_key.1");
        let p = Prepared {
            data_dir: dd.clone(),
            writes: vec![(f.clone(), "k1".into())],
            old: Vec::new(),
        };
        // Dropped without keep: undone.
        drop(p.write().unwrap());
        assert!(!f.exists());
        assert_eq!(read_written(&dd), Vec::<PathBuf>::new());
        // Kept.
        p.write().unwrap().keep();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "k1");
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(secrets_dir(&dd))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(read_written(&dd), vec![f.clone()]);
        // Never overwritten.
        assert!(p.write().is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "k1");
        let _ = std::fs::remove_dir_all(&d);
    }
}
