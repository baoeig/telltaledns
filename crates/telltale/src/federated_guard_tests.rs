//! REQ: CLU-004 (ADR-174) — the guard behind "no federated read is made on a runtime worker".
//!
//! A federated read made on a worker thread of the multi-thread runtime costs its whole deadline
//! even when the runtime is healthy (`federated_bridge.rs`, "What this does not do"): the task it
//! starts lands in that worker's own LIFO slot, which no other worker steals, and the worker is the
//! one parked waiting for it. Every handler therefore runs the backend calls that can federate in
//! `blocking(..)` or `spawn_blocking(..)`, and nothing but this scan keeps the next handler from
//! forgetting: a call that was missed works, slowly, and answers from this node alone (on a replica,
//! with its own configuration version as the `ETag`).
//!
//! The scan, over the production code of `crates/telltale-api/src` and `crates/telltale/src`:
//!
//! 1. Which `Backend` methods can federate is derived, not listed: from `federated.rs` and
//!    `federated_support.rs`, the methods that reach `bridge::start`, by calls through `self`.
//! 2. Every method call `.name(` of one of them must be inside the arguments of a call to `blocking`
//!    or `spawn_blocking`, or be matched by an entry of [`ALLOWED`], each with its reason. An entry
//!    that matches no call is itself a failure, so the list can't go stale.
//! 3. Test code is left out: a `#[cfg(test)]` module or item, and the files such an item declares.
//!
//! What it does not see: a call written as a path (`Backend::config_version(&*b)`), and a call
//! made in a helper that is itself called inside `blocking(..)` (it is flagged, and goes on the
//! list with the reason). It reads names, not types: a method of another type that shares a name
//! with a federating one (`.top(` on a top-K list, `.latency(` on the aggregator) looks like a
//! call, and is allowed by name, in the function that makes it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A call the scan lets by without `blocking(..)` around it, and why.
struct Allowed {
    /// The file, from `crates/`.
    file: &'static str,
    /// The `fn` the call is in, or `*` for every one of them in the file.
    in_fn: &'static str,
    why: &'static str,
}

/// Every entry is justified here, and has to match a call the scan finds.
const ALLOWED: &[Allowed] = &[
    Allowed {
        file: "telltale-api/src/lib.rs",
        in_fn: "heatmap_network",
        why: "the `Backend` trait's default: it reads this backend's own `timeseries`. Whoever \
              calls `heatmap_network` is a call the scan checks (it federates)",
    },
    Allowed {
        file: "telltale/src/alerts.rs",
        in_fn: "observe",
        why: "`alerts::run` calls it only inside its one `spawn_blocking` (the rules' reads); the \
              other callers are the module's tests",
    },
    Allowed {
        file: "telltale/src/anomaly.rs",
        in_fn: "start",
        why: "not a backend: `e.clients()` is the anomaly engine's count of tracked devices",
    },
    Allowed {
        file: "telltale/src/anomaly.rs",
        in_fn: "counters",
        why: "not a backend: `e.clients()` is the anomaly engine's count of tracked devices",
    },
    Allowed {
        file: "telltale/src/api_backend.rs",
        in_fn: "*",
        why: "this is the node's own backend: its methods read this node's data and each other, \
              and never ask a peer. The federating backend is `federated.rs`",
    },
    Allowed {
        file: "telltale/src/federated.rs",
        in_fn: "*",
        why: "the federating backend itself: `answer` runs a peer's read on this node's own \
              backend, and the methods call `self.local` or each other. Their callers are what \
              the scan checks",
    },
    Allowed {
        file: "telltale/src/health.rs",
        in_fn: "slo_reasons",
        why: "called only from the `health` method of the two backends, which the scan checks at \
              its callers (`health` federates)",
    },
    Allowed {
        file: "telltale/src/http.rs",
        in_fn: "metrics",
        why: "not a backend: `traces.exemplars()` is the trace ring's",
    },
    Allowed {
        file: "telltale/src/http.rs",
        in_fn: "render",
        why: "not a backend: `w.queries(..)` is the Prometheus writer's",
    },
    Allowed {
        file: "telltale/src/http.rs",
        in_fn: "render_annotations",
        why: "not a backend: `config_version()` is the state store's own",
    },
    Allowed {
        file: "telltale/src/pihole.rs",
        in_fn: "convert",
        why: "not a backend: `b.clients(..)` is the importer's own builder writing the converted \
              clients",
    },
    Allowed {
        file: "telltale/src/pipeline.rs",
        in_fn: "new",
        why: "not a backend: `clients.clients()` is the client table's rows",
    },
    Allowed {
        file: "telltale/src/pipeline.rs",
        in_fn: "mask",
        why: "not a backend: `self.clients.clients()` is the client table's rows",
    },
    Allowed {
        file: "telltale/src/replication.rs",
        in_fn: "serving_loop",
        why: "not a backend: `agg.latency(..)` is the aggregator's",
    },
    Allowed {
        file: "telltale/src/rollups.rs",
        in_fn: "hour_extras",
        why: "not a backend: `agg.latency(..)` is the aggregator's",
    },
    Allowed {
        file: "telltale/src/routers.rs",
        in_fn: "poll_unifi",
        why: "not a backend: `s.clients(..)` is the UniFi source's own (async) request",
    },
    Allowed {
        file: "telltale/src/server.rs",
        in_fn: "config_marker",
        why: "not a backend: `config_version()` is the state store's own",
    },
    Allowed {
        file: "telltale/src/shadow.rs",
        in_fn: "lists_view",
        why: "not a backend: `.top(..)` is the top-K list's",
    },
    Allowed {
        file: "telltale/src/shadow.rs",
        in_fn: "record",
        why: "not a backend: `f.clients.clients()` is the client table's rows",
    },
    Allowed {
        file: "telltale/src/support.rs",
        in_fn: "collect",
        why: "reads this node's own `ApiBackend` (`me`), not the federating one: a support part \
              is this node's, each collector on a thread of its own with a deadline",
    },
];

// ---- reading Rust well enough to scan it -----------------------------------------------------

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Where a string literal that starts after its opening quote at `from` ends (just past the
/// closing quote).
fn string_end(b: &[u8], from: usize) -> usize {
    let mut j = from;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

/// Whether the `r` at `i` starts a raw string (`r"..."`, `r#"..."#`, `br"..."`), and not the end
/// of an identifier.
fn is_raw_string_start(b: &[u8], i: usize) -> bool {
    let before = if i >= 1 && b[i - 1] == b'b' {
        i.checked_sub(2)
    } else {
        i.checked_sub(1)
    };
    if before.is_some_and(|k| is_ident_byte(b[k])) {
        return false;
    }
    let mut j = i + 1;
    while b.get(j) == Some(&b'#') {
        j += 1;
    }
    b.get(j) == Some(&b'"')
}

/// Just past the end of the raw string that starts at the `r` at `i`.
fn raw_string_end(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    let mut hashes = 0;
    while b.get(j) == Some(&b'#') {
        hashes += 1;
        j += 1;
    }
    j += 1; // the opening quote
    while j < b.len() {
        if b[j] == b'"' && (0..hashes).all(|k| b.get(j + 1 + k) == Some(&b'#')) {
            return j + 1 + hashes;
        }
        j += 1;
    }
    b.len()
}

/// Just past the character literal that starts at the quote at `i`, or `i + 1` when it is a
/// lifetime.
fn char_literal_end(src: &str, i: usize) -> usize {
    let rest = &src[i + 1..];
    let mut chars = rest.char_indices();
    match chars.next() {
        // `'\n'`, `'\''`, `'\x41'`, `'\u{1F600}'`: the first quote after the escaped character.
        Some((_, '\\')) => rest
            .char_indices()
            .skip(2)
            .find(|&(_, c)| c == '\'')
            .map_or(i + 1, |(k, _)| i + 1 + k + 1),
        Some((_, c)) if c != '\'' => match chars.next() {
            Some((k, '\'')) => i + 1 + k + 1,
            _ => i + 1,
        },
        _ => i + 1,
    }
}

/// The source with comments, strings and character literals blanked: the same length, with the
/// newlines kept, so offsets and lines are the same in both.
fn code_only(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    while i < b.len() {
        let next = b.get(i + 1).copied();
        let end = match b[i] {
            b'/' if next == Some(b'/') => src[i..].find('\n').map_or(b.len(), |k| i + k),
            b'/' if next == Some(b'*') => {
                let (mut depth, mut j) = (1, i + 2);
                while j < b.len() && depth > 0 {
                    match (b[j], b.get(j + 1)) {
                        (b'/', Some(b'*')) => {
                            depth += 1;
                            j += 2;
                        }
                        (b'*', Some(b'/')) => {
                            depth -= 1;
                            j += 2;
                        }
                        _ => j += 1,
                    }
                }
                j
            }
            b'"' => string_end(b, i + 1),
            b'r' if is_raw_string_start(b, i) => raw_string_end(b, i),
            b'\'' => char_literal_end(src, i),
            _ => i + 1,
        };
        // A lifetime's quote is one byte, and is blanked too; any other single byte is code.
        if end > i + 1 || b[i] == b'\'' {
            for byte in &mut out[i..end.min(b.len())] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        }
        i = end.max(i + 1);
    }
    String::from_utf8(out).expect("whole tokens are blanked, so the text stays UTF-8")
}

/// Just past the `}` that matches the `{` at `open`.
fn after_matching_brace(b: &[u8], open: usize) -> usize {
    let mut depth = 0_usize;
    for (k, &c) in b.iter().enumerate().skip(open) {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return k + 1;
                }
            }
            _ => {}
        }
    }
    b.len()
}

/// A module that a `#[cfg(test)]` attribute declares as a file: its name, and its `#[path]`.
type TestModule = (String, Option<String>);

/// The test code of a file: the byte spans of the `#[cfg(test)]` items that have a body, and the
/// modules that such an attribute declares as files. `code` is `code_only(raw)`; a `#[path = ".."]`
/// string is only in `raw`.
fn test_items(raw: &str, code: &str) -> (Vec<(usize, usize)>, Vec<TestModule>) {
    const ATTR: &str = "#[cfg(test)]";
    let (mut spans, mut modules) = (Vec::new(), Vec::new());
    let cb = code.as_bytes();
    let mut from = 0;
    while let Some(k) = code[from..].find(ATTR) {
        let at = from + k;
        from = at + ATTR.len();
        // The item's header runs to its `{` or its `;`, outside parentheses and brackets.
        let (mut depth, mut j) = (0_i32, from);
        while j < cb.len() {
            match cb[j] {
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                b'{' | b';' if depth <= 0 => break,
                _ => {}
            }
            j += 1;
        }
        if j >= cb.len() {
            break;
        }
        if cb[j] == b'{' {
            spans.push((at, after_matching_brace(cb, j)));
            continue;
        }
        let words: Vec<&str> = code[from..j].split_whitespace().collect();
        let Some(name) = words
            .iter()
            .position(|w| *w == "mod")
            .and_then(|pos| words.get(pos + 1))
        else {
            continue;
        };
        let attrs = &raw[from..j];
        let path = attrs.find("#[path").and_then(|p| {
            let q = attrs[p..].find('"')? + p + 1;
            let e = attrs[q..].find('"')? + q;
            Some(attrs[q..e].to_owned())
        });
        modules.push(((*name).to_owned(), path));
    }
    (spans, modules)
}

/// The identifier called by the `(` at `open`: `blocking` in `blocking(..)` and in
/// `tokio::task::spawn_blocking(..)`. `None` for a macro (`vec!(..)`) or a parenthesised expression.
fn callee(code: &str, open: usize) -> Option<&str> {
    let head = code[..open].trim_end();
    let start = head
        .bytes()
        .rposition(|c| !is_ident_byte(c))
        .map_or(0, |k| k + 1);
    let name = &head[start..];
    (!name.is_empty()).then_some(name)
}

/// The name of the `fn` whose signature is the text before the `{` at `open`, if it is one.
fn fn_before(code: &str, open: usize) -> Option<String> {
    // The signature starts after the last `;`, `{` or `}` outside parentheses and brackets
    // (`fn f(a: [u8; 4]) -> T {` has a `;` inside).
    let (mut depth, mut begin) = (0_usize, 0);
    for (k, c) in code[..open].char_indices().rev() {
        match c {
            ')' | ']' => depth += 1,
            '(' | '[' => depth = depth.saturating_sub(1),
            ';' | '{' | '}' if depth == 0 => {
                begin = k + 1;
                break;
            }
            _ => {}
        }
    }
    let header = &code[begin..open];
    let k = header
        .rmatch_indices("fn ")
        .map(|(k, _)| k)
        .find(|&k| k == 0 || !is_ident_byte(header.as_bytes()[k - 1]))?;
    let name: String = header[k + 3..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// For the byte `pos` of `code`: whether it is inside the arguments of a call to `blocking` or
/// `spawn_blocking`, and the name of the `fn` it is in.
fn enclosing(code: &str, pos: usize) -> (bool, String) {
    let cb = code.as_bytes();
    let (mut depth, mut behind, mut in_fn) = (0_usize, false, None);
    for i in (0..pos).rev() {
        match cb[i] {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' | b'{' if depth > 0 => depth -= 1,
            b'(' => {
                behind |= callee(code, i).is_some_and(|n| n == "blocking" || n == "spawn_blocking");
            }
            b'{' if in_fn.is_none() => in_fn = fn_before(code, i),
            _ => {}
        }
    }
    (
        behind,
        in_fn.unwrap_or_else(|| "<outside any fn>".to_owned()),
    )
}

// ---- which methods federate ------------------------------------------------------------------

/// The `fn`s of the `impl` blocks that start with `header` at the left margin of `src`, by name,
/// with their text (whitespace removed, so `self .gather (` is `self.gather(`).
fn impl_fns(src: &str, header: &str) -> BTreeMap<String, String> {
    let mut fns = BTreeMap::new();
    let marker = format!("\n{header}");
    let mut from = 0;
    while let Some(k) = src[from..].find(&marker) {
        let start = from + k + 1;
        let end = src[start..].find("\n}\n").map_or(src.len(), |e| start + e);
        from = end;
        let mut current: Option<(String, String)> = None;
        // Comments and strings out, so a word in a comment is not a call.
        for line in code_only(&src[start..end]).lines() {
            let declared = ["fn ", "pub fn ", "pub(super) fn ", "pub(crate) fn "]
                .iter()
                .find_map(|p| line.strip_prefix("    ")?.strip_prefix(p));
            if let Some(rest) = declared {
                if let Some((name, body)) = current.take() {
                    fns.insert(name, body);
                }
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                current = Some((name, String::new()));
            }
            if let Some((_, body)) = current.as_mut() {
                body.extend(line.split_whitespace());
            }
        }
        if let Some((name, body)) = current {
            fns.insert(name, body);
        }
    }
    fns
}

/// The `Backend` methods of `Federated` that can wait on a peer: those that reach
/// `bridge::start`, directly or through `self` (and through each other), found in the sources of
/// `federated.rs` and `federated_support.rs`. Also the seed the search started from.
fn federating_methods(main: &str, support: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut inherent = impl_fns(main, "impl Federated {");
    inherent.extend(impl_fns(support, "impl Federated {"));
    let backend = impl_fns(main, "impl Backend for Federated {");
    let all: Vec<(&String, &String)> = inherent.iter().chain(&backend).collect();
    let seed: BTreeSet<String> = all
        .iter()
        .filter(|(_, body)| body.contains("bridge::start("))
        .map(|(name, _)| (*name).clone())
        .collect();
    let mut reaches = seed.clone();
    loop {
        let before = reaches.len();
        for (name, body) in &all {
            let calls_one = reaches.iter().any(|f| {
                body.contains(&format!("self.{f}(")) || body.contains(&format!("self.{f}::<"))
            });
            if calls_one {
                reaches.insert((*name).clone());
            }
        }
        if reaches.len() == before {
            break;
        }
    }
    let methods = backend
        .keys()
        .filter(|n| reaches.contains(*n))
        .cloned()
        .collect();
    (methods, seed)
}

// ---- the scan --------------------------------------------------------------------------------

/// What a scan found.
#[derive(Debug, Default)]
struct Report {
    /// Calls that are inside `blocking(..)` or `spawn_blocking(..)`.
    behind: usize,
    /// Each problem, as a line to print.
    problems: Vec<String>,
}

/// Scans `sources` (a label from `crates/`, and the file's text; test files already left out).
fn scan(sources: &[(String, String)], fan: &BTreeSet<String>, allowed: &[Allowed]) -> Report {
    let mut report = Report::default();
    let mut used = vec![false; allowed.len()];
    for (label, raw) in sources {
        let code = code_only(raw);
        let (tests, _) = test_items(raw, &code);
        for (dot, _) in code.match_indices('.') {
            let rest = &code[dot + 1..];
            let len = rest.bytes().take_while(|&c| is_ident_byte(c)).count();
            let name = &rest[..len];
            let after = rest[len..].trim_start();
            if !fan.contains(name) || !(after.starts_with('(') || after.starts_with("::<")) {
                continue;
            }
            if tests.iter().any(|&(a, b)| (a..b).contains(&dot)) {
                continue;
            }
            let (behind, in_fn) = enclosing(&code, dot);
            if behind {
                report.behind += 1;
                continue;
            }
            let entry = allowed
                .iter()
                .position(|a| a.file == label && (a.in_fn == "*" || a.in_fn == in_fn));
            if let Some(k) = entry {
                used[k] = true;
                continue;
            }
            let line = code[..dot].matches('\n').count() + 1;
            report.problems.push(format!(
                "{label}:{line}: `.{name}(` in `{in_fn}` is not inside `blocking(..)` or \
                 `spawn_blocking(..)`: a federated read on a runtime worker costs its whole \
                 deadline and answers from this node alone (ADR-174). Run it in `blocking`; or, \
                 if it isn't a backend call (another type's method of the same name), add it to \
                 ALLOWED with the reason"
            ));
        }
    }
    for (a, used) in allowed.iter().zip(used) {
        if !used {
            report.problems.push(format!(
                "ALLOWED entry {} in `{}` matches no call: remove it ({})",
                a.file, a.in_fn, a.why
            ));
        }
    }
    report
}

/// Every `.rs` file under `dir`.
fn rust_files(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            rust_files(&p, into);
        } else if p.extension().is_some_and(|e| e == "rs") {
            into.push(p);
        }
    }
}

/// The files that `#[cfg(test)] mod name;` items declare, among `files` (`path`, `text`).
fn declared_test_files(files: &[(PathBuf, String)]) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    for (path, raw) in files {
        let (_, modules) = test_items(raw, &code_only(raw));
        let Some(dir) = path.parent() else { continue };
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let root_like = matches!(stem, "lib" | "main" | "mod");
        for (name, attr) in modules {
            let mut candidates = vec![
                dir.join(format!("{name}.rs")),
                dir.join(&name).join("mod.rs"),
            ];
            if !root_like {
                candidates.push(dir.join(stem).join(format!("{name}.rs")));
            }
            if let Some(p) = attr {
                candidates = vec![dir.join(p)];
            }
            out.extend(
                candidates
                    .into_iter()
                    .filter(|c| files.iter().any(|(f, _)| f == c)),
            );
        }
    }
    out
}

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

// ---- the tests -------------------------------------------------------------------------------

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn found(src: &str, fan: &[&str], allowed: &[Allowed]) -> Report {
    scan(&[("x.rs".to_owned(), src.to_owned())], &set(fan), allowed)
}

// REQ: CLU-004 (ADR-174) — the scan fails on a call outside `blocking`, so it can't pass by
// finding nothing. A bare call is a problem that names its function and line; the same call inside
// `blocking(..)`, inside `tokio::task::spawn_blocking(..)`, or inside a closure nested in either is
// not; and one after a closed `blocking(..)` call in the same function is.
#[test]
fn clu_004_the_scan_flags_a_call_outside_blocking() {
    let bare = "async fn handler(b: Shared) {\n    let x = b.config_version();\n}\n";
    let r = found(bare, &["config_version"], &[]);
    assert_eq!(r.behind, 0);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(r.problems[0].contains("x.rs:2"), "{}", r.problems[0]);
    assert!(r.problems[0].contains("`handler`"), "{}", r.problems[0]);

    let wrapped = "async fn handler(b: Shared) {\n\
        let x = blocking(&b, move |b| b.config_version()).await;\n\
        let y = tokio::task::spawn_blocking(move || {\n    let v = b2.config_version();\n    v\n}).await;\n\
        let z = blocking(&b, |b| b.items().iter().map(|i| b.config_version()).count()).await;\n}\n";
    let r = found(wrapped, &["config_version"], &[]);
    assert_eq!(r.problems, Vec::<String>::new());
    assert_eq!(r.behind, 3);

    let after = "async fn handler(b: Shared) {\n\
        let x = blocking(&b, |b| b.config_version()).await;\n\
        let y = b.config_version();\n}\n";
    let r = found(after, &["config_version"], &[]);
    assert_eq!(r.behind, 1);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(r.problems[0].contains("x.rs:3"), "{}", r.problems[0]);

    // A signature with a `;` inside it (an array type) still names its function.
    let array = "fn weird(a: [u8; 4]) -> u8 {\n    b.config_version()\n}\n";
    let r = found(array, &["config_version"], &[]);
    assert!(r.problems[0].contains("`weird`"), "{:?}", r.problems);
}

// REQ: CLU-004 (ADR-174) — what is not a call is not flagged: a comment, a string, a raw string, a
// definition, a name that doesn't federate, a lifetime that is not a character literal, and test
// code (an inline `#[cfg(test)]` module or fn).
#[test]
fn clu_004_the_scan_ignores_what_is_not_a_call() {
    let src = r##"
// b.config_version() in a comment
/* b.config_version() in /* a nested */ block comment */
/// b.config_version() in a doc comment
fn config_version(&self) -> u64 { 0 }
fn quiet<'a>(b: &'a Shared, c: char) -> &'a str {
    let s = "b.config_version()";
    let r = r#"b.config_version() and a " quote"#;
    let q = '"';
    let t = '\'';
    let other = b.some_other_method();
    "x"
}
#[cfg(test)]
mod tests {
    fn t(b: Shared) { b.config_version(); }
}
#[cfg(test)]
fn helper(b: Shared) { b.config_version(); }
"##;
    let r = found(src, &["config_version"], &[]);
    assert_eq!(r.problems, Vec::<String>::new());
    assert_eq!(r.behind, 0);
    // The same text with the test-only items made real code flags both of their calls.
    let real = src.replace("#[cfg(test)]", "");
    assert_eq!(found(&real, &["config_version"], &[]).problems.len(), 2);
}

// REQ: CLU-004 (ADR-174) — the allow-list: an entry silences the one function it names (or every
// function of its file with `*`), and an entry that matches no call is a problem, so a fixed handler
// can't leave a stale exemption behind.
#[test]
fn clu_004_the_scan_allow_list_is_exact() {
    let src = "fn a(b: Shared) { b.config_version(); }\nfn c(b: Shared) { b.config_version(); }\n";
    let entry = |in_fn| Allowed {
        file: "x.rs",
        in_fn,
        why: "a reason",
    };
    let r = found(src, &["config_version"], &[entry("a")]);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(r.problems[0].contains("`c`"), "{}", r.problems[0]);
    assert_eq!(
        found(src, &["config_version"], &[entry("*")]).problems,
        Vec::<String>::new()
    );
    let stale = found(
        src,
        &["config_version"],
        &[entry("a"), entry("c"), entry("gone")],
    );
    assert_eq!(stale.problems.len(), 1, "{:?}", stale.problems);
    assert!(
        stale.problems[0].contains("matches no call"),
        "{}",
        stale.problems[0]
    );
    // An entry for another file does not apply, and is itself stale.
    let other = Allowed {
        file: "y.rs",
        in_fn: "*",
        why: "elsewhere",
    };
    assert_eq!(found(src, &["config_version"], &[other]).problems.len(), 3);
}

// REQ: CLU-004 (ADR-174) — the reader: nested block comments, raw strings with quotes and hashes
// inside, character literals, and lifetimes (which are not literals, and must not eat the code
// after them).
#[test]
fn clu_004_the_scan_reads_rust_tokens() {
    let spaces = |n: usize| " ".repeat(n);
    // `/* b /* c */ d */` is 17 characters, with a space on each side.
    assert_eq!(
        code_only("a /* b /* c */ d */ e"),
        format!("a{}e", spaces(19))
    );
    assert_eq!(code_only("x // y\nz"), format!("x{}\nz", spaces(5)));
    // The string `"b\"c"` is 6 characters.
    assert_eq!(code_only(r#"a "b\"c" d"#), format!("a{}d", spaces(8)));
    // The raw string `r#"b"c"#` is 8.
    assert_eq!(code_only("a r#\"b\"c\"# d"), format!("a{}d", spaces(10)));
    // A lifetime's quote goes, the letter stays; `'"'` is a 3-character literal.
    assert_eq!(
        code_only("f::<'a>(x) '\"' g"),
        format!("f::< a>(x){}g", spaces(5))
    );
    // Lifetimes keep the code after them; `'x'` and `'\''` are literals.
    assert!(code_only("fn f<'a>(x: &'a str) { y.top() }").contains("y.top()"));
    assert!(!code_only("let c = 'x'; let d = '\\''; z").contains('x'));
    // The text keeps its length and its lines, whatever was blanked.
    let src = "a \"é\"\n/* ü */ b 'é'\n";
    let out = code_only(src);
    assert_eq!(out.len(), src.len());
    assert_eq!(out.matches('\n').count(), 2);
}

// REQ: CLU-004 (ADR-174) — the test code of a file: an inline `#[cfg(test)]` module's span, and a
// `#[cfg(test)] mod x;` with its `#[path]`.
#[test]
fn clu_004_the_scan_finds_the_test_code_of_a_file() {
    let src = "#[cfg(test)]\nmod tests;\n\
               #[cfg(test)]\n#[path = \"other_tests.rs\"]\nmod other;\n\
               #[cfg(test)]\nuse std::fmt;\n\
               #[cfg(test)]\nmod inline { fn f() {} }\nfn real() {}\n";
    let (spans, modules) = test_items(src, &code_only(src));
    assert_eq!(
        modules,
        [
            ("tests".to_owned(), None),
            ("other".to_owned(), Some("other_tests.rs".to_owned()))
        ]
    );
    assert_eq!(spans.len(), 1);
    let (a, b) = spans[0];
    assert!(src[a..b].contains("fn f()") && !src[a..b].contains("real"));
}

// REQ: CLU-004 (ADR-174) — which methods federate is derived from the code that does the asking: a
// method that reaches `bridge::start`, directly or through others. A sample with a chain (`alpha`
// calls `gather` calls the bridge, `beta` calls `alpha`, `local` calls neither) shows the
// derivation; the real sources are then checked for methods that must be in it and ones that must
// not, so a refactor that breaks the derivation fails here and does not empty the guard.
#[test]
fn clu_004_the_federating_methods_are_derived_from_the_bridge() {
    let sample = "\nimpl Federated {\n    fn start_calls(&self) {\n        bridge::start(&W, rt, calls, t)\n    }\n\
        \n    fn gather(&self) {\n        self.start_calls();\n    }\n}\n\
        \nimpl Backend for Federated {\n    fn alpha(&self) {\n        self.gather()\n    }\n\
        \n    fn beta(&self) {\n        self.alpha();\n    }\n\
        \n    fn local(&self) {\n        self.local.local()\n    }\n}\n";
    let (methods, seed) = federating_methods(sample, "");
    assert_eq!(seed, set(&["start_calls"]));
    assert_eq!(methods, set(&["alpha", "beta"]));

    let (methods, seed) = federating_methods(
        include_str!("federated.rs"),
        include_str!("federated_support.rs"),
    );
    assert_eq!(
        seed,
        set(&["start_calls"]),
        "the one place that hands calls to the bridge"
    );
    for m in [
        "config_version",
        "config_entries",
        "timeseries",
        "queries",
        "top",
        "top_range",
        "latency",
        "identities",
        "blocking_pause",
        "blocking_resume",
        "cache_flush",
        "support_package",
        "health",
        "slo_parts",
        "heatmap_network",
        "exemplars",
        "probes",
        "annotations",
    ] {
        assert!(
            methods.contains(m),
            "{m} federates, and is not in {methods:?}"
        );
    }
    for m in [
        "system_info",
        "backup",
        "lists",
        "services",
        "promote",
        "local",
        "missing_nodes",
    ] {
        assert!(!methods.contains(m), "{m} is this node's own");
    }
    assert!(methods.len() >= 25, "{methods:?}");
}

// REQ: CLU-004 (ADR-174) — the guard itself. Every backend call that can federate, in the
// production code of `telltale-api` and `telltale`, is inside `blocking(..)` or `spawn_blocking(..)`,
// apart from the entries of `ALLOWED`. ADR-174's sweep of the handlers was done by hand once; this
// keeps it true for the next one.
#[test]
fn clu_004_every_federating_backend_call_sits_behind_blocking() {
    let (fan, _) = federating_methods(
        include_str!("federated.rs"),
        include_str!("federated_support.rs"),
    );
    let root = crates_dir();
    let mut paths = Vec::new();
    for dir in ["telltale-api/src", "telltale/src"] {
        rust_files(&root.join(dir), &mut paths);
    }
    let files: Vec<(PathBuf, String)> = paths
        .into_iter()
        .filter_map(|p| Some((p.clone(), std::fs::read_to_string(&p).ok()?)))
        .collect();
    let test_files = declared_test_files(&files);
    // Not vacuous: the files are there, and the test files that must be left out are found.
    assert!(files.len() >= 60, "{} files", files.len());
    for must in [
        "telltale-api/src/tests.rs",
        "telltale/src/kubernetes/tests.rs",
        "telltale/src/federated_guard_tests.rs",
    ] {
        assert!(
            test_files.iter().any(|f| f.ends_with(must)),
            "{must} is not found as a test file: {test_files:?}"
        );
    }
    let sources: Vec<(String, String)> = files
        .iter()
        .filter(|(p, _)| !test_files.contains(p))
        .map(|(p, text)| {
            let label = p.strip_prefix(&root).unwrap_or(p).to_string_lossy();
            (label.into_owned(), text.clone())
        })
        .collect();
    let report = scan(&sources, &fan, ALLOWED);
    assert!(
        report.problems.is_empty(),
        "{} problem(s):\n{}",
        report.problems.len(),
        report.problems.join("\n")
    );
    assert!(
        report.behind >= 30,
        "the scan saw only {} calls behind blocking; it is reading nothing",
        report.behind
    );
}
