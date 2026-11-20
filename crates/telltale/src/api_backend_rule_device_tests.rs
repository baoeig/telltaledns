// REQ: FLT-005 (GitHub issue #73) — a quick rule names a device by a `[[client]]` name (any case),
// an IP, or a CIDR, and by nothing else: a name a router, mDNS, or another node gives an address is
// refused. The refusal says what a rule can name and what to do, and when the string is such a
// name, says which address has it. The hint reads this node's tables, but the answer (accept or
// refuse) comes from the configuration alone, so every node gives the same one. Driven through the
// real routes, sign-in and backend (the harness of `local_access_tests`). No test reaches a host:
// the leases are made in memory.

use std::net::Ipv4Addr;
use std::sync::Arc;

use axum::http::StatusCode;

use super::local_access_tests::Node;

fn lease(ip: [u8; 4], name: &str) -> crate::devices::Lease {
    crate::devices::Lease {
        mac: String::new(),
        ip: Ipv4Addr::from(ip),
        hostname: Some(name.into()),
        expires: 0,
        offline: false,
    }
}

fn table(leases: &[crate::devices::Lease]) -> Arc<crate::devices::Leases> {
    Arc::new(leases.iter().map(|l| (l.ip, l.clone())).collect())
}

/// A node whose router calls 192.168.1.102 "`LGwebOSTV`".
fn node_with_router_tv() -> Node {
    let n = Node::new(telltale_config::Config::default());
    n.src
        .pipeline
        .router_leases
        .store(table(&[lease([192, 168, 1, 102], "LGwebOSTV")]));
    n
}

fn rule(devices: &[&str]) -> serde_json::Value {
    serde_json::json!({ "action": "block", "domain": "scan.example.com", "devices": devices })
}

/// The sentence every refusal's hint ends with.
const WHAT_TO_DO: &str =
    "Use the address, or name the device first (Clients → Name this device…) so rules follow it.";

fn text<'a>(v: &'a serde_json::Value, field: &str) -> &'a str {
    v[field].as_str().unwrap_or("")
}

/// REQ: FLT-005 (issue #73) — the form's old pre-fill: the name a router gives an address is
/// refused (it is no device named in TelltaleDNS), with words that say so, and a hint that says
/// which address the router means and what to do. Nothing is stored, in any case of the name.
#[tokio::test]
async fn flt_005_rule_router_name_refused_with_hint() {
    let n = node_with_router_tv();
    for name in ["LGwebOSTV", "lgwebostv"] {
        let (s, v) = n
            .put("/api/v1/rules/x", &n.admin, false, rule(&[name]))
            .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {v}");
        assert_eq!(v["code"], "invalid_config", "{v}");
        assert!(
            text(&v, "detail").contains(&format!(
                "`{name}` isn't a named device (`[[client]]`), an IP, or a CIDR"
            )),
            "the refusal says what a rule can name: {v}"
        );
        assert_eq!(
            text(&v, "hint"),
            format!("`{name}` is what your router calls 192.168.1.102. {WHAT_TO_DO}"),
            "{v}"
        );
    }
    assert!(
        crate::managed::entries(n.auth.state()).rules.is_empty(),
        "nothing was stored"
    );
}

/// REQ: FLT-005 (issue #73) — a string no table knows still gets the clear text and the hint
/// without an address; a rule refused for another reason (an unknown group) gets no device hint;
/// and with one good device and one bad, only the bad one is named.
#[tokio::test]
async fn flt_005_rule_unknown_device_refused_with_the_plain_hint() {
    let n = node_with_router_tv();
    let (s, v) = n
        .put("/api/v1/rules/x", &n.admin, false, rule(&["fridge"]))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(
        text(&v, "detail")
            .contains("`fridge` isn't a named device (`[[client]]`), an IP, or a CIDR"),
        "{v}"
    );
    assert_eq!(text(&v, "hint"), WHAT_TO_DO, "no address to name: {v}");

    let (s, v) = n
        .put(
            "/api/v1/rules/x",
            &n.admin,
            false,
            rule(&["192.168.1.20", "fridge"]),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(
        text(&v, "detail").contains("devices[1]"),
        "the bad one: {v}"
    );
    assert!(!text(&v, "detail").contains("devices[0]"), "{v}");

    let (s, v) = n
        .put(
            "/api/v1/rules/x",
            &n.admin,
            false,
            serde_json::json!({ "action": "block", "domain": "scan.example.com", "groups": ["nope"] }),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(text(&v, "detail").contains("unknown group `nope`"), "{v}");
    assert!(
        v.get("hint").is_none_or(serde_json::Value::is_null),
        "a group mistake has no device hint: {v}"
    );
}

/// REQ: FLT-005 (issue #73) — the hint also reads mDNS and the names other nodes know (in the order
/// the query log uses: this node's router, then mDNS, then the peers'), says an announced name
/// belongs to the device itself, and, when several addresses have the name, lists them rather than
/// choosing one.
#[tokio::test]
async fn flt_005_rule_hint_reads_mdns_peers_and_several_matches() {
    let n = Node::new(telltale_config::Config::default());
    n.src
        .pipeline
        .mdns_names
        .store(table(&[lease([192, 168, 1, 30], "printer")]));
    let mut peers = crate::devices::Names::default();
    peers.routers.insert(
        Ipv4Addr::new(192, 168, 1, 40),
        lease([192, 168, 1, 40], "nas"),
    );
    peers.mdns.insert(
        Ipv4Addr::new(192, 168, 1, 50),
        lease([192, 168, 1, 50], "speaker"),
    );
    n.src.pipeline.peer_names.store(Arc::new(peers));
    for (name, says) in [
        ("printer", "`printer` is what 192.168.1.30 calls itself."),
        ("nas", "`nas` is what your router calls 192.168.1.40."),
        ("speaker", "`speaker` is what 192.168.1.50 calls itself."),
    ] {
        let (s, v) = n
            .put("/api/v1/rules/x", &n.admin, true, rule(&[name]))
            .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {v}");
        assert_eq!(text(&v, "hint"), format!("{says} {WHAT_TO_DO}"), "{v}");
    }

    // Two televisions with one name (and the router and mDNS agreeing about one of them).
    n.src.pipeline.router_leases.store(table(&[
        lease([192, 168, 1, 102], "LGwebOSTV"),
        lease([192, 168, 1, 140], "LGwebOSTV"),
    ]));
    n.src
        .pipeline
        .mdns_names
        .store(table(&[lease([192, 168, 1, 102], "LGwebOSTV")]));
    let (s, v) = n
        .put("/api/v1/rules/x", &n.admin, true, rule(&["LGwebOSTV"]))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(
        text(&v, "hint"),
        format!(
            "`LGwebOSTV` is the name of more than one device here (192.168.1.102, 192.168.1.140), so pick the address of the one you mean. {WHAT_TO_DO}"
        ),
        "{v}"
    );
}

/// REQ: FLT-005 (issue #73) — "What would this have done?" and the pre-save check are dry runs:
/// they refuse a string with the same words and the same hint as the save, and store nothing.
#[tokio::test]
async fn flt_005_rule_dry_run_refuses_with_the_same_message() {
    let n = node_with_router_tv();
    let (s_dry, dry) = n
        .put("/api/v1/rules/x", &n.admin, true, rule(&["LGwebOSTV"]))
        .await;
    let (s, saved) = n
        .put("/api/v1/rules/x", &n.admin, false, rule(&["LGwebOSTV"]))
        .await;
    assert_eq!(s_dry, StatusCode::UNPROCESSABLE_ENTITY, "{dry}");
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{saved}");
    assert_eq!(dry["detail"], saved["detail"]);
    assert_eq!(dry["hint"], saved["hint"]);
    assert!(text(&dry, "hint").contains("192.168.1.102"), "{dry}");
    // The words are the backend's, whoever asks.
    let (s, v) = n
        .put("/api/v1/rules/x", &n.operator, true, rule(&["LGwebOSTV"]))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["hint"], dry["hint"]);
}

/// REQ: FLT-005 (issue #73) — what a rule accepts doesn't change: the name of a configured device
/// in any case, an IP, a CIDR. A router's name is refused until the device is named that, and
/// then it is the device's name like any other.
#[tokio::test]
async fn flt_005_rule_named_device_any_case_accepted() {
    let n = node_with_router_tv();
    let (s, v) = n
        .put(
            "/api/v1/clients/Living%20Room%20TV",
            &n.admin,
            false,
            serde_json::json!({ "match": ["192.168.1.20"] }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    for device in [
        "Living Room TV",
        "living room tv",
        "LIVING ROOM TV",
        "192.168.1.20",
        "192.168.1.0/24",
        "fd00::1",
    ] {
        let (s, v) = n
            .put("/api/v1/rules/ok", &n.admin, false, rule(&[device]))
            .await;
        assert_eq!(s, StatusCode::OK, "{device}: {v}");
    }
    let (s, v) = n
        .put("/api/v1/rules/ok", &n.admin, false, rule(&["LGwebOSTV"]))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "not named yet: {v}");
    // Naming the device (what the hint says to do) makes its name a target, in any case.
    let (s, v) = n
        .put(
            "/api/v1/clients/LGwebOSTV",
            &n.admin,
            false,
            serde_json::json!({ "match": ["192.168.1.102"] }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    for device in ["LGwebOSTV", "lgwebostv"] {
        let (s, v) = n
            .put("/api/v1/rules/ok", &n.admin, false, rule(&[device]))
            .await;
        assert_eq!(s, StatusCode::OK, "{device}: {v}");
    }
}
