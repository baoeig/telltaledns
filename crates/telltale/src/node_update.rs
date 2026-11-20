//! Updates driven from the cluster (REQ: OPS-014, CLU-010; ADR-182): an admin, or
//! `telltale ctl upgrade`, asks a native node to install a version, and the node's root update
//! service installs it.
//!
//! - **The node never replaces its own binary.** It runs as `telltale` under
//!   `ProtectSystem=strict` (ADR-038), so it can't, and ADR-046 keeps it that way. It writes
//!   `<data_dir>/update/request.json` and nothing else. `telltale-update.path` (root, installed by
//!   `install.sh`, off until enabled) starts `telltale-update.service`, which runs
//!   `telltale self-update --request …` as root: [`serve_request`] reads the request, removes it,
//!   checks it again, installs the signed release with [`crate::selfupdate`], records the outcome
//!   in [`RESULT_PATH`] (a root-owned directory the node can read), and restarts the node. Of the
//!   data directory, root may write only the request's folder (`update/`, to remove the request).
//! - **What the request file can do is bounded.** The root side trusts nothing in it: it reads
//!   only a small regular file the node's user wrote, and acts only on a release or edge build
//!   version that is newer than the one installed and on the node's channel. The binary still has
//!   to carry the release signature (ADR-182 says exactly what each party can make root do).
//! - **Containers and Kubernetes pods** are upgraded by their image and say so
//!   (`update_unsupported`); a Helm chart's pre-upgrade hook (`upgradeHook`) runs
//!   `telltale ctl upgrade` from the new image, so the native nodes go first.
//!
//! Nothing here is on the query path.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use telltale_api::NodeUpdateCall;
use telltale_api::model::{NodeUpdate, NodeUpdatePending, NodeUpdateRun};
use telltale_api::problem::{Code, Problem};
use telltale_api::update_api::{CHANNEL_HINT, check_version, is_edge, same_channel, version_key};
use telltale_cluster::net::Cluster;
use tracing::{info, warn};

/// REQ: OPS-014 (ADR-182) — the request's folder, in the node's data directory: the only place
/// in it the root update service may write (`ReadWritePaths=` of `telltale-update.service`).
pub(crate) const REQUEST_DIR: &str = "update";
/// The request, in [`REQUEST_DIR`] (`PathExists=` of `telltale-update.path`).
pub(crate) const REQUEST_FILE: &str = "request.json";
/// The update service's latest run, in its own state directory (`StateDirectory=telltale-update`).
pub(crate) const RESULT_PATH: &str = "/var/lib/telltale-update/result.json";
/// `systemctl enable telltale-update.path` makes this link (`WantedBy=paths.target`).
const ENABLED_LINK: &str = "/etc/systemd/system/paths.target.wants/telltale-update.path";
/// The RPC to the node a request or read is for.
pub(crate) const KIND: &str = "update";
/// How long the entry node waits for the target's answer.
const DEADLINE: Duration = Duration::from_secs(5);
/// An RPC older than this when the target reads it is dropped, as for maintenance.
const STALE_RPC_MS: u64 = 30_000;
/// The update service refuses a request file older than this (or this far in the future): one
/// left behind while the service was off isn't acted on when it's switched on months later.
const STALE_REQUEST_MS: u64 = 3_600_000;
/// The largest request file the update service reads.
const MAX_REQUEST: u64 = 4096;
/// The longest `by` kept.
const MAX_BY: usize = 200;

/// `<data_dir>/update/request.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RequestFile {
    pub(crate) id: String,
    pub(crate) to: String,
    pub(crate) by: String,
    pub(crate) requested_ms: u64,
}

/// [`RESULT_PATH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResultFile {
    pub(crate) id: String,
    pub(crate) to: String,
    pub(crate) from: String,
    /// `installing`, `updated`, `up_to_date`, `failed`, or `refused`.
    pub(crate) state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    pub(crate) by: String,
    pub(crate) at_ms: u64,
}

/// A request on the cluster channel: the call and when the entry node sent it.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    call: NodeUpdateCall,
    sent_ms: u64,
}

fn now_ms() -> u64 {
    crate::maintenance::now_ms()
}

fn rfc3339(ms: u64) -> String {
    telltale_api::time::format_us(ms.saturating_mul(1000))
}

/// What a node knows about itself, for [`status`] and [`request`].
#[derive(Debug, Clone)]
pub(crate) struct Here {
    /// The node's label (pod, site, or ID).
    pub(crate) node: String,
    pub(crate) version: String,
    /// `native`, `container`, or `helm`.
    pub(crate) install: String,
    /// The update service is enabled.
    pub(crate) enabled: bool,
    pub(crate) data_dir: PathBuf,
    pub(crate) result: PathBuf,
}

impl Here {
    /// This process.
    fn of(src: &crate::http::Sources) -> Self {
        Self {
            node: crate::maintenance::label(src),
            version: crate::build_info::VERSION.to_owned(),
            install: crate::build_info::install_type().to_owned(),
            enabled: Path::new(ENABLED_LINK).exists(),
            data_dir: PathBuf::from(src.config.load().node.data_dir.as_str()),
            result: PathBuf::from(RESULT_PATH),
        }
    }

    /// REQ: OPS-014 (ADR-182) — `<data_dir>/update/request.json`, where the units look.
    pub(crate) fn request_path(&self) -> PathBuf {
        self.data_dir.join(REQUEST_DIR).join(REQUEST_FILE)
    }
}

/// A JSON file, if it's there and parses (a missing or half-written file reads as none).
fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Option<T> {
    let bytes = std::fs::read(p).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// REQ: OPS-014 — the node's version, its update service, a request not picked up yet, and the
/// service's latest run.
pub(crate) fn status(h: &Here) -> NodeUpdate {
    let pending = read_json::<RequestFile>(&h.request_path()).map(|r| NodeUpdatePending {
        id: r.id,
        to: r.to,
        by: r.by,
        requested_at: rfc3339(r.requested_ms),
    });
    let last = read_json::<ResultFile>(&h.result).map(|r| NodeUpdateRun {
        id: r.id,
        to: r.to,
        from: r.from,
        state: r.state,
        error: r.error,
        by: r.by,
        at: rfc3339(r.at_ms),
    });
    NodeUpdate {
        node: h.node.clone(),
        version: h.version.clone(),
        install: h.install.clone(),
        enabled: h.enabled,
        state: None,
        pending,
        last,
    }
}

/// REQ: OPS-014 — whether this node can take `to`, and if so the request to hand over (`None`:
/// it already runs `to`).
pub(crate) fn plan(h: &Here, to: &str) -> Result<Option<String>, Problem> {
    let to = check_version(to)?;
    if h.install != "native" {
        let (what, hint) = if h.install == "helm" {
            (
                "a Kubernetes pod",
                "Upgrade the chart (or its image.tag). With the chart's upgradeHook enabled, the native nodes are upgraded first.",
            )
        } else {
            (
                "a container",
                "Pull the new image and restart it: docker compose pull && docker compose up -d.",
            )
        };
        return Err(Problem::new(
            Code::UpdateUnsupported,
            format!(
                "{} runs in {what}: its image is upgraded by its deployment, not by TelltaleDNS",
                h.node
            ),
        )
        .hint(hint));
    }
    let (Some(have), Some(want)) = (version_key(&h.version), version_key(&to)) else {
        return Err(Problem::new(
            Code::UpdateUnsupported,
            format!(
                "{} runs a local build ({}), not a release",
                h.node, h.version
            ),
        )
        .hint("Install a release or edge build with install.sh first."));
    };
    if want == have {
        return Ok(None);
    }
    if want < have {
        return Err(Problem::new(
            Code::UpdateDowngrade,
            format!("{} runs {}, which is newer than {to}", h.node, h.version),
        )
        .hint(format!(
            "The cluster only moves nodes forward. To go back, as root on the node: telltale self-update --to {to} --restart"
        )));
    }
    // REQ: OPS-014 (ADR-182) — never a channel change: checked here (for the API on this node
    // and for the cluster RPC alike) and again by the update service.
    if let Err(why) = same_channel(&h.version, &to) {
        return Err(
            Problem::new(Code::UpdateChannel, format!("{}: {why}", h.node)).hint(CHANNEL_HINT),
        );
    }
    if !h.enabled {
        return Err(Problem::new(
            Code::UpdateNotEnabled,
            format!("{}'s update service is off, so nothing would install {to}", h.node),
        )
        .hint("On the node, as root: systemctl enable --now telltale-update.path (install.sh adds it; re-run install.sh to add it to an older install)."));
    }
    Ok(Some(to))
}

/// REQ: OPS-014 — hands `to` to this node's update service (or answers that it runs it).
pub(crate) fn request(h: &Here, to: &str, by: &str, now: u64) -> Result<NodeUpdate, Problem> {
    let Some(to) = plan(h, to)? else {
        let mut s = status(h);
        s.state = Some("up_to_date".into());
        return Ok(s);
    };
    if let Some(busy) = in_progress(h, now) {
        return Err(busy);
    }
    let r = RequestFile {
        id: format!("{now:x}-{:08x}", rand::random::<u32>()),
        to: to.clone(),
        by: by.chars().take(MAX_BY).collect(),
        requested_ms: now,
    };
    // REQ: OPS-014 (ADR-182) — the request's own folder (0750, this node's user's), the only
    // place of the data directory the update service writes. A request is created, never
    // renamed over another: of two at once, one is written and the other waits.
    let written = create_folder(&h.data_dir.join(REQUEST_DIR))
        .and_then(|()| write_new(&h.request_path(), &r, 0o640));
    match written {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(busy_waiting(h)),
        Err(e) => {
            return Err(Problem::internal(format!(
                "can't write the update request: {e}"
            )));
        }
    }
    info!(to = %to, by = %r.by, "update: requested {to} (the update service installs it and restarts this node)");
    let mut s = status(h);
    s.state = Some("requested".into());
    Ok(s)
}

/// REQ: OPS-014, AGT-002 (ADR-182) — every check [`request`] makes, and what it would do
/// (`would_request`, or `up_to_date`), with nothing written.
pub(crate) fn dry_run(h: &Here, to: &str, now: u64) -> Result<NodeUpdate, Problem> {
    let mut s = status(h);
    if plan(h, to)?.is_none() {
        s.state = Some("up_to_date".into());
        return Ok(s);
    }
    if let Some(busy) = in_progress(h, now) {
        return Err(busy);
    }
    s.state = Some("would_request".into());
    Ok(s)
}

/// REQ: OPS-014 (ADR-182) — a run that says `installing` holds a new request off this long (the
/// update service's own limit, `TimeoutStartSec=900`): one that died is no lock for ever.
const RUNNING_MS: u64 = 15 * 60 * 1000;

fn busy_waiting(h: &Here) -> Problem {
    Problem::new(
        Code::UpdateInProgress,
        format!("{} has a request waiting for its update service", h.node),
    )
    .hint("One at a time: GET on the same path shows it; ask again once it is done.")
    .retry_after(30)
}

/// REQ: OPS-014 (ADR-182) — one at a time per node: a request waiting for the update service, or
/// a run that says `installing` and isn't older than [`RUNNING_MS`], holds a new one off. A
/// request older than the update service would take (an hour) is dead, and is replaced.
fn in_progress(h: &Here, now: u64) -> Option<Problem> {
    let path = h.request_path();
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        let at = read_json::<RequestFile>(&path)
            .map(|r| r.requested_ms)
            .or_else(|| {
                let t = meta.modified().ok()?;
                let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
                u64::try_from(d.as_millis()).ok()
            });
        if at.is_none_or(|at| now.saturating_sub(at) <= STALE_REQUEST_MS) {
            return Some(busy_waiting(h));
        }
        // Dead: the update service would refuse it. Only a file of ours is removed here.
        if meta.file_type().is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
    let last = read_json::<ResultFile>(&h.result)?;
    (last.state == "installing" && now.saturating_sub(last.at_ms) < RUNNING_MS).then(|| {
        Problem::new(
            Code::UpdateInProgress,
            format!("{} is installing {}", h.node, last.to),
        )
        .hint("One at a time: GET on the same path shows how it goes; ask again once it is done.")
        .retry_after(60)
    })
}

/// Writes `v` as JSON to a temporary file beside `path` and renames it to `path`, which must not
/// exist (`AlreadyExists` if it does: nothing is replaced, and a reader never sees half a file
/// or a file with two names).
fn write_new<T: Serialize>(path: &Path, v: &T, mode: u32) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".request.{:08x}.tmp", rand::random::<u32>()));
    let body = serde_json::to_vec_pretty(v).map_err(std::io::Error::other)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut f = opts.open(&tmp)?;
    let written = f.write_all(&body).and_then(|()| f.sync_all());
    drop(f);
    let moved = written.and_then(|()| rename_new(&tmp, path));
    if moved.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    moved
}

/// `rename(from, to)` that never replaces `to` (`RENAME_NOREPLACE`).
#[cfg(target_os = "linux")]
fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags};
    match rustix::fs::renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        // A filesystem without it: the plain rename, after a look (not atomic, but no worse).
        Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS) => rename_if_absent(from, to),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
    rename_if_absent(from, to)
}

fn rename_if_absent(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(std::io::ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(from, to)
}

/// Makes the folder `p` (mode 0750) unless it is there.
fn create_folder(p: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o750);
    }
    match b.create(p) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

/// Writes `v` as JSON next to `path` and renames it over `path` (a reader never sees half a
/// file, and a symlink planted at `path` is replaced, not followed).
fn write_atomic<T: Serialize>(path: &Path, v: &T, mode: u32) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{:08x}.tmp", rand::random::<u32>()));
    let body = serde_json::to_vec_pretty(v).map_err(std::io::Error::other)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut f = opts.open(&tmp)?;
    let written = f.write_all(&body).and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

// ---- The API and the cluster channel ----

/// REQ: OPS-014 — a call for this node, on a blocking thread.
pub(crate) fn local_backend_call(
    src: Arc<crate::http::Sources>,
    node: &str,
    call: NodeUpdateCall,
) -> telltale_api::BoxFuture<Result<NodeUpdate, Problem>> {
    if !crate::maintenance::is_local(&src, node) {
        let node = node.to_owned();
        return Box::pin(async move {
            Err(Problem::new(
                Code::NodeUnknown,
                format!("no node `{node}`: this node isn't in a cluster"),
            )
            .hint("Use `local` for this node."))
        });
    }
    Box::pin(async move {
        tokio::task::spawn_blocking(move || apply_here(&src, call))
            .await
            .map_err(|e| Problem::internal(format!("update: {e}")))?
    })
}

fn apply_here(src: &crate::http::Sources, call: NodeUpdateCall) -> Result<NodeUpdate, Problem> {
    let h = Here::of(src);
    match call {
        NodeUpdateCall::Status => Ok(status(&h)),
        NodeUpdateCall::Request { to, by } => request(&h, &to, &by, now_ms()),
        NodeUpdateCall::DryRun { to } => dry_run(&h, &to, now_ms()),
    }
}

/// REQ: OPS-014 — sends a call to `peer` over the cluster channel.
pub(crate) async fn send(
    cluster: Arc<Cluster>,
    peer: String,
    label: String,
    call: NodeUpdateCall,
) -> Result<NodeUpdate, Problem> {
    let body = serde_json::to_vec(&Envelope {
        call,
        sent_ms: now_ms(),
    })
    .map_err(|e| Problem::internal(e.to_string()))?;
    let reply = cluster.call(&peer, KIND, body, DEADLINE).await.map_err(|e| {
        let older = e.contains("unknown call");
        Problem::new(
            Code::NodeUnreachable,
            if older {
                format!("{label} runs a version without updates from the cluster")
            } else {
                format!("{label} isn't reachable over the cluster channel: {e}")
            },
        )
        .hint(if older {
            "Upgrade it by hand this once (as root: telltale self-update --restart), then enable telltale-update.path."
        } else {
            "The node installs the version itself, so it must be up. Check the Cluster page."
        })
    })?;
    crate::forward::decode_answer(&reply)
}

/// REQ: OPS-014 (ADR-182) — a peer's call for this node: applied and, for a request, audited
/// here with the entry node. `ephemeral`: the peer is an ephemeral member or one the registry
/// doesn't list (`federated::rpc_handler`), which may read but never ask for an install.
pub(crate) fn handle(
    src: &crate::http::Sources,
    cluster: &Cluster,
    peer: &str,
    body: &[u8],
    ephemeral: bool,
) -> Result<Vec<u8>, String> {
    let Envelope { call, sent_ms } =
        serde_json::from_slice(body).map_err(|e| format!("bad update request: {e}"))?;
    // REQ: OPS-014, CLU-009 (ADR-182, ADR-058) — as for maintenance, rollouts, removals,
    // simulations and drills: an ephemeral member serves no UI, so no user is behind a request
    // from it (nor from a peer the registry doesn't list).
    if ephemeral && !matches!(call, NodeUpdateCall::Status) {
        return Err("an ephemeral member can't change this node's state".into());
    }
    if now_ms() > sent_ms.saturating_add(STALE_RPC_MS) {
        return Err("the request is too old; the node that sent it gave up on it".into());
    }
    let via = cluster
        .members()
        .into_iter()
        .find(|m| m.node_id == peer)
        .map(|m| if m.pod.is_empty() { m.site } else { m.pod })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| peer.to_owned());
    // Only the sender (`via`, its stream's certificate) is checked here; the user is the
    // entry node's word for it, so the audit names the node and keeps the user as unverified.
    let (call, said) = match call {
        NodeUpdateCall::Request { to, by } => (
            NodeUpdateCall::Request {
                to,
                by: format!("{by} via {via} (unverified)"),
            },
            Some(by),
        ),
        // A read, or a dry run (nothing written): not audited.
        other => (other, None),
    };
    let to = match &call {
        NodeUpdateCall::Request { to, .. } => Some(to.clone()),
        NodeUpdateCall::Status | NodeUpdateCall::DryRun { .. } => None,
    };
    let r = apply_here(src, call);
    if let (Some(said), Some(auth)) = (said, src.auth.get()) {
        let actor = telltale_api::auth::Actor {
            name: format!("node {via}"),
            kind: "cluster",
            remote: Some(format!("node {peer}")),
            reason: None,
        };
        match &r {
            Ok(res) => auth.record(
                &actor,
                "node.update.request",
                &res.node,
                &serde_json::json!({ "to": to, "from": res.version, "state": res.state,
                    "by": said, "byVerified": false }),
            ),
            Err(p) => auth.record(
                &actor,
                "node.update.refused",
                &crate::maintenance::label(src),
                &serde_json::json!({ "to": to, "code": p.code, "by": said, "byVerified": false }),
            ),
        }
    }
    crate::forward::encode_answer(r)
}

// ---- The root update service ----

/// What [`serve_request`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Served {
    /// No request file.
    Nothing,
    /// The state recorded (`refused`, `up_to_date`, `updated`, `rolled_back`, `unavailable`,
    /// `failed`).
    Done { state: String },
}

/// REQ: OPS-014 (ADR-182) — whether a run ends well for systemd: a refusal is the service doing
/// its job (recorded, exit 0, so refusals don't fail the unit); an install that failed or was
/// rolled back, or that couldn't be fetched (`unavailable`), is a failure.
pub(crate) fn served_ok(state: &str) -> bool {
    !matches!(state, "failed" | "rolled_back" | "unavailable")
}

/// REQ: OPS-014 (ADR-182) — what the update service does on its host, beyond reading the request
/// and recording the result: injected, so every step and its order can be tested.
pub(crate) trait Host {
    /// Installs `to` (the signed, pinned self-update in production).
    fn install(&mut self, to: &str) -> Result<crate::selfupdate::Outcome, String>;
    /// Restarts the node (`systemctl restart telltale`).
    fn restart(&mut self) -> Result<(), String>;
    /// Waits, bounded, for the restarted node to be healthy; `Err` says how it wasn't.
    fn healthy(&mut self) -> Result<(), String>;
    /// Puts `previous` (the binary the install kept) back in place.
    fn restore(&mut self, previous: &Path) -> Result<(), String>;
}

/// REQ: OPS-014 (ADR-182) — what the update service records for anything at the request that
/// isn't one: the same words whatever was there (the result is world-readable, and says nothing
/// about the files root can see).
const NOT_A_REQUEST: &str = "the request isn't a small regular file written by the node: ignored";

/// REQ: OPS-014 (ADR-182) — reads the request at `path` and removes it. Everything is done
/// relative to the request's folder, opened once without following a link (a folder swapped for
/// a link afterwards changes nothing), and every check is made on the file as opened: the open
/// follows no link and never waits (a FIFO opens at once and is refused), and the file must be
/// a regular file with one name, at most [`MAX_REQUEST`] bytes, owned by the folder's owner, in a
/// folder that is a real directory and not world-writable. Whatever is at the request's name
/// afterwards goes (a directory only if it is empty: root never reaches into the node's files),
/// so it is acted on once at most. `Ok(None)`: nothing there.
#[cfg(unix)]
fn take_request(path: &Path) -> Result<Option<Vec<u8>>, String> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;
    let not_a_request = || Err(NOT_A_REQUEST.to_owned());
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return not_a_request();
    };
    let folder = match rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        // Not a directory, or a link: not the node's folder, so nothing in it is touched.
        Err(_) => return not_a_request(),
    };
    let Ok(fst) = rustix::fs::fstat(&folder) else {
        return not_a_request();
    };
    // Not a folder anyone may write (the owner check below is what keeps others' files out), and
    // the node's own: owned by whoever owns the folder above it (the data directory, the node's
    // user's; its path is root's up to it).
    let above = dir.parent().and_then(|p| rustix::fs::stat(p).ok());
    if FileType::from_raw_mode(fst.st_mode) != FileType::Directory
        || fst.st_mode & 0o002 != 0
        || above.is_none_or(|a| a.st_uid != fst.st_uid)
    {
        return not_a_request();
    }
    // Whatever is at the name goes: a file, a link, a FIFO, or an empty directory.
    let gone = || {
        if rustix::fs::unlinkat(&folder, name, AtFlags::empty()).is_err() {
            let _ = rustix::fs::unlinkat(&folder, name, AtFlags::REMOVEDIR);
        }
    };
    let file = match rustix::fs::openat(
        &folder,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(_) => {
            gone();
            return not_a_request();
        }
    };
    let ok = rustix::fs::fstat(&file).is_ok_and(|st| {
        FileType::from_raw_mode(st.st_mode) == FileType::RegularFile
            && st.st_nlink == 1
            && u64::try_from(st.st_size).is_ok_and(|n| n <= MAX_REQUEST)
            && st.st_uid == fst.st_uid
    });
    if !ok {
        gone();
        return not_a_request();
    }
    let mut body = Vec::new();
    let read = Read::take(&mut std::fs::File::from(file), MAX_REQUEST + 1).read_to_end(&mut body);
    gone();
    if read.is_err() || !u64::try_from(body.len()).is_ok_and(|n| n <= MAX_REQUEST) {
        return not_a_request();
    }
    Ok(Some(body))
}

/// Only Unix has the root update service.
#[cfg(not(unix))]
fn take_request(_: &Path) -> Result<Option<Vec<u8>>, String> {
    Err(NOT_A_REQUEST.to_owned())
}

/// Checks a request the update service read, against the version installed now.
fn check_request(
    body: &[u8],
    current: &str,
    now: u64,
) -> Result<RequestFile, (RequestFile, String)> {
    let r: RequestFile = serde_json::from_slice(body).map_err(|_| {
        (
            RequestFile {
                id: String::new(),
                to: String::new(),
                by: String::new(),
                requested_ms: 0,
            },
            "the request isn't valid JSON of the expected shape".to_owned(),
        )
    })?;
    let bad = |why: String| Err((r.clone(), why));
    let id_ok = !r.id.is_empty()
        && r.id.len() <= 40
        && r.id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    if !id_ok {
        return bad("the request's id is malformed".into());
    }
    if r.by.chars().count() > MAX_BY || r.by.chars().any(char::is_control) {
        return bad("the request's `by` is malformed".into());
    }
    if r.requested_ms.saturating_add(STALE_REQUEST_MS) < now
        || r.requested_ms > now.saturating_add(300_000)
    {
        return bad("the request is more than an hour old (or from the future): ask again".into());
    }
    // REQ: OPS-014 (ADR-182) — exactly as a release or edge build is written: what is checked
    // below and what is fetched is this string, unchanged (padding, a newline or a leading zero
    // is refused, never trimmed into another version).
    let Some(want) = check_version(&r.to)
        .ok()
        .filter(|v| *v == r.to)
        .and_then(|v| version_key(&v))
    else {
        let shown: String = r.to.chars().take(48).collect();
        return bad(format!(
            "`{}` isn't a release or edge build version",
            shown.escape_debug()
        ));
    };
    let Some(have) = version_key(current) else {
        return bad(format!(
            "the installed binary is a local build ({current}): install a release by hand"
        ));
    };
    if want < have {
        return bad(format!(
            "{} is older than the installed {current}: the cluster only moves nodes forward",
            r.to
        ));
    }
    if let Err(why) = same_channel(current, &r.to) {
        return bad(why);
    }
    Ok(r)
}

/// REQ: OPS-014 (ADR-182) — the update service's run: reads and removes the request at
/// `request`, checks it, records `installing` and then the outcome in `result`, and installs with
/// `host`. With `restart`, it then restarts the node and waits for it to be healthy; if it isn't,
/// it puts the previous binary back and restarts once more (one attempt: `rolled_back`, or
/// `failed` if that isn't healthy either). `current` is the installed version.
pub(crate) fn serve_request(
    request: &Path,
    result: &Path,
    current: &str,
    now: u64,
    restart: bool,
    host: &mut impl Host,
) -> Served {
    let record = |r: &RequestFile, state: &str, error: Option<String>| {
        let out = ResultFile {
            id: r.id.clone(),
            to: r.to.clone(),
            from: current.to_owned(),
            state: state.to_owned(),
            error,
            by: r.by.clone(),
            at_ms: now_ms().max(now),
        };
        if let Some(dir) = result.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = write_atomic(result, &out, 0o644) {
            warn!(
                "update: can't record the outcome in {}: {e}",
                result.display()
            );
        }
    };
    let blank = RequestFile {
        id: String::new(),
        to: String::new(),
        by: String::new(),
        requested_ms: 0,
    };
    let body = match take_request(request) {
        Ok(None) => return Served::Nothing,
        Ok(Some(b)) => b,
        Err(e) => {
            warn!("update: {e}");
            record(&blank, "refused", Some(e));
            return done("refused");
        }
    };
    let r = match check_request(&body, current, now) {
        Ok(r) => r,
        Err((r, why)) => {
            warn!("update: refused: {why}");
            record(&r, "refused", Some(why));
            return done("refused");
        }
    };
    if version_key(&r.to) == version_key(current) {
        record(&r, "up_to_date", None);
        return done("up_to_date");
    }
    info!(to = %r.to, by = %r.by, "update: installing {} (was {current})", r.to);
    record(&r, "installing", None);
    let previous = match host.install(&r.to) {
        Ok(crate::selfupdate::Outcome::Updated { previous }) => previous,
        // REQ: OPS-014 (ADR-182) — an edge build main has since passed: nothing installed.
        Ok(crate::selfupdate::Outcome::Unavailable { serves, commit }) => {
            let e = crate::selfupdate::unavailable_text(&r.to, &serves, &commit);
            warn!("update: {} not installed: {e}", r.to);
            record(&r, "unavailable", Some(e));
            return done("unavailable");
        }
        Ok(_) => {
            record(&r, "up_to_date", None);
            return done("up_to_date");
        }
        Err(e) => {
            warn!("update: {} not installed: {e}", r.to);
            record(&r, "failed", Some(e));
            return done("failed");
        }
    };
    let (state, error) = if restart {
        restart_and_check(&r.to, current, &previous, host)
    } else {
        info!("update: installed {}; restart the node to run it", r.to);
        ("updated", None)
    };
    record(&r, state, error);
    done(state)
}

/// REQ: OPS-014 (ADR-182) — the health gate after an install: restarts the node, and if it isn't
/// healthy puts `previous` back, restarts once more and checks again (one rollback, no loop). The
/// state to record (`updated`, `rolled_back`, `failed`) and why.
fn restart_and_check(
    to: &str,
    current: &str,
    previous: &Path,
    host: &mut impl Host,
) -> (&'static str, Option<String>) {
    info!("update: installed {to}; restarting the node");
    let Err(why) = host.restart().and_then(|()| host.healthy()) else {
        info!("update: {to} is running and healthy");
        return ("updated", None);
    };
    warn!("update: {to} isn't healthy after the restart ({why}): putting {current} back");
    match host
        .restore(previous)
        .and_then(|()| host.restart())
        .and_then(|()| host.healthy())
    {
        Ok(()) => {
            let e = format!(
                "{to} wasn't healthy after the restart ({why}); {current} is back and running"
            );
            warn!("update: rolled back: {e}");
            ("rolled_back", Some(e))
        }
        Err(back) => {
            let e = format!(
                "{to} wasn't healthy after the restart ({why}), and putting {current} back didn't bring the node back either ({back})"
            );
            warn!("update: {e}");
            ("failed", Some(e))
        }
    }
}

fn done(state: &str) -> Served {
    Served::Done {
        state: state.to_owned(),
    }
}

/// The channel a version is published on.
pub(crate) fn channel_for(to: &str) -> crate::selfupdate::Channel {
    if is_edge(to) {
        crate::selfupdate::Channel::Edge
    } else {
        crate::selfupdate::Channel::Stable
    }
}

#[cfg(test)]
#[path = "node_update_tests.rs"]
mod tests;
