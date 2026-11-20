//! REQ: OPS-014, AGT-004 (ADR-182) — updates driven from the cluster: ask a native node to
//! install a version, and read where it stands. The node only writes a request into its data
//! directory; its root update service (`telltale-update.path`, off until enabled) verifies the
//! release signature, installs it, and restarts the node, so the node itself never needs root
//! and a container or Kubernetes pod is still upgraded by its image (ADR-046). Requests go to
//! the node they're for over the cluster channel, like maintenance. Installing needs an admin
//! (agents: `cluster:upgrade`); reading is for viewers. Audit-logged as `node.update.request`.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::Auth;
use crate::auth::routes::{body, principal, reason, remote};
use crate::model::{NodeUpdate, NodeUpdateRequest};
use crate::problem::{Code, Problem};
use crate::{NodeUpdateCall, Shared};
use utoipa::IntoParams;

/// The longest version string taken.
const MAX_VERSION: usize = 40;

/// A version's sort key: `(major, minor, patch, edge run)`, a release after every edge build of
/// its version. `None` for anything else (a local `dev` build, a pull request's build), and for
/// a number written with a leading zero (`0.09.0`): CI never writes one, so one version has one
/// spelling (REQ: OPS-014, ADR-182).
pub fn version_key(v: &str) -> Option<(u64, u64, u64, u64)> {
    let (core, pre) = v.split_once('-').unwrap_or((v, ""));
    let num = |p: &str| {
        (!p.is_empty()
            && p.len() <= 9
            && p.bytes().all(|b| b.is_ascii_digit())
            && (p.len() == 1 || !p.starts_with('0')))
        .then(|| p.parse::<u64>().ok())
        .flatten()
    };
    let mut it = core.split('.');
    let (a, b, c) = (num(it.next()?)?, num(it.next()?)?, num(it.next()?)?);
    if it.next().is_some() {
        return None;
    }
    let run = if pre.is_empty() {
        u64::MAX
    } else {
        num(pre.strip_prefix("edge.")?)?
    };
    Some((a, b, c, run))
}

/// Whether `v` names an edge build (`0.8.0-edge.393`).
pub fn is_edge(v: &str) -> bool {
    v.contains("-edge.")
}

/// REQ: OPS-014 (ADR-182) — a request never changes a node's channel: a node that runs a release
/// takes only a release; one on the edge channel takes an edge build or a release (newer, as
/// every request). `Err` is why not, for `current` running and `to` asked.
pub fn same_channel(current: &str, to: &str) -> Result<(), String> {
    if is_edge(to) && !is_edge(current) {
        return Err(format!(
            "this node runs {current}, on the stable channel: a request never moves it to an edge build ({to})"
        ));
    }
    Ok(())
}

/// The hint for [`Code::UpdateChannel`].
pub const CHANNEL_HINT: &str = "Ask for a release, or install an edge build by hand with self-update on that node (as root: telltale self-update --channel edge --restart).";

/// A version the cluster may ask a node to install: a release or an edge build.
pub fn check_version(to: &str) -> Result<String, Problem> {
    let to = to.trim();
    if to.len() <= MAX_VERSION && version_key(to).is_some() {
        return Ok(to.to_owned());
    }
    Err(Problem::new(
        Code::UpdateInvalid,
        format!("`to`: `{}` isn't a release or edge build version", to.chars().take(MAX_VERSION).collect::<String>()),
    )
    .hint("A release such as 0.8.0, or an edge build such as 0.8.0-edge.393 (`telltale --version` prints a node's)."))
}

/// Reading a node's update state (viewer).
pub(crate) fn read_routes(backend: Shared) -> Router {
    Router::new()
        .route("/api/v1/nodes/{id}/update", get(node_update_status))
        .with_state(backend)
}

/// Asking a node to install a version (admin).
pub(crate) fn admin_routes(backend: Shared, auth: Arc<Auth>) -> Router {
    Router::new()
        .route("/api/v1/nodes/{id}/update", post(node_update_request))
        .with_state((backend, auth))
}

/// A node's version and update state.
///
/// The version it runs, how it was installed, whether its root update service
/// (`telltale-update.path`) is enabled, a request it hasn't picked up yet, and the service's
/// latest run (`installing`, `updated`, `up_to_date`, `failed`, or `refused`, with the error).
/// `id` is `local` (the node answering) or a cluster node's ID, site, or pod name: the read goes
/// to that node over the cluster channel.
#[utoipa::path(get, path = "/api/v1/nodes/{id}/update", tag = "system",
    params(("id" = String, Path, description = "`local`, or a node's ID, site, or pod name.")),
    responses(
        (status = 200, body = NodeUpdate, description = "The node's version and update state."),
        (status = 404, body = Problem, description = "No such node (`node_unknown`)."),
        (status = 503, body = Problem, description = "The node isn't reachable over the cluster channel (`node_unreachable`)."),
    ))]
async fn node_update_status(State(backend): State<Shared>, Path(id): Path<String>) -> Response {
    match backend.node_update(&id, NodeUpdateCall::Status).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Query parameters of an update request.
#[derive(Debug, Default, serde::Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct UpdateQuery {
    /// REQ: AGT-002 — run every check the node makes and say what would happen; nothing is
    /// written and nothing installed (`state: would_request` or `up_to_date`).
    pub dry_run: Option<bool>,
}

/// Install a version on a node.
///
/// A native node (installed with `install.sh`) hands the request to its root update service,
/// which verifies the release's signature and checksum, installs the binary, and restarts the
/// node; `GET` on the same path shows how it went. The version is a release (`0.8.0`) or an edge
/// build (`0.8.0-edge.393`); an edge build can be installed only while it is the newest on the
/// edge channel. Never older than the node's version (`update_downgrade`: downgrade by hand), and
/// never a change of channel: a node on releases takes only a release (`update_channel`). The
/// same version answers `up_to_date` without a request. One at a time per node: while a request
/// waits for the update service or an update runs, another gets `update_in_progress`. A node in a
/// container or Kubernetes is upgraded by its image (`update_unsupported`), and a node whose
/// update service isn't enabled says so (`update_not_enabled`). In a cluster, upgrade replicas
/// before the primary, one at a time: `telltale ctl upgrade` does this. `id` as for the read.
/// `?dryRun=true` makes every check and writes nothing. It changes no configuration, so a
/// GitOps-managed cluster takes it too. Needs the admin role (agents: the `cluster:upgrade`
/// scope); audit-logged as `node.update.request`.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/update", tag = "system",
    params(("id" = String, Path, description = "`local`, or a node's ID, site, or pod name."), UpdateQuery),
    request_body = NodeUpdateRequest,
    responses(
        (status = 200, body = NodeUpdate, description = "`state`: `requested` (handed to the update service) or `up_to_date`; with `dryRun`, `would_request` or `up_to_date`."),
        (status = 404, body = Problem, description = "No such node (`node_unknown`)."),
        (status = 409, body = Problem, description = "The node is upgraded by its image (`update_unsupported`), its update service is off (`update_not_enabled`), the version is older than its own (`update_downgrade`) or on another channel (`update_channel`), or an update is already under way there (`update_in_progress`)."),
        (status = 422, body = Problem, description = "Not a release or edge build version (`update_invalid`)."),
        (status = 503, body = Problem, description = "The node isn't reachable over the cluster channel (`node_unreachable`)."),
    ))]
async fn node_update_request(
    State((backend, auth)): State<(Shared, Arc<Auth>)>,
    Path(id): Path<String>,
    Query(q): Query<UpdateQuery>,
    headers: HeaderMap,
    ext: axum::http::Extensions,
    b: Result<Json<NodeUpdateRequest>, JsonRejection>,
) -> Response {
    let req = match body(b) {
        Ok(r) => r,
        Err(p) => return p.into_response(),
    };
    let p = match principal(&ext) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let to = match check_version(&req.to) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let actor = auth.actor(&p, remote(&auth, &ext, &headers), reason(&headers));
    if q.dry_run.unwrap_or(false) {
        // REQ: AGT-002 — the node's own checks, nothing written: not audited, as other dry runs.
        return match backend
            .node_update(&id, NodeUpdateCall::DryRun { to: to.clone() })
            .await
        {
            Ok(r) => Json(r).into_response(),
            Err(e) => e.into_response(),
        };
    }
    let call = NodeUpdateCall::Request {
        to: to.clone(),
        by: actor.name.clone(),
    };
    match backend.node_update(&id, call).await {
        Ok(r) => {
            auth.record(
                &actor,
                "node.update.request",
                &r.node,
                &serde_json::json!({
                    "to": to,
                    "from": r.version,
                    "state": r.state,
                    "requested": id,
                }),
            );
            Json(r).into_response()
        }
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // REQ: OPS-014 — only releases and edge builds are versions a node may be asked for, and
    // they sort as the update check sorts them: a release after every edge build of its version.
    #[test]
    fn ops_014_versions_are_checked_and_ordered() {
        assert_eq!(check_version(" 0.8.0 ").unwrap(), "0.8.0");
        assert_eq!(check_version("0.8.0-edge.393").unwrap(), "0.8.0-edge.393");
        for bad in [
            "",
            "dev",
            "0.8",
            "0.8.0.1",
            "v0.8.0",
            "0.8.0-pr.12",
            "0.8.0-edge.",
            "0.8.0-edge.x",
            "0.8.0;rm",
            "1.2.3 -edge.4",
            "0.8.0/../..",
            "01234567890.0.0",
            // REQ: OPS-014 (ADR-182) — one spelling per version: no leading zeros.
            "0.09.0",
            "00.8.0",
            "0.8.0-edge.01",
        ] {
            let e = check_version(bad).unwrap_err();
            assert_eq!((e.code, e.status), (Code::UpdateInvalid, 422), "{bad:?}");
        }
        assert!(check_version(&format!("0.8.{}", "1".repeat(40))).is_err());
        let k = |v| version_key(v).unwrap();
        assert!(k("0.8.0-edge.393") < k("0.8.0"));
        assert!(k("0.7.0") < k("0.8.0-edge.1"));
        assert!(k("0.8.0-edge.9") < k("0.8.0-edge.10"));
        assert!(is_edge("0.8.0-edge.393") && !is_edge("0.8.0"));
    }
}
