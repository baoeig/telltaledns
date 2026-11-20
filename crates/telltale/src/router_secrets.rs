//! REQ: T8.2, API-002 (ADR-181) — the credentials of a router set through the API or the UI.
//!
//! A router body may carry `api_key`, `api_secret`, or `password`. Each pasted value is written
//! to a new file under `<data_dir>/secrets/` (0600, in a 0700 directory, created exclusively and
//! synced with its directory, so nothing is ever overwritten) and only that file's path is
//! stored, listed to administrators, audited, or replicated. The rules, from the reviews:
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
//! - **Used for the router it was saved for.** `secrets/.written` records each file the API
//!   wrote: for which router, which credential, and the endpoint the key was entered for. A node
//!   sends a key file for a router in effect only if its own config files define that router
//!   with that file at the same endpoint, or the file is recorded there for that router,
//!   credential and endpoint and lies in its `secrets/` ([`UseCheck`]). Otherwise the router isn't
//!   polled here and its status says why: a stored entry naming another router's key, a line an
//!   earlier build wrote, a restored or hand-edited `state.db`, a replica's copy of a router whose
//!   key is on another node.
//! - **Written as new files, kept once stored.** A change that fails or conflicts before its
//!   entry is stored removes what it wrote; the key in use is never replaced in place.
//! - **Only what the API wrote is removed, on each node.** A file recorded in this node's
//!   `.written` and in its `secrets/` is removed once no router names it: not in effect, not in
//!   this node's config files, not in its own stored entries. Each node sweeps at start and after
//!   every change of the routers in effect ([`sweep`]), so a key rotated, deleted, replaced on
//!   another node, or left by a crash doesn't stay behind.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use telltale_api::problem::{Code, Problem};
use telltale_api::{ManagedKind, ManagedWrite};
use telltale_config::RouterConfig;
use tracing::{info, warn};

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

/// A router's credential files, as configured: (credential, its file field, the file).
fn files_of(r: &RouterConfig) -> impl Iterator<Item = (&'static str, &'static str, &str)> + '_ {
    FIELDS
        .into_iter()
        .filter_map(|(field, file_field)| file_of(r, file_field).map(|f| (field, file_field, f)))
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
/// safe for a file name, with a hash when that changed or shortened it). REQ: T8.2 (ADR-181) —
/// F9: a random source that fails is an error, not a name of zeros.
fn new_secret_file(data_dir: &str, name: &str, field: &str) -> Result<PathBuf, Problem> {
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
    let slug = if slug == name && slug.len() <= 64 {
        slug
    } else {
        let hash = blake3::hash(name.as_bytes()).to_hex();
        let short: String = slug.chars().take(48).collect();
        format!("{short}-{}", hash.as_str().get(..8).unwrap_or_default())
    };
    let mut r = [0u8; 8];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut r).map_err(|_| {
        Problem::internal("couldn't name the key file: the system's random source failed")
    })?;
    let tag = r.iter().fold(String::new(), |mut t, b| {
        let _ = std::fmt::Write::write_fmt(&mut t, format_args!("{b:02x}"));
        t
    });
    Ok(secrets_dir(data_dir).join(format!("router-{slug}.{field}.{tag}")))
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

/// REQ: T8.2, API-002 (ADR-181) — where a router's keys are sent: its type, address, site and
/// TLS settings. A key is used only for the endpoint it was entered for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Endpoint {
    #[serde(rename = "type")]
    kind: String,
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tls_ca: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tls_server_name: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    tls_insecure_skip_verify: bool,
}

/// A setting as an endpoint compares it: trimmed, and empty is none.
fn setting(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

impl Endpoint {
    fn of(r: &RouterConfig) -> Self {
        Self {
            kind: serde_json::to_value(r.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            url: r.url.trim().to_owned(),
            site: setting(r.site.as_deref()),
            tls_ca: setting(r.tls_ca.as_deref()),
            tls_server_name: setting(r.tls_server_name.as_deref()),
            tls_insecure_skip_verify: r.tls_insecure_skip_verify,
        }
    }

    /// The endpoint a router body says, compared the same way.
    fn of_body(o: &serde_json::Map<String, serde_json::Value>) -> Self {
        let s = |k: &str| setting(o.get(k).and_then(serde_json::Value::as_str));
        Self {
            kind: s("type").unwrap_or_default(),
            url: s("url").unwrap_or_default(),
            site: s("site"),
            tls_ca: s("tls_ca"),
            tls_server_name: s("tls_server_name"),
            tls_insecure_skip_verify: o
                .get("tls_insecure_skip_verify")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        }
    }
}

/// Whether the body sends credentials somewhere other than `current` does: another type,
/// address, site, or TLS setting.
fn endpoint_changed(
    current: &RouterConfig,
    o: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    Endpoint::of(current) != Endpoint::of_body(o)
}

/// A pasted value and the new file it goes to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NewKey {
    /// The credential (`api_key`, `api_secret`, `password`).
    field: &'static str,
    file: PathBuf,
    value: String,
}

/// A router write's credentials, prepared: the body already names the files.
#[derive(Debug, Default)]
pub(crate) struct Prepared {
    data_dir: String,
    /// New files and what goes in them.
    writes: Vec<NewKey>,
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
    let old = current
        .map(|c| files_of(c).map(|(_, _, f)| PathBuf::from(f)).collect())
        .unwrap_or_default();
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
) -> Result<Vec<NewKey>, Problem> {
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
            Some(value) => {
                let f = new_secret_file(data_dir, name, field)?;
                let path = f.to_string_lossy().into_owned();
                out.push(NewKey {
                    field,
                    file: f,
                    value,
                });
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
    /// Writes the new files for `router` (the router as planned; never on a dry run), each
    /// recorded in `.written` first, for that router, credential and endpoint. They're removed
    /// again, with their records, when the returned guard is dropped without [`Written::keep`]:
    /// the change failed or conflicted before its entry was stored. Blocking.
    pub(crate) fn write(&self, router: Option<&RouterConfig>) -> Result<Written, Problem> {
        let mut done = Written {
            data_dir: self.data_dir.clone(),
            recorded: Vec::new(),
            made: Vec::new(),
            kept: false,
        };
        if self.writes.is_empty() {
            return Ok(done);
        }
        let Some(r) = router else {
            return Err(Problem::internal(
                "the planned router is missing; no key was saved",
            ));
        };
        let dir = secrets_dir(&self.data_dir);
        private_dir(&dir).map_err(|e| {
            Problem::internal(format!(
                "{}: couldn't make the folder for the key: {e}",
                dir.display()
            ))
        })?;
        // Recorded first: a file made and then left behind by a crash is on the list, for the
        // sweep (ADR-181).
        let records: Vec<Record> = self
            .writes
            .iter()
            .map(|k| Record {
                router: Some(r.name.to_string()),
                field: Some(k.field.to_owned()),
                endpoint: Some(Endpoint::of(r)),
                file: k.file.clone(),
            })
            .collect();
        add_records(&self.data_dir, &records).map_err(|e| {
            Problem::internal(format!(
                "{}: couldn't record the new key file: {e}",
                dir.join(WRITTEN).display()
            ))
        })?;
        done.recorded = records.into_iter().map(|x| x.file).collect();
        for k in &self.writes {
            write_new(&k.file, &k.value).map_err(|e| {
                Problem::internal(format!("{}: couldn't write the key: {e}", k.file.display()))
            })?;
            done.made.push(k.file.clone());
        }
        Ok(done)
    }

    /// After the change applied, when the router had key files: those of them, and any other
    /// file the API wrote here, that nothing names any more are removed with their records, by
    /// the sweep's rule ([`sweep_now`]; best effort: a file left behind is only untidy, and the
    /// next sweep looks again). Blocking; the write holds `config_writes`.
    pub(crate) fn prune(&self, src: &Sources) {
        if self.old.is_empty() {
            return;
        }
        match sweep_now(src) {
            Ok(gone) => log_removed(&gone),
            Err(e) => warn!(error = %e, "router keys: the old key files weren't removed"),
        }
    }
}

/// The files a write created, removed on drop unless kept.
pub(crate) struct Written {
    data_dir: String,
    /// Recorded in `.written` (un-recorded on drop).
    recorded: Vec<PathBuf>,
    /// Created by this write (removed on drop): never a file that was there before.
    made: Vec<PathBuf>,
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
        for f in &self.made {
            let _ = std::fs::remove_file(f);
        }
        if let Err(e) = remove_records(&self.data_dir, &self.recorded) {
            warn!(error = %e, "router keys: couldn't update the list of key files written");
        }
    }
}

/// Removes `f`; true when it's gone (or was already).
fn remove(f: &Path) -> bool {
    match std::fs::remove_file(f) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            warn!(file = %f.display(), error = %e, "router keys: couldn't remove a key file");
            false
        }
    }
}

/// REQ: T8.2 (ADR-181) — F9: `dir`, made with 0700 on every level this creates, and 0700 itself.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Makes what was just created or renamed in `dir` durable.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Creates `path` (it must not exist) with `value`, readable only by this user, synced with its
/// directory (F9). A partial file is removed.
fn write_new(path: &Path, value: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600);
    }
    let mut f = o.open(path)?;
    let r = f
        .write_all(value.as_bytes())
        .and_then(|()| f.sync_all())
        .and_then(|()| sync_dir(dir));
    if r.is_err() {
        let _ = std::fs::remove_file(path);
    }
    r
}

/// REQ: T8.2 (ADR-181) — one file the API wrote, a line of `secrets/.written` (JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The router it was written for, the credential, and the endpoint the key was entered for.
    /// All absent in a line an earlier build wrote (the path alone): such a file is never used
    /// for any router, only removed once nothing names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    router: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<Endpoint>,
    file: PathBuf,
}

/// What `secrets/.written` records (none when there's no list yet).
fn read_records(data_dir: &str) -> std::io::Result<Vec<Record>> {
    let text = match std::fs::read_to_string(secrets_dir(data_dir).join(WRITTEN)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|_| Record {
                router: None,
                field: None,
                endpoint: None,
                file: PathBuf::from(l),
            })
        })
        .collect())
}

/// REQ: T8.2 (ADR-181) — F9: replaces the list, durably (a private temporary file, synced,
/// renamed over it, the folder synced). Configuration writes, and the sweep, run one at a time
/// (`config_writes`), so this needs no lock of its own.
fn write_records(data_dir: &str, records: &[Record]) -> std::io::Result<()> {
    let dir = secrets_dir(data_dir);
    let mut text = String::new();
    for r in records {
        text.push_str(&serde_json::to_string(r).map_err(std::io::Error::other)?);
        text.push('\n');
    }
    let tmp = dir.join(".written.tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600);
    }
    let mut f = o.open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, dir.join(WRITTEN))?;
    sync_dir(&dir)
}

/// Adds `new` to the list. A file already on it is refused (a name that came up twice): its
/// record stays as it is, and nothing is added.
fn add_records(data_dir: &str, new: &[Record]) -> std::io::Result<()> {
    let mut list = read_records(data_dir)?;
    if new.iter().any(|n| list.iter().any(|x| x.file == n.file)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "a key file of that name is already recorded",
        ));
    }
    list.extend(new.iter().cloned());
    write_records(data_dir, &list)
}

/// Takes `files` off the list.
fn remove_records(data_dir: &str, files: &[PathBuf]) -> std::io::Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let mut list = read_records(data_dir)?;
    let before = list.len();
    list.retain(|x| !files.contains(&x.file));
    if list.len() == before {
        return Ok(());
    }
    write_records(data_dir, &list)
}

/// REQ: API-002, T8.2 (ADR-181) — F5: every key file a router names that this node keeps: in the
/// configuration in effect, in this node's config files, and in this node's own entries made
/// through the API (`state.db`, read as JSON so a row that no longer decodes still keeps its
/// files). The last two cover a configuration in effect that leaves the API's entries out for a
/// while (a reload that fell back to the config files, a pinned version) and a primary that was
/// demoted and may be promoted again (its stored entries apply again then). `None` when the
/// stored entries can't be read: nothing is removed then.
fn named_files(src: &Sources) -> Option<HashSet<PathBuf>> {
    let mut named = HashSet::new();
    let cfg = src.config.load();
    let file = src.file_config.load();
    for r in cfg.router.iter().chain(file.router.iter()) {
        named.extend(files_of(r).map(|(_, _, f)| PathBuf::from(f)));
    }
    let state = src.auth.get()?.state();
    for m in state.managed(crate::managed::ROUTER).ok()? {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&m.body) {
            for (_, file_field) in FIELDS {
                if let Some(f) = v.get(file_field).and_then(serde_json::Value::as_str) {
                    named.insert(PathBuf::from(f));
                }
            }
        }
    }
    Some(named)
}

/// REQ: API-002, T8.2 (ADR-181) — F5: removes the key files this node's API wrote (`.written`)
/// that nothing names any more ([`named_files`]), with their records: what a rotation, a delete,
/// or a crash between writing a file and storing its entry left behind, and, on a replica, a key
/// the primary replaced. Never a file the API didn't write, and never one outside
/// `<data_dir>/secrets/` (a record for one is left as it is). Returns what it removed. Blocking;
/// run with `config_writes` held, so it never sees a file a write is about to store an entry for.
pub(crate) fn sweep_now(src: &Sources) -> Result<Vec<Record>, String> {
    let data_dir = src.file_config.load().node.data_dir.to_string();
    let list = secrets_dir(&data_dir).join(WRITTEN);
    let records = read_records(&data_dir).map_err(|e| format!("{}: {e}", list.display()))?;
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let named = named_files(src).ok_or("the entries made through the API couldn't be read")?;
    let (mut keep, mut gone) = (Vec::new(), Vec::new());
    for r in records {
        let ours = crate::secret_path::in_data_secrets(&r.file.to_string_lossy(), &data_dir);
        if named.contains(&r.file) || !ours || !remove(&r.file) {
            keep.push(r);
        } else {
            gone.push(r);
        }
    }
    if !gone.is_empty() {
        write_records(&data_dir, &keep).map_err(|e| format!("{}: {e}", list.display()))?;
    }
    Ok(gone)
}

/// REQ: API-002, T8.2 (ADR-181) — F5: the sweep, as the router task runs it at start and after
/// each change of the routers in effect: one change at a time (`config_writes`), off the async
/// workers. Each removed file is logged at info, on every node.
pub(crate) async fn sweep(src: &Arc<Sources>) {
    let _one_at_a_time = src.config_writes.lock().await;
    let s = Arc::clone(src);
    match tokio::task::spawn_blocking(move || sweep_now(&s)).await {
        Ok(Ok(gone)) => log_removed(&gone),
        Ok(Err(e)) => {
            warn!(error = %e, "router keys: the sweep of key files no router names didn't run");
        }
        Err(e) => {
            warn!(error = %e, "router keys: the sweep of key files no router names failed");
        }
    }
}

fn log_removed(gone: &[Record]) {
    for r in gone {
        info!(
            router = r.router.as_deref().unwrap_or("(an earlier build's)"),
            file = %r.file.display(),
            "router keys: removed a key file this node wrote that no router names any more"
        );
    }
}

/// REQ: API-002, T8.2 (ADR-181) — which key files this node sends for the routers in effect (the
/// use-time rule), and each router's credential status.
#[derive(Debug, Default)]
pub(crate) struct UseCheck {
    data_dir: String,
    /// The routers this node's administrators define: its config files and, on a node that takes
    /// the cluster's configuration from Git, the commit in use. None while the files couldn't be
    /// read at start-up (`file_config` then holds the running configuration, API entries and all).
    own: Vec<RouterConfig>,
    /// What `.written` records; `None` when it couldn't be read, so nothing counts as written
    /// (fails closed).
    written: Option<Vec<Record>>,
}

impl UseCheck {
    /// The check for this node now. Blocking (it reads `.written` and the commit in use).
    pub(crate) fn load(src: &Sources) -> Self {
        let file = src.file_config.load_full();
        let data_dir = file.node.data_dir.to_string();
        let own = if src.files_unread.load(std::sync::atomic::Ordering::Acquire) {
            Vec::new()
        } else {
            let mut own = file.router.clone();
            if file.cluster.git.is_some()
                && let Some(list) = crate::gitsource::shared(&file)
                    .as_ref()
                    .and_then(|s| s.get("router"))
                && let Ok(git) = serde_json::from_value::<Vec<RouterConfig>>(list.clone())
            {
                own.extend(git);
            }
            own
        };
        let written = read_records(&data_dir).ok();
        Self {
            data_dir,
            own,
            written,
        }
    }

    /// Whether this node sends `file` as `r`'s `field` (`file_field` in the configuration).
    fn usable(&self, r: &RouterConfig, field: &str, file_field: &str, file: &str) -> bool {
        let at = Endpoint::of(r);
        // The node's own: its files define this router with this file for this endpoint (an
        // API entry that keeps the file and the endpoint of the files' router counts).
        if self.own.iter().any(|o| {
            o.name == r.name && file_of(o, file_field) == Some(file) && Endpoint::of(o) == at
        }) {
            return true;
        }
        // Saved here, through the API, for this router, credential and endpoint.
        self.written.as_ref().is_some_and(|w| {
            w.iter().any(|x| {
                x.router.as_deref() == Some(r.name.as_str())
                    && x.field.as_deref() == Some(field)
                    && x.endpoint.as_ref() == Some(&at)
                    && x.file == Path::new(file)
            })
        }) && crate::secret_path::in_data_secrets(file, &self.data_dir)
    }

    /// Why this node doesn't poll `r`, if it doesn't: a key file it won't send for it.
    pub(crate) fn refused(&self, r: &RouterConfig) -> Option<String> {
        let (field, file_field, _) = files_of(r)
            .find(|(field, file_field, file)| !self.usable(r, field, file_field, file))?;
        Some(format!(
            "not read on this node: its `{file_field}` isn't a key this node saved for this router at this address, and this node's config files don't define the router with it; an administrator can enter the {field} again (Clients → Discovered → Routers)"
        ))
    }

    /// Each credential `r` names, by its body name: `saved` (the file is there and this node
    /// uses it), `missing` (no file there on this node: enter it again), or `not_used` (there,
    /// but not one this node sends for this router). Nothing in a file is read.
    pub(crate) fn status(&self, r: &RouterConfig) -> BTreeMap<String, String> {
        files_of(r)
            .map(|(field, file_field, file)| {
                let s = if !Path::new(file).is_file() {
                    "missing"
                } else if self.usable(r, field, file_field, file) {
                    "saved"
                } else {
                    "not_used"
                };
                (field.to_owned(), s.to_owned())
            })
            .collect()
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
        assert_eq!((w[0].field, w[0].value.as_str()), ("api_key", "k1"));
        let f = w[0].file.to_string_lossy().into_owned();
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
        // A name that isn't a safe file name, or a long one, is shortened and hashed.
        for name in ["a b/../c", &"x".repeat(200)] {
            let mut b = serde_json::json!({"type": "unifi", "url": "https://h", "api_key": "k"});
            let w = prepare_body("/data", name, None, &mut b).unwrap();
            let base = w[0]
                .file
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            assert!(w[0].file.starts_with("/data/secrets"), "{name}");
            assert!(base.len() < 100 && !base.contains('/'), "{base}");
        }

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
            serde_json::json!({"type": "unifi", "url": "https://192.168.1.1", "site": "other"}),
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

    /// REQ: API-002, T8.2 (ADR-181) — F9: a new key file is private (0600, in a 0700 folder, and
    /// the list of files written too), recorded first for its router, credential and endpoint,
    /// and never overwrites a file; a change that fails before its entry is stored removes what
    /// it made and its records, and never a file that was there before or a record another write
    /// made.
    #[cfg(unix)]
    #[test]
    fn api_002_router_key_files_are_private_recorded_and_undone() {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let d = tmp("files");
        let dd = d.to_string_lossy().into_owned();
        let r = router(serde_json::json!({"name": "udm", "type": "unifi",
            "url": "https://192.168.1.1", "api_key_file": "x"}));
        let f = secrets_dir(&dd).join("router-udm.api_key.1");
        let first = Prepared {
            data_dir: dd.clone(),
            writes: vec![NewKey {
                field: "api_key",
                file: f.clone(),
                value: "k1".into(),
            }],
            old: Vec::new(),
        };
        // Dropped without keep: undone.
        drop(first.write(Some(&r)).unwrap());
        assert!(!f.exists());
        assert_eq!(read_records(&dd).unwrap(), Vec::new());
        // Kept.
        first.write(Some(&r)).unwrap().keep();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "k1");
        assert_eq!(mode(&f), 0o600);
        assert_eq!(mode(&secrets_dir(&dd)), 0o700);
        assert_eq!(mode(&secrets_dir(&dd).join(WRITTEN)), 0o600);
        let saved = vec![Record {
            router: Some("udm".into()),
            field: Some("api_key".into()),
            endpoint: Some(Endpoint::of(&r)),
            file: f.clone(),
        }];
        assert_eq!(read_records(&dd).unwrap(), saved);
        // Never overwritten; the record of the kept file stays.
        assert!(first.write(Some(&r)).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "k1");
        assert_eq!(read_records(&dd).unwrap(), saved);
        // A file someone put there under that name stays, and isn't recorded.
        let theirs = secrets_dir(&dd).join("router-udm.api_key.2");
        std::fs::write(&theirs, "theirs").unwrap();
        let second = Prepared {
            data_dir: dd.clone(),
            writes: vec![NewKey {
                field: "api_key",
                file: theirs.clone(),
                value: "k2".into(),
            }],
            old: Vec::new(),
        };
        assert!(second.write(Some(&r)).is_err());
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        assert_eq!(read_records(&dd).unwrap(), saved);
        // A line an earlier build wrote (the path alone) reads as a record for no router.
        std::fs::write(secrets_dir(&dd).join(WRITTEN), format!("{}\n", f.display())).unwrap();
        assert_eq!(
            read_records(&dd).unwrap(),
            vec![Record {
                router: None,
                field: None,
                endpoint: None,
                file: f.clone()
            }]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// REQ: API-002, T8.2 (ADR-181) — the use-time rule: a key file is sent for a router only if
    /// it was saved here for that router, credential and endpoint and is in `secrets/`, or this
    /// node's own files define the router with it at that endpoint. Another router naming it, the
    /// same router at another address, a line from an earlier build, an unreadable list, and a
    /// record for a file outside `secrets/` are all refused; a missing file reads as missing.
    #[test]
    fn api_002_a_key_file_is_used_only_for_the_router_it_was_saved_for() {
        let d = tmp("use");
        let dd = d.to_string_lossy().into_owned();
        std::fs::create_dir_all(secrets_dir(&dd)).unwrap();
        let key = secrets_dir(&dd).join("router-udm.api_key.1");
        let out = d.join("outside-key");
        std::fs::write(&key, "k").unwrap();
        std::fs::write(&out, "k").unwrap();
        let at = |name: &str, url: &str, file: &Path| {
            router(
                serde_json::json!({"name": name, "type": "unifi", "url": url,
                "api_key_file": file.to_str().unwrap()}),
            )
        };
        let rec = |name: Option<&str>, r: &RouterConfig, file: &Path| Record {
            router: name.map(str::to_owned),
            field: Some("api_key".into()),
            endpoint: Some(Endpoint::of(r)),
            file: file.to_path_buf(),
        };
        let check = |own: Vec<RouterConfig>, written: Option<Vec<Record>>| UseCheck {
            data_dir: dd.clone(),
            own,
            written,
        };
        let udm = at("udm", "https://192.168.1.1", &key);
        let c = check(Vec::new(), Some(vec![rec(Some("udm"), &udm, &key)]));
        assert!(c.refused(&udm).is_none());
        assert_eq!(c.status(&udm)["api_key"], "saved");
        for other in [
            at("evil", "https://192.168.1.1", &key),
            at("udm", "https://203.0.113.9", &key),
        ] {
            assert!(c.refused(&other).is_some(), "{other:?}");
            assert_eq!(c.status(&other)["api_key"], "not_used");
        }
        let legacy = Record {
            router: None,
            field: None,
            endpoint: None,
            file: key.clone(),
        };
        for c in [
            check(Vec::new(), Some(vec![legacy])),
            check(Vec::new(), None),
        ] {
            assert!(c.refused(&udm).is_some());
        }
        let udm_out = at("udm", "https://192.168.1.1", &out);
        assert!(
            check(Vec::new(), Some(vec![rec(Some("udm"), &udm_out, &out)]))
                .refused(&udm_out)
                .is_some(),
            "recorded, but outside secrets/"
        );
        // The node's own files: the same router, file and endpoint, whatever else differs.
        let c = check(vec![udm_out.clone()], None);
        let mut slower = udm_out.clone();
        slower.interval_secs = 600;
        assert!(c.refused(&udm_out).is_none() && c.refused(&slower).is_none());
        assert!(
            c.refused(&at("udm", "https://203.0.113.9", &out))
                .unwrap()
                .contains("api_key_file")
        );
        assert!(
            c.refused(&at("other", "https://192.168.1.1", &out))
                .is_some()
        );
        // Not there: missing.
        std::fs::remove_file(&key).unwrap();
        assert_eq!(
            check(Vec::new(), Some(vec![rec(Some("udm"), &udm, &key)])).status(&udm)["api_key"],
            "missing"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
