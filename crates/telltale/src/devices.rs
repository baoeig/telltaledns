//! Names devices have outside TelltaleDNS (REQ: API-010; T8.2, T8.3): the DHCP clients of
//! the routers (`[[router]]`) and the names devices announce over mDNS, by IPv4 address.
//! TelltaleDNS doesn't hand out addresses itself (ADR-091): the router's DHCP does.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

/// One device as a router or mDNS knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Lease {
    /// Empty when unknown (mDNS).
    pub(crate) mac: String,
    pub(crate) ip: Ipv4Addr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hostname: Option<String>,
    /// Unix seconds (0 when unknown).
    pub(crate) expires: u64,
    /// REQ: T8.2 (ADR-166) — a client the router remembers but doesn't see now (UniFi's offline
    /// clients): it names an address in the query log, but it isn't "seen", so device
    /// identification doesn't count it. Not written when false, so a node on an older build,
    /// which doesn't know the field, reads the same form as before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) offline: bool,
    /// REQ: FLT-006, CLU-002 (ADR-183) — a connected client whose binding of this address to this
    /// MAC can't be trusted to identify a device: the routers disagree on the address, one MAC
    /// sits at several addresses (a range extender), UniFi gave the address from its last-known
    /// one, or the router that said so hasn't answered for 15 minutes. It still names the
    /// address (display), but it never identifies, and no lower source fills the address.
    /// Not written when false, so a node on an older build, which doesn't know the field, reads
    /// the same form as before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) unsure: bool,
}

/// Devices by address.
pub(crate) type Leases = HashMap<Ipv4Addr, Lease>;

/// REQ: FLT-006 (ADR-183) — the MAC a lease can identify a device by: its client is connected
/// (not offline), its binding is not `unsure`, and the MAC parses (`aa:bb:..` or `aa-bb-..`),
/// isn't all zero, and is unicast. Locally administered ("private") MACs count. MACs are
/// compared as parsed bytes, never as text.
// The members' bindings (`keep_answers`) and the table (`router_bindings`) use it from the next
// commits on; until then only the tests do.
#[allow(dead_code)]
pub(crate) fn eligible(l: &Lease) -> Option<[u8; 6]> {
    if l.offline || l.unsure {
        return None;
    }
    let mac = crate::identify::oui::parse_mac(&l.mac)?;
    (mac != [0; 6] && mac[0] & 1 == 0).then_some(mac)
}

/// REQ: CLU-002, API-010 (ADR-125) — the device names other nodes know: the ones their
/// routers and mDNS saw. Only one node usually reads the router (the one holding its API
/// key), and a pod can't hear the LAN's mDNS, so without these a resolver pod's query-log
/// rows name a device by its address while the primary's rows name it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Names {
    pub(crate) routers: Leases,
    pub(crate) mdns: Leases,
}

/// The cluster call that asks a peer for its own device names.
pub(crate) const KIND: &str = "devices.names";
/// How often a node asks its peers (routers are read every few minutes).
const EVERY: std::time::Duration = std::time::Duration::from_secs(60);
/// How long a peer has to answer.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// What this node answers: only the names it learned itself, never ones it got from peers
/// (a name a router forgot doesn't keep circulating between nodes).
pub(crate) fn own(pipeline: &crate::pipeline::Pipeline) -> Result<Vec<u8>, String> {
    let names = Names {
        routers: (**pipeline.router_leases.load()).clone(),
        mdns: (**pipeline.mdns_names.load()).clone(),
    };
    serde_json::to_vec(&names).map_err(|e| e.to_string())
}

/// The peers' answers as one table: the primary's first, then by node ID; the first node to
/// know an address names it.
fn merge(mut answers: Vec<(String, Names)>, primary: Option<&str>) -> Names {
    answers.sort_by(|a, b| {
        (Some(a.0.as_str()) != primary)
            .cmp(&(Some(b.0.as_str()) != primary))
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut out = Names::default();
    for (_, n) in answers {
        for (ip, l) in n.routers {
            out.routers.entry(ip).or_insert(l);
        }
        for (ip, l) in n.mdns {
            out.mdns.entry(ip).or_insert(l);
        }
    }
    out
}

/// The task: every minute, ask every reachable peer for its device names. Off the DNS path;
/// a peer that doesn't answer (or is on an older build, "unknown call") is skipped, and when
/// none answer the last table stays.
pub(crate) async fn run(
    sources: std::sync::Arc<crate::http::Sources>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let Some(cluster) = sources.cluster.clone() else {
        return;
    };
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            _ = tick.tick() => {}
        }
        let answers: Vec<(String, Names)> = cluster
            .call_all(KIND, &[], DEADLINE)
            .await
            .into_iter()
            .filter_map(|(peer, r)| {
                let body = r.ok()?;
                serde_json::from_slice(&body).ok().map(|n| (peer, n))
            })
            .collect();
        if answers.is_empty() {
            continue;
        }
        let primary = cluster.reachable_primary();
        let names = merge(answers, primary.as_deref());
        if **sources.pipeline.peer_names.load() != names {
            sources
                .pipeline
                .peer_names
                .store(std::sync::Arc::new(names));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(ip: [u8; 4], name: &str) -> Lease {
        Lease {
            mac: String::new(),
            ip: Ipv4Addr::from(ip),
            hostname: Some(name.into()),
            expires: 0,
            offline: false,
            unsure: false,
        }
    }

    fn names(routers: &[Lease], mdns: &[Lease]) -> Names {
        Names {
            routers: routers.iter().map(|l| (l.ip, l.clone())).collect(),
            mdns: mdns.iter().map(|l| (l.ip, l.clone())).collect(),
        }
    }

    // REQ: CLU-002, API-010 — the primary's names win; others fill in what it doesn't know.
    #[test]
    fn clu_002_peer_names_merge_primary_first() {
        let a = names(&[lease([10, 0, 0, 2], "k8s-from-a")], &[]);
        let p = names(
            &[lease([10, 0, 0, 2], "k8s")],
            &[lease([10, 0, 0, 9], "printer")],
        );
        let z = names(&[lease([10, 0, 0, 3], "nas")], &[]);
        let m = merge(
            vec![("a".into(), a), ("z".into(), z), ("p".into(), p)],
            Some("p"),
        );
        let host =
            |t: &Leases, ip: [u8; 4]| t.get(&Ipv4Addr::from(ip)).and_then(|l| l.hostname.clone());
        assert_eq!(host(&m.routers, [10, 0, 0, 2]).as_deref(), Some("k8s"));
        assert_eq!(host(&m.routers, [10, 0, 0, 3]).as_deref(), Some("nas"));
        assert_eq!(host(&m.mdns, [10, 0, 0, 9]).as_deref(), Some("printer"));
    }

    // The wire form round-trips (addresses as map keys).
    #[test]
    fn clu_002_peer_names_wire_form() {
        let n = names(&[lease([192, 168, 5, 2], "k8s")], &[]);
        let back: Names = serde_json::from_slice(&serde_json::to_vec(&n).unwrap()).unwrap();
        assert_eq!(back, n);
    }

    // REQ: T8.2, CLU-002 (ADR-166) — the offline mark travels to peers that know it, and costs
    // nothing on the wire for a connected device: its form is what older nodes already read, and
    // an answer from an older node (no field) is a connected device.
    #[test]
    fn t8_2_offline_mark_is_compatible_on_the_wire() {
        let connected = serde_json::to_string(&lease([10, 0, 0, 2], "k8s")).unwrap();
        assert!(!connected.contains("offline"), "{connected}");
        let gone = Lease {
            offline: true,
            ..lease([10, 0, 0, 3], "old phone")
        };
        let text = serde_json::to_string(&gone).unwrap();
        assert!(text.contains("\"offline\":true"), "{text}");
        assert_eq!(serde_json::from_str::<Lease>(&text).unwrap(), gone);
        let older = r#"{"mac":"","ip":"10.0.0.2","hostname":"k8s","expires":0}"#;
        assert!(!serde_json::from_str::<Lease>(older).unwrap().offline);
    }

    // REQ: FLT-006, CLU-002, CLU-010 (ADR-183) — the unsure mark travels to nodes that know it and
    // costs nothing on the wire for a sure binding: its form is what older nodes already read; an
    // answer from an older node (no field) is a sure binding; an older node, which has no such
    // field, reads the new form and ignores it.
    #[test]
    fn flt_006_unsure_mark_is_compatible_on_the_wire() {
        // What the previous version's `Lease` looks like: the same fields, none for `unsure`.
        #[derive(Debug, PartialEq, Deserialize)]
        struct PreviousLease {
            mac: String,
            ip: Ipv4Addr,
            #[serde(default)]
            hostname: Option<String>,
            expires: u64,
            #[serde(default)]
            offline: bool,
        }
        let sure = serde_json::to_string(&lease([10, 0, 0, 2], "k8s")).unwrap();
        assert!(!sure.contains("unsure"), "{sure}");
        let doubtful = Lease {
            unsure: true,
            mac: "aa:bb:cc:00:00:01".into(),
            ..lease([10, 0, 0, 3], "tv")
        };
        let text = serde_json::to_string(&doubtful).unwrap();
        assert!(text.contains("\"unsure\":true"), "{text}");
        assert_eq!(serde_json::from_str::<Lease>(&text).unwrap(), doubtful);
        let older = r#"{"mac":"aa:bb:cc:00:00:01","ip":"10.0.0.2","hostname":"k8s","expires":0}"#;
        assert!(!serde_json::from_str::<Lease>(older).unwrap().unsure);
        let old: PreviousLease = serde_json::from_str(&text).unwrap();
        assert_eq!(
            old,
            PreviousLease {
                mac: "aa:bb:cc:00:00:01".into(),
                ip: Ipv4Addr::new(10, 0, 0, 3),
                hostname: Some("tv".into()),
                expires: 0,
                offline: false,
            }
        );
    }

    // REQ: FLT-006 (ADR-183) — only a connected client whose binding is sure, with a MAC that
    // parses (either separator, any case), isn't all zero and is unicast, can identify a device.
    // A locally administered ("private") MAC can.
    #[test]
    fn flt_006_offline_and_unsure_leases_never_identify() {
        let with = |mac: &str, offline: bool, unsure: bool| Lease {
            mac: mac.into(),
            offline,
            unsure,
            ..lease([10, 0, 0, 2], "x")
        };
        let desk = Some([0xaa, 0xbb, 0xcc, 0x00, 0x00, 0x01]);
        assert_eq!(eligible(&with("aa:bb:cc:00:00:01", false, false)), desk);
        assert_eq!(eligible(&with("AA-BB-CC-00-00-01", false, false)), desk);
        assert_eq!(
            eligible(&with("02:00:00:00:00:01", false, false)),
            Some([0x02, 0, 0, 0, 0, 0x01]),
            "a private MAC counts"
        );
        for (mac, offline, unsure, why) in [
            ("aa:bb:cc:00:00:01", true, false, "offline"),
            ("aa:bb:cc:00:00:01", false, true, "unsure"),
            ("aa:bb:cc:00:00:01", true, true, "both"),
            ("", false, false, "no MAC (mDNS)"),
            ("zz:bb:cc:00:00:01", false, false, "unparsable"),
            ("aa:bb:cc:00:01", false, false, "too short"),
            ("00:00:00:00:00:00", false, false, "all zero"),
            ("01:00:5e:00:00:01", false, false, "multicast"),
            ("ff:ff:ff:ff:ff:ff", false, false, "broadcast"),
        ] {
            assert_eq!(eligible(&with(mac, offline, unsure)), None, "{why}");
        }
    }
}
