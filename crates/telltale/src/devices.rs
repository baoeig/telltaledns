//! Names devices have outside TelltaleDNS (REQ: API-010; T8.2, T8.3): the DHCP clients of
//! the routers (`[[router]]`) and the names devices announce over mDNS, by IPv4 address.
//! TelltaleDNS doesn't hand out addresses itself (ADR-091): the router's DHCP does.

use std::collections::{HashMap, HashSet};
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
    /// REQ: FLT-006, CLU-010 (ADR-183) — written (true) by a node that marks its router bindings
    /// the way ADR-183 says (`unsure`, silence, one MAC at several addresses): another node
    /// takes MAC bindings only from an answer that carries it. An answer from an older node
    /// has no such field, so its lists name devices but identify none. Not written when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) vetted: bool,
}

/// The cluster call that asks a peer for its own device names.
pub(crate) const KIND: &str = "devices.names";
/// How often a node asks its peers (routers are read every few minutes).
const EVERY: std::time::Duration = std::time::Duration::from_secs(60);
/// How long a peer has to answer.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
/// REQ: CLU-002 (ADR-183) — until a full member has answered, a node asks again this often,
/// for at most [`RETRY_FOR_S`] seconds after it started, so a new pod or a restarted node
/// doesn't wait a minute for the first answer.
const RETRY: std::time::Duration = std::time::Duration::from_secs(5);
/// How long after start the quick retry lasts.
const RETRY_FOR_S: u64 = 120;
/// REQ: FLT-006, CLU-002 (ADR-183) — how long a member's last answer keeps identifying devices
/// here (monotonic seconds). With the router's own 15-minute bound, a stale binding lives about
/// 30 minutes plus one exchange: a node passes on only what its own routers said.
pub(crate) const PEER_MACS_FOR_S: u64 = 900;

/// What this node answers: only the names it learned itself, never ones it got from peers
/// (a name a router forgot doesn't keep circulating between nodes). REQ: FLT-006 (ADR-183) —
/// marked `vetted`: its router bindings follow the rules of ADR-183.
pub(crate) fn own(pipeline: &crate::pipeline::Pipeline) -> Result<Vec<u8>, String> {
    let names = Names {
        routers: (**pipeline.router_leases.load()).clone(),
        mdns: (**pipeline.mdns_names.load()).clone(),
        vetted: true,
    };
    serde_json::to_vec(&names).map_err(|e| e.to_string())
}

/// REQ: FLT-006, CLU-002 (ADR-183) — what the other full members' routers say, as this node uses it
/// to identify devices: an address to a MAC they agree on, the addresses any of them marks
/// `unsure` or that two of them disagree on (tombstones: no lower source fills them), and how many
/// that is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PeerMacs {
    pub(crate) macs: HashMap<Ipv4Addr, [u8; 6]>,
    pub(crate) tombstones: HashSet<Ipv4Addr>,
    pub(crate) dropped: usize,
}

/// Each full member's last vetted answer, as the bindings it gave (an address to `Some(mac)`, or
/// `None` for an `unsure` lease) and when it came (monotonic seconds).
pub(crate) type MemberBindings = HashMap<String, (HashMap<Ipv4Addr, Option<[u8; 6]>>, u64)>;

/// REQ: FLT-006, CLU-002, CLU-010 (ADR-183) — keeps, per member, the bindings of its last answer
/// at `now`: only from answers that carry `vetted` and come from a full member (`full`: in the
/// registry and not ephemeral), as `eligible` gives them, with `None` for a connected `unsure`
/// lease (offline ones are skipped). Then forgets every member that is no longer full or hasn't
/// answered for more than [`PEER_MACS_FOR_S`] seconds. Saturating: a time in the future is not
/// old.
pub(crate) fn keep_answers(
    last: &mut MemberBindings,
    answers: &[(String, Names)],
    now: u64,
    full: impl Fn(&str) -> bool,
) {
    for (peer, names) in answers {
        if !names.vetted || !full(peer) {
            continue;
        }
        let bindings = names
            .routers
            .iter()
            .filter_map(|(ip, l)| {
                if l.offline {
                    None
                } else if l.unsure {
                    Some((*ip, None))
                } else {
                    eligible(l).map(|mac| (*ip, Some(mac)))
                }
            })
            .collect();
        last.insert(peer.clone(), (bindings, now));
    }
    last.retain(|peer, (_, at)| full(peer) && now.saturating_sub(*at) <= PEER_MACS_FOR_S);
}

/// REQ: FLT-006, CLU-002 (ADR-183) — the bindings the members agree on: an address that any
/// member gives as `None`, or that two members give different MACs for, is a tombstone and is
/// counted in `dropped`. The result doesn't depend on the order of the members.
pub(crate) fn peer_macs(last: &MemberBindings) -> PeerMacs {
    let mut agreed: HashMap<Ipv4Addr, [u8; 6]> = HashMap::new();
    let mut tombstones: HashSet<Ipv4Addr> = HashSet::new();
    for (bindings, _) in last.values() {
        for (ip, mac) in bindings {
            match (mac, agreed.get(ip)) {
                (None, _) => {
                    tombstones.insert(*ip);
                }
                (Some(m), Some(other)) if other != m => {
                    tombstones.insert(*ip);
                }
                (Some(m), None) => {
                    agreed.insert(*ip, *m);
                }
                (Some(_), Some(_)) => {}
            }
        }
    }
    agreed.retain(|ip, _| !tombstones.contains(ip));
    PeerMacs {
        macs: agreed,
        dropped: tombstones.len(),
        tombstones,
    }
}

/// REQ: FLT-006, CLU-002 (ADR-183) — the MAC bindings that identify a device on this node, from
/// this node's routers and the members' (see [`router_bindings`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Bindings {
    /// This node's routers' sure bindings.
    pub(crate) own: HashMap<Ipv4Addr, [u8; 6]>,
    /// The members' bindings, for addresses this node has no binding or tombstone for.
    pub(crate) peer: HashMap<Ipv4Addr, [u8; 6]>,
    /// Addresses no router or member entry fills: this node's `unsure` ones and the members'.
    pub(crate) tombstones: HashSet<Ipv4Addr>,
    /// The addresses of a MAC that sits at two or more addresses, with that MAC: it identifies
    /// a device at none of them.
    pub(crate) shared: HashMap<Ipv4Addr, [u8; 6]>,
    /// How many bindings were withheld: this node's `unsure` leases, the addresses the members
    /// dropped, and the shared ones (the metric `telltale_router_macs_unsure`).
    pub(crate) unsure: usize,
}

/// REQ: FLT-006, CLU-002 (ADR-183) — what this node's routers (`own`) and the other full members'
/// (`peers`) say about which MAC is at an address, as far as identifying a device goes:
/// 1. this node's sure bindings (`eligible`); a connected `unsure` lease is a tombstone;
/// 2. the members' bindings, for addresses that are neither this node's binding nor a tombstone
///    (this node's or the members');
/// 3. then every MAC at two or more addresses over both moves its addresses to `shared`.
pub(crate) fn router_bindings(own: &Leases, peers: &PeerMacs) -> Bindings {
    let mut b = Bindings::default();
    let mut own_unsure = 0;
    for (ip, l) in own {
        if l.offline {
            continue;
        }
        if l.unsure {
            b.tombstones.insert(*ip);
            own_unsure += 1;
        } else if let Some(mac) = eligible(l) {
            b.own.insert(*ip, mac);
        }
    }
    b.tombstones.extend(peers.tombstones.iter().copied());
    for (ip, mac) in &peers.macs {
        if !b.own.contains_key(ip) && !b.tombstones.contains(ip) {
            b.peer.insert(*ip, *mac);
        }
    }
    let mut at: HashMap<[u8; 6], usize> = HashMap::new();
    for mac in b.own.values().chain(b.peer.values()) {
        *at.entry(*mac).or_default() += 1;
    }
    let mut shared = HashMap::new();
    let mut sure = |map: &mut HashMap<Ipv4Addr, [u8; 6]>| {
        map.retain(|ip, mac| {
            let alone = at.get(mac).is_none_or(|n| *n < 2);
            if !alone {
                shared.insert(*ip, *mac);
            }
            alone
        });
    };
    sure(&mut b.own);
    sure(&mut b.peer);
    b.unsure = own_unsure + peers.dropped + shared.len();
    b.shared = shared;
    b
}

/// REQ: FLT-006 (ADR-183) — the MAC table: the kernel's entries first, then this node's routers',
/// then the members'. `None`: only the kernel's (no device is named by its MAC, or
/// `router_macs` is off), and nothing of the routers is built.
pub(crate) fn build_table(
    kernel: &[(std::net::IpAddr, [u8; 6])],
    bindings: Option<&Bindings>,
) -> telltale_policy::MacTable {
    let v4 = |m: &HashMap<Ipv4Addr, [u8; 6]>| -> Vec<(std::net::IpAddr, [u8; 6])> {
        m.iter()
            .map(|(ip, mac)| (std::net::IpAddr::V4(*ip), *mac))
            .collect()
    };
    match bindings {
        None => telltale_policy::MacTable::from_sources(
            kernel.iter().copied(),
            std::iter::empty(),
            std::iter::empty(),
            0,
        ),
        Some(b) => telltale_policy::MacTable::from_sources(
            kernel.iter().copied(),
            v4(&b.own),
            v4(&b.peer),
            b.unsure,
        ),
    }
}

/// REQ: CLU-002 (ADR-183) — whether the answers of this round end the quick retry: a full member's
/// answer that is `vetted` and carries router leases (it reads a router: its bindings are what
/// the retry is for), or the reachable primary's answer, whatever it holds. A full replica with
/// no router key answers first in some clusters, and its empty answer must not end the retry.
pub(crate) fn answered_now(
    answers: &[(String, Names)],
    full: impl Fn(&str) -> bool,
    primary: Option<&str>,
) -> bool {
    answers
        .iter()
        .any(|(p, n)| (full(p) && n.vetted && !n.routers.is_empty()) || primary == Some(p.as_str()))
}

/// REQ: CLU-002 (ADR-183) — how long to wait before asking the members again: [`RETRY`] while no
/// full member has answered and the node is under [`RETRY_FOR_S`] seconds old, else a minute.
pub(crate) fn next_wait(full_member_answered: bool, since_start_s: u64) -> std::time::Duration {
    if !full_member_answered && since_start_s < RETRY_FOR_S {
        RETRY
    } else {
        EVERY
    }
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
/// none answer the last table stays. REQ: CLU-002, FLT-006 (ADR-183) — until a full member has
/// answered, and for the first two minutes at most, every 5 seconds; and each round, answered
/// or not, the members' MAC bindings are brought up to date (a silent member's age out).
pub(crate) async fn run(
    sources: std::sync::Arc<crate::http::Sources>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let Some(cluster) = sources.cluster.clone() else {
        return;
    };
    // Time for the bindings is this task's monotonic clock, never the wall clock.
    let started = std::time::Instant::now();
    let mut last = MemberBindings::new();
    let mut answered = false;
    let mut wait = std::time::Duration::ZERO;
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            () = tokio::time::sleep(wait) => {}
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
        // A full member: in the registry and not ephemeral (as `rpc_handler` decides). The
        // registry is a small file: read off the async task.
        let registry = {
            let c = std::sync::Arc::clone(&cluster);
            tokio::task::spawn_blocking(move || c.identity.registry())
                .await
                .unwrap_or_default()
        };
        let full = |p: &str| registry.iter().any(|n| n.node_id == p && !n.ephemeral);
        let primary = cluster.reachable_primary();
        answered |= answered_now(&answers, full, primary.as_deref());
        let now = started.elapsed().as_secs();
        keep_answers(&mut last, &answers, now, full);
        let macs = peer_macs(&last);
        if **sources.pipeline.peer_router_macs.load() != macs {
            sources
                .pipeline
                .peer_router_macs
                .store(std::sync::Arc::new(macs));
        }
        wait = next_wait(answered, now);
        if answers.is_empty() {
            continue;
        }
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
            vetted: false,
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

    const MAC_A: [u8; 6] = [0xaa, 0xbb, 0xcc, 0x00, 0x00, 0x01];
    const MAC_B: [u8; 6] = [0xaa, 0xbb, 0xcc, 0x00, 0x00, 0x02];

    fn at(last: u8) -> Ipv4Addr {
        Ipv4Addr::new(10, 0, 0, last)
    }

    /// A connected client at 10.0.0.`last` with `mac`.
    fn bound(last: u8, mac: &str, unsure: bool) -> Lease {
        Lease {
            mac: mac.into(),
            unsure,
            ..lease([10, 0, 0, last], "x")
        }
    }

    /// A node's answer: its router leases, marked `vetted`.
    fn vetted(leases: &[Lease]) -> Names {
        Names {
            vetted: true,
            ..names(leases, &[])
        }
    }

    fn from_nodes(answers: Vec<(&str, Names)>) -> Vec<(String, Names)> {
        answers
            .into_iter()
            .map(|(p, n)| (p.to_owned(), n))
            .collect()
    }

    fn members(last: &MemberBindings) -> Vec<&str> {
        let mut m: Vec<&str> = last.keys().map(String::as_str).collect();
        m.sort_unstable();
        m
    }

    // REQ: FLT-006, CLU-002 (ADR-183) — a member's bindings count only from a full member (the
    // registry lists it, it isn't ephemeral): a resolver pod or a node the registry doesn't list
    // can send what it likes, and gives none. Its mDNS names, its offline clients and a lease
    // whose MAC can't identify give none either.
    #[test]
    fn clu_002_peer_router_macs_come_only_from_full_members() {
        let full = |p: &str| p == "pi" || p == "k8s";
        let mut last = MemberBindings::new();
        let answers = from_nodes(vec![
            (
                "pi",
                vetted(&[
                    bound(5, "aa:bb:cc:00:00:01", false),
                    bound(6, "AA-BB-CC-00-00-02", false),
                ]),
            ),
            ("pod-1", vetted(&[bound(7, "aa:bb:cc:00:00:03", false)])),
            ("ghost", vetted(&[bound(8, "aa:bb:cc:00:00:04", false)])),
        ]);
        keep_answers(&mut last, &answers, 10, full);
        assert_eq!(members(&last), ["pi"]);
        let m = peer_macs(&last);
        assert_eq!(m.macs, HashMap::from([(at(5), MAC_A), (at(6), MAC_B)]));
        assert!(m.tombstones.is_empty());
        assert_eq!(m.dropped, 0);

        let mut n = vetted(&[bound(9, "aa:bb:cc:00:00:05", false)]);
        n.mdns = names(&[], &[bound(10, "aa:bb:cc:00:00:06", false)]).mdns;
        for l in [
            Lease {
                offline: true,
                ..bound(11, "aa:bb:cc:00:00:07", false)
            },
            bound(12, "", false),
            bound(13, "01:00:5e:00:00:01", false),
            bound(14, "00:00:00:00:00:00", false),
        ] {
            n.routers.insert(l.ip, l);
        }
        keep_answers(&mut last, &from_nodes(vec![("pi", n)]), 20, full);
        let m = peer_macs(&last);
        assert_eq!(
            m.macs,
            HashMap::from([(at(9), [0xaa, 0xbb, 0xcc, 0x00, 0x00, 0x05])])
        );
        assert!(m.tombstones.is_empty(), "no binding is not a tombstone");
    }

    // REQ: FLT-006, CLU-002 (ADR-183) — a member that isn't sure of an address (its lease is
    // `unsure`) withdraws it for this node as well: the other members' binding for it is dropped
    // and the address is a tombstone, so no lower source fills it.
    #[test]
    fn clu_002_a_peers_unsure_lease_counts_as_a_disagreement() {
        let full = |p: &str| matches!(p, "pi" | "k8s" | "nas");
        let mut last = MemberBindings::new();
        let answers = from_nodes(vec![
            (
                "pi",
                vetted(&[
                    bound(5, "aa:bb:cc:00:00:01", false),
                    bound(6, "aa:bb:cc:00:00:02", false),
                ]),
            ),
            (
                "k8s",
                vetted(&[
                    bound(5, "aa:bb:cc:00:00:01", true),
                    bound(7, "aa:bb:cc:00:00:01", false),
                ]),
            ),
        ]);
        keep_answers(&mut last, &answers, 10, full);
        let m = peer_macs(&last);
        assert!(!m.macs.contains_key(&at(5)), "{m:?}");
        assert_eq!(m.tombstones, HashSet::from([at(5)]));
        assert_eq!(m.dropped, 1);
        assert_eq!(m.macs[&at(6)], MAC_B);
        assert_eq!(m.macs[&at(7)], MAC_A);

        // An unsure lease with no other member's binding is a tombstone all the same.
        let mut alone = MemberBindings::new();
        keep_answers(
            &mut alone,
            &from_nodes(vec![(
                "nas",
                vetted(&[bound(9, "aa:bb:cc:00:00:01", true)]),
            )]),
            10,
            full,
        );
        let m = peer_macs(&alone);
        assert!(m.macs.is_empty());
        assert_eq!((m.tombstones, m.dropped), (HashSet::from([at(9)]), 1));
    }

    // REQ: FLT-006, CLU-002 (ADR-183) — two members that give an address different MACs both
    // lose it; one they agree on stays. What comes out doesn't depend on which answered first.
    #[test]
    fn clu_002_peers_disagreeing_on_an_address_drop_it() {
        let full = |_: &str| true;
        let pi = vetted(&[
            bound(5, "aa:bb:cc:00:00:01", false),
            bound(6, "aa:bb:cc:00:00:01", false),
        ]);
        let k8s = vetted(&[
            bound(5, "aa:bb:cc:00:00:02", false),
            bound(6, "aa-bb-cc-00-00-01", false),
        ]);
        let mut results = Vec::new();
        for order in [["pi", "k8s"], ["k8s", "pi"]] {
            let mut last = MemberBindings::new();
            for (i, who) in order.iter().enumerate() {
                let names = if *who == "pi" {
                    pi.clone()
                } else {
                    k8s.clone()
                };
                keep_answers(
                    &mut last,
                    &from_nodes(vec![(*who, names)]),
                    10 + i as u64,
                    full,
                );
            }
            results.push(peer_macs(&last));
        }
        assert_eq!(results[0], results[1]);
        let m = &results[0];
        assert!(!m.macs.contains_key(&at(5)));
        assert_eq!(m.tombstones, HashSet::from([at(5)]));
        assert_eq!(m.dropped, 1);
        assert_eq!(
            m.macs[&at(6)],
            MAC_A,
            "the same MAC written two ways is agreement"
        );
    }

    // REQ: FLT-006, CLU-002 (ADR-183) — a member's last answer is used for 900 seconds (on the
    // monotonic clock), then forgotten; at once when the member leaves the registry; an answer
    // renews it; a time in the future (a clock that stepped) is not old, and nothing panics.
    #[test]
    fn clu_002_a_silent_member_is_dropped_after_15_minutes() {
        let full = |p: &str| p == "pi";
        let mut last = MemberBindings::new();
        let answer = from_nodes(vec![(
            "pi",
            vetted(&[bound(5, "aa:bb:cc:00:00:01", false)]),
        )]);
        keep_answers(&mut last, &answer, 100, full);
        for (now, kept) in [
            (100, true),
            (100 + PEER_MACS_FOR_S - 1, true),
            (100 + PEER_MACS_FOR_S, true),
            (100 + PEER_MACS_FOR_S + 1, false),
            (u64::MAX, false),
        ] {
            let mut l = last.clone();
            keep_answers(&mut l, &[], now, full);
            assert_eq!(!l.is_empty(), kept, "at {now}");
            assert_eq!(peer_macs(&l).macs.len(), usize::from(kept), "at {now}");
        }
        let mut l = last.clone();
        keep_answers(&mut l, &[], 101, |_| false);
        assert!(l.is_empty(), "a member the registry no longer lists");
        let mut l = last.clone();
        keep_answers(&mut l, &[], 5, full);
        assert!(!l.is_empty(), "now before the answer: not old, no panic");
        // A new answer renews the time.
        let mut l = last.clone();
        keep_answers(&mut l, &answer, 1_000, full);
        keep_answers(&mut l, &[], 1_000 + PEER_MACS_FOR_S, full);
        assert!(!l.is_empty());
        keep_answers(&mut l, &[], 1_000 + PEER_MACS_FOR_S + 1, full);
        assert!(l.is_empty());
        // An answer that isn't vetted doesn't renew it either.
        let mut l = last.clone();
        let unvetted = from_nodes(vec![(
            "pi",
            names(&[bound(5, "aa:bb:cc:00:00:01", false)], &[]),
        )]);
        keep_answers(&mut l, &unvetted, 500, full);
        keep_answers(&mut l, &[], 100 + PEER_MACS_FOR_S + 1, full);
        assert!(l.is_empty());
    }

    // REQ: FLT-006, CLU-010 (ADR-183) — an answer from a member on an earlier build (no `vetted`)
    // gives no bindings, since its lists have none of the rules, but it still names devices (the
    // display merge is unchanged). `vetted` is written only when true: the form is the old one
    // plus that field, which an older node ignores, and this node's own answer carries it.
    #[test]
    fn clu_010_bindings_from_an_older_member_are_not_used() {
        // What the previous version's answer looks like: the same fields, none for `vetted`.
        #[derive(Debug, Deserialize)]
        struct PreviousNames {
            routers: Leases,
            mdns: Leases,
        }
        let older = r#"{"routers":{"10.0.0.5":{"mac":"aa:bb:cc:00:00:01","ip":"10.0.0.5","hostname":"desk","expires":0}},"mdns":{}}"#;
        let n: Names = serde_json::from_str(older).unwrap();
        assert!(!n.vetted);
        let answers = vec![("pi".to_owned(), n)];
        let mut last = MemberBindings::new();
        keep_answers(&mut last, &answers, 10, |_| true);
        assert!(last.is_empty());
        assert_eq!(peer_macs(&last), PeerMacs::default());
        let merged = merge(answers, None);
        assert_eq!(
            merged.routers[&at(5)].hostname.as_deref(),
            Some("desk"),
            "display names are unchanged"
        );

        // The wire form.
        let plain = names(&[bound(5, "aa:bb:cc:00:00:01", false)], &[]);
        let text = serde_json::to_string(&plain).unwrap();
        assert!(!text.contains("vetted"), "{text}");
        let marked = Names {
            vetted: true,
            ..plain.clone()
        };
        let marked_text = serde_json::to_string(&marked).unwrap();
        assert!(marked_text.contains("\"vetted\":true"), "{marked_text}");
        let mut with_field: serde_json::Value = serde_json::from_str(&marked_text).unwrap();
        with_field.as_object_mut().unwrap().remove("vetted");
        assert_eq!(
            with_field,
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            "the old form plus the one field"
        );
        let prev: PreviousNames = serde_json::from_str(&marked_text).unwrap();
        assert_eq!((prev.routers.len(), prev.mdns.len()), (1, 0));
        assert!(serde_json::from_str::<Names>(&marked_text).unwrap().vetted);

        // This node's own answer carries it (and keeps `unsure`).
        let p = crate::pipeline::tests_support::pipeline_from(&telltale_config::Config::default());
        p.router_leases.store(std::sync::Arc::new(HashMap::from([(
            at(6),
            bound(6, "aa:bb:cc:00:00:02", true),
        )])));
        let mine: Names = serde_json::from_slice(&own(&p).unwrap()).unwrap();
        assert!(mine.vetted);
        assert!(mine.routers[&at(6)].unsure);
    }

    // REQ: CLU-002 (ADR-183) — until a full member has answered, a node asks again every 5
    // seconds for the first two minutes (a new pod or a restarted node doesn't wait a minute);
    // after that, or once one has answered, every minute.
    #[test]
    fn clu_002_names_are_asked_again_soon_until_a_full_member_answers() {
        use std::time::Duration;
        let soon = Duration::from_secs(5);
        let minute = Duration::from_secs(60);
        assert_eq!(next_wait(false, 0), soon);
        assert_eq!(next_wait(false, 119), soon);
        assert_eq!(next_wait(false, 120), minute, "the retry is bounded");
        assert_eq!(next_wait(false, u64::MAX), minute);
        assert_eq!(next_wait(true, 0), minute);
        assert_eq!(next_wait(true, 60), minute);
    }

    // REQ: CLU-002 (ADR-183) — the quick retry isn't ended by a full replica that holds no router
    // key and answers first (its empty answer carries nothing the pods need), nor by an older
    // member's answer or an ephemeral member's: only a full member's vetted answer with router
    // leases, or the reachable primary's answer, ends it.
    #[test]
    fn clu_002_the_quick_retry_waits_for_an_answer_that_carries_routers() {
        use std::time::Duration;
        let full = |p: &str| matches!(p, "replica" | "primary" | "old");
        let leased = |last: u8| bound(last, "aa:bb:cc:00:00:01", false);
        let replica_no_key = vetted(&[]);
        let with_router = vetted(&[leased(5)]);
        let older = names(&[leased(5)], &[]);
        let ask = |a: Vec<(&str, Names)>, primary: Option<&str>| {
            answered_now(&from_nodes(a), full, primary)
        };
        // A replica without the router key, vetted and empty: not yet.
        assert!(!ask(vec![("replica", replica_no_key.clone())], None));
        assert!(!ask(
            vec![("replica", replica_no_key.clone())],
            Some("primary")
        ));
        // A vetted answer with router leases from a full member: done.
        assert!(ask(vec![("primary", with_router.clone())], None));
        assert!(ask(
            vec![
                ("replica", replica_no_key.clone()),
                ("primary", with_router.clone())
            ],
            None
        ));
        // An older member (no `vetted`), an ephemeral pod or an unlisted node, with leases: not done.
        assert!(!ask(vec![("old", older)], None));
        assert!(!ask(vec![("pod-1", with_router.clone())], None));
        assert!(!ask(vec![("ghost", with_router)], None));
        // The primary's answer, whatever it holds, ends it (it is the node that reads the router).
        assert!(ask(
            vec![("primary", replica_no_key.clone())],
            Some("primary")
        ));
        assert!(!ask(vec![("replica", replica_no_key)], Some("primary")));
        assert!(!ask(vec![], Some("primary")), "nobody answered");
        // The whole story, with `next_wait`: the replica answers at 5 s, the primary at 10 s.
        let mut answered = false;
        answered |= ask(vec![("replica", vetted(&[]))], Some("primary"));
        assert_eq!(next_wait(answered, 5), Duration::from_secs(5));
        answered |= ask(vec![("primary", vetted(&[leased(5)]))], Some("primary"));
        assert_eq!(next_wait(answered, 10), Duration::from_secs(60));
    }
}
