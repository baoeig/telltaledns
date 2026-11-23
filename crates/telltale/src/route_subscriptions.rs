//! REQ: DNS-017, UPS-007, OPS-009 — refresh `AutoProxy` suffix routes off the DNS path.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use telltale_config::{Config, RouteSubscription};
use telltale_filter::fetch::{Client, Conditional, Response, Store, content_hash};
use telltale_filter::parse::normalize;
use telltale_proto::NameBuf;
use telltale_upstream::Router;
use tokio::task::JoinHandle;
use tracing::{info, warn};

const MAX_BYTES: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

fn source_name(subscription: &RouteSubscription) -> String {
    content_hash(subscription.url.as_bytes())
}

fn store(cfg: &Config) -> io::Result<Store> {
    Store::open(&cache_dir(cfg))
}

fn cache_dir(cfg: &Config) -> std::path::PathBuf {
    Path::new(cfg.node.data_dir.as_str()).join("route-subscriptions")
}

/// Load each last-good list on startup, reload, or an offline explain.
pub(crate) fn restore(cfg: &Config, router: &Router) {
    if cfg.route_subscription.is_empty() || !cache_dir(cfg).join("lists").is_dir() {
        return;
    }
    let store = match store(cfg) {
        Ok(store) => store,
        Err(e) => {
            warn!("route subscriptions: cache unavailable: {e}");
            return;
        }
    };
    for (index, subscription) in cfg.route_subscription.iter().enumerate() {
        match store.read_source(&source_name(subscription)) {
            Ok(data) => match parse(&data) {
                Ok(rules) => router.replace_subscription(index, rules),
                Err(e) => warn!(name = %subscription.name, "route subscription cache ignored: {e}"),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => warn!(name = %subscription.name, "route subscription cache unavailable: {e}"),
        }
    }
}

#[derive(Default)]
pub(crate) struct RefreshTasks(Vec<JoinHandle<()>>);

impl RefreshTasks {
    pub(crate) fn start(cfg: &Config, router: &Arc<Router>) -> Self {
        if cfg.route_subscription.is_empty() {
            return Self::default();
        }
        let listen: Vec<_> = cfg.listen.iter().map(|l| l.addr).collect();
        let result = store(cfg).map_err(|e| e.to_string()).and_then(|store| {
            let client = Client::new(Arc::new(crate::lists::ListResolver::new(&listen)), &[])?;
            Ok((store, client))
        });
        let (store, client) = match result {
            Ok(v) => v,
            Err(e) => {
                warn!("route subscriptions could not start: {e}");
                return Self::default();
            }
        };
        // Download each URL once, then publish its rules to all slots using that URL.
        let mut by_url = HashMap::<_, (RouteSubscription, Vec<usize>)>::new();
        for (index, subscription) in cfg.route_subscription.iter().enumerate() {
            let (source, indexes) = by_url
                .entry(subscription.url.as_str())
                .or_insert_with(|| (subscription.clone(), Vec::new()));
            source.refresh_secs = source.refresh_secs.min(subscription.refresh_secs);
            indexes.push(index);
        }
        let client = Arc::new(client);
        Self(
            by_url
                .into_values()
                .map(|(source, indexes)| {
                    tokio::spawn(refresh(
                        source,
                        indexes,
                        store.clone(),
                        Arc::clone(&client),
                        Arc::clone(router),
                    ))
                })
                .collect(),
        )
    }

    pub(crate) async fn stop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
        for task in self.0.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for RefreshTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

async fn refresh(
    source: RouteSubscription,
    indexes: Vec<usize>,
    store: Store,
    client: Arc<Client>,
    router: Arc<Router>,
) {
    let key = source_name(&source);
    let mut conditional = Conditional::default();
    let interval = Duration::from_secs(u64::from(source.refresh_secs));
    loop {
        let result = tokio::time::timeout(
            TIMEOUT,
            client.get(source.url.as_str(), &conditional, MAX_BYTES),
        )
        .await;
        let updated = match result {
            Ok(Ok(Response::NotModified)) => true,
            Ok(Ok(Response::Body {
                data,
                etag,
                last_modified,
            })) => match save(&store, &key, &data) {
                Ok(rules) => {
                    info!(name = %source.name, rules = rules.len(), "route subscription refreshed");
                    for &index in &indexes {
                        router.replace_subscription(index, rules.iter().map(|(&n, &v)| (n, v)));
                    }
                    conditional = Conditional {
                        etag,
                        last_modified,
                    };
                    true
                }
                Err(e) => {
                    warn!(name = %source.name, "route subscription unchanged: {e}");
                    false
                }
            },
            Ok(Err(e)) => {
                warn!(name = %source.name, "route subscription download failed: {}", e.message);
                false
            }
            Err(_) => {
                warn!(name = %source.name, "route subscription download timed out");
                false
            }
        };
        tokio::time::sleep(if updated {
            interval
        } else {
            Duration::from_secs(300)
        })
        .await;
    }
}

fn save(store: &Store, key: &str, data: &[u8]) -> Result<HashMap<NameBuf, bool>, String> {
    let rules = parse(data).map_err(str::to_owned)?;
    store.save_source(key, data).map_err(|e| e.to_string())?;
    Ok(rules)
}

/// `GFWList` is Base64-wrapped `AutoProxy`. Plain `AutoProxy` is also supported. DNS has only a
/// hostname, so paths and schemes cannot constrain a route. Skip regex, partial-label
/// wildcards and IP addresses. An exception wins even when it precedes the include.
fn parse(source: &[u8]) -> Result<HashMap<NameBuf, bool>, &'static str> {
    if source.len() as u64 > MAX_BYTES {
        return Err("route list is too large");
    }
    let decoded;
    let source = if source.starts_with(b"[AutoProxy ") || source.starts_with(b"\xef\xbb\xbf") {
        source
    } else {
        let encoded: Vec<u8> = source
            .iter()
            .copied()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "invalid Base64 route list")?;
        &decoded
    };
    let text = std::str::from_utf8(source).map_err(|_| "route list is not UTF-8")?;
    let mut lines = text.trim_start_matches('\u{feff}').lines();
    if !lines
        .next()
        .is_some_and(|line| line.starts_with("[AutoProxy ") && line.ends_with(']'))
    {
        return Err("missing AutoProxy header");
    }
    let mut rules = HashMap::new();
    for line in lines.map(str::trim) {
        let exception = line.strip_prefix("@@");
        if let Some(name) = rule_name(exception.unwrap_or(line)) {
            rules
                .entry(name)
                .and_modify(|forward| *forward &= exception.is_none())
                .or_insert_with(|| exception.is_none());
        }
    }
    if !rules.values().any(|forward| *forward) {
        return Err("no usable forwarding rules");
    }
    Ok(rules)
}

fn rule_name(rule: &str) -> Option<NameBuf> {
    if rule.starts_with(['!', '[', '/']) {
        return None;
    }
    let host = if let Some(domain) = rule
        .strip_prefix("||")
        .or_else(|| rule.strip_prefix("|https://"))
        .or_else(|| rule.strip_prefix("|http://"))
    {
        domain.split(['/', ':', '^', '?', '#', '|']).next()?
    } else if rule.contains('.') {
        rule
    } else {
        return None;
    };
    let host = normalize(host.strip_prefix("*.").unwrap_or(host)).ok()?;
    NameBuf::from_presentation(&host).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use telltale_config::SafeString;
    use telltale_upstream::Question;

    fn config(path: &Path) -> Config {
        let mut cfg = telltale_config::Loader::new()
            .toml_str(
                "test",
                r#"
[[upstream]]
name = "local"
url = "udp://127.0.0.1:10053"
[[upstream]]
name = "remote"
url = "udp://127.0.0.1:20053"
[[upstream_group]]
name = "default"
members = ["local"]
[[upstream_group]]
name = "overseas"
members = ["remote"]
[[route]]
match_suffix = ["custom.example"]
upstream_group = "default"
[[route_subscription]]
name = "gfwlist"
url = "https://lists.example/gfwlist.txt"
upstream_group = "overseas"
"#,
            )
            .load()
            .unwrap()
            .config;
        cfg.node.data_dir = SafeString::new(path.to_str().unwrap()).unwrap();
        cfg
    }

    fn group_for<'a>(router: &'a Router, name: &str) -> &'a str {
        &router
            .select::<&str>(
                &Question {
                    name: NameBuf::from_presentation(name).unwrap(),
                    qtype: 1,
                    qclass: 1,
                    dnssec_ok: false,
                    checking_disabled: false,
                    client_subnet: 0,
                },
                &[],
            )
            .unwrap()
            .group
            .name
    }

    #[test]
    fn dns_017_gfwlist_routes_and_keeps_the_last_good_list() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let key = source_name(&cfg.route_subscription[0]);
        let store = store(&cfg).unwrap();
        let source = b"[AutoProxy 0.2.9]\n\
            ! Comment\n\
            ||example.com\n\
            @@||keep.example.com\n\
            ||nested.keep.example.com\n\
            ||custom.example\n\
            |https://path.example/one-page\n\
            |http://*.wild.example/\n\
            ||img*.skip.example\n\
            /^https?:\\/\\/skip\\.example/\n\
            |http://1.2.3.4/\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(source);
        let wrapped = format!("{}\n{}\n", &encoded[..60], &encoded[60..]);
        let router = Router::from_config(&cfg).unwrap();
        assert_eq!(group_for(&router, "www.example.com"), "default");
        router.replace_subscription(0, save(&store, &key, wrapped.as_bytes()).unwrap());
        assert_eq!(group_for(&router, "WWW.EXAMPLE.COM"), "overseas");
        assert_eq!(group_for(&router, "notexample.com"), "default");
        assert_eq!(group_for(&router, "nested.keep.example.com"), "default");
        assert_eq!(group_for(&router, "custom.example"), "default");
        assert_eq!(group_for(&router, "path.example"), "overseas");
        assert_eq!(group_for(&router, "a.wild.example"), "overseas");
        assert_eq!(group_for(&router, "img1.skip.example"), "default");
        assert_eq!(group_for(&router, "1.2.3.4"), "default");
        assert!(save(&store, &key, b"[AutoProxy 0.2.9]\n! Empty\n").is_err());

        let restarted = Router::from_config(&cfg).unwrap();
        restore(&cfg, &restarted);
        assert_eq!(group_for(&restarted, "www.example.com"), "overseas");

        let mut changed = cfg;
        changed.route_subscription[0].url = "https://lists.example/new.txt".into();
        let restarted = Router::from_config(&changed).unwrap();
        restore(&changed, &restarted);
        assert_eq!(group_for(&restarted, "www.example.com"), "default");
    }
}
