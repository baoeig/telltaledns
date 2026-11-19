//! REQ: CLU-002, CLU-004, OPS-012 (ADR-174, GitHub issue #7) — how synchronous code waits for the
//! cluster channel.
//!
//! The `Backend` trait is synchronous, so a federated read starts its RPCs on the runtime and
//! waits for the answers on a thread of its own: a blocking-pool thread, where the API runs its
//! backend calls. Three rules:
//!
//! 1. The wait never needs the runtime to end it. It is a `recv_timeout` on a `std` channel,
//!    for the RPC's own timeout plus a short grace. A runtime that can't run the tasks (every
//!    worker parked, or this very call made on a worker) costs the read its deadline, never the
//!    node its life. Before, the wait was a scoped thread running `Handle::block_on` and a
//!    `join()` with no bound: with every worker parked in one, nothing could write an RPC, read
//!    its answer or fire its timer, and the node stopped answering for good.
//! 2. Few waits at once. Each holds a thread (of the blocking pool's 512) for up to its deadline,
//!    so a slow or dead primary can't pile them up. Past [`MAX_WAITS`] a read is told "didn't
//!    answer" without being sent.
//! 3. Few calls at once. A read is one task per call and holds one place under rule 2 for all
//!    of them, so the places bound reads, not tasks; tasks are what hold memory. Two limits
//!    bound them. The calls of all reads together are a budget of [`CALL_BUDGET`] (32,768: the
//!    most that 32 reads of 1,024 calls, the first cap, could have had): a read takes a share
//!    for each of its calls, all of them or none, and a call's share comes back when its task
//!    ends or is dropped, so tasks queued on a stuck runtime keep theirs. And one read may have
//!    at most [`MAX_READ_CALLS`] calls, which is what a range of time on a large cluster needs
//!    (a read for the nodes' time buckets is one call per node and window of time,
//!    `telltale_api::federation::timeseries_chunks`, which bounds the windows on its own,
//!    first). A read over either is not sent (every call: "wasn't asked"), and takes nothing.
//!
//! A read that is refused is counted and said in the log, as a warning at most once a minute
//! ([`Limiter::refused`]).
//!
//! What this does not do: a read made on a worker thread of a multi-thread runtime costs its
//! deadline even when the runtime is healthy. Tokio puts a task a worker spawns in that worker's
//! own LIFO slot, which no other worker steals, and the worker is the one parked in
//! `recv_timeout`, so the task (the last, or only, call of the read) never runs. No caller does
//! that: every one is under `blocking()` or `spawn_blocking`. This is for the next handler.
//!
//! No `block_in_place`: it panics outside a worker (the release profile aborts on a panic) and
//! ties the runtime's own liveness to the blocking pool.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, mpsc};
use std::time::{Duration, Instant};

/// How long past a call's own timeout the caller keeps waiting for the runtime to run it.
pub(super) const GRACE: Duration = Duration::from_millis(250);

/// Federated waits in flight at once on this node (each is a read: all of its peers together).
/// A healthy cluster's reads end in milliseconds; this only bounds a slow or dead peer's cost, in
/// threads: each wait holds one of the blocking pool's.
const MAX_WAITS: usize = 32;

/// The most calls one read may have. A read is one call to each peer, or, for a range of time,
/// one to each peer for each window of it. The longest range any step documents is 87 windows
/// (`step=day` over all of history; 7 days of minutes is 43, 400 days of hours 41, 15 minutes of
/// seconds 4), and `telltale_api::federation::TIMESERIES_MAX_WINDOWS` caps one read at 256 a
/// peer, so 4,096 lets a cluster of 47 peers ask for all of history by the day, 95 for 7 days of
/// minutes and 99 for 400 days of hours, whole. A read over it is not sent. It bounds what one
/// request can ask; [`CALL_BUDGET`] bounds what all of them hold.
pub(super) const MAX_READ_CALLS: usize = 4096;

/// The calls that may be tasks on the runtime at once, all reads together: what holds memory is
/// a task, so this is the bound that matters. 32,768 is 32 reads ([`MAX_WAITS`]) of 1,024
/// calls, the most the first, flat cap (1,024 calls a read) could ever have had, so raising
/// [`MAX_READ_CALLS`] did not raise the worst case: it lets one big read use what many small ones
/// could. At most 8 reads of [`MAX_READ_CALLS`] fit at once.
pub(super) const CALL_BUDGET: usize = 32 * 1024;

// The budget lives in the low half of a `u64` beside the reads in the high half (see `Limiter`),
// and a read that is allowed at all must be able to fit.
const _: () = assert!(MAX_READ_CALLS <= CALL_BUDGET);
const _: () = assert!(CALL_BUDGET as u64 <= CALL_MASK);

/// The cap every federated read of this process shares.
pub(super) static WAITS: LazyLock<Limiter> = LazyLock::new(|| Limiter::new(MAX_WAITS));

/// How often, at most, refused reads are said in the log.
const SAY_EVERY_MS: u64 = 60_000;

/// Why a read was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refused {
    /// [`MAX_WAITS`] reads were already waiting.
    Full,
    /// The calls other reads have in flight left no room for all of this read's: the
    /// [`CALL_BUDGET`] was spent.
    Spent,
    /// The read asked for too much at once: over [`MAX_READ_CALLS`] calls, or a range of time of
    /// too many windows.
    TooBig,
}

impl Refused {
    /// What each call of the read is told, after "wasn't asked".
    fn why(self) -> &'static str {
        match self {
            Self::Full => "too many cluster reads are already waiting",
            Self::Spent => "too many cluster calls are already in flight",
            Self::TooBig => "the read has too many calls at once",
        }
    }
}

/// How many reads were refused for each reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Tally {
    full: u64,
    spent: u64,
    too_big: u64,
}

/// What was refused since the last line said so, and when that was.
#[derive(Debug, Default)]
struct RefusalLog {
    /// The clock (milliseconds, any origin) of the last line, plus one: zero is "never".
    last: AtomicU64,
    full: AtomicU64,
    spent: AtomicU64,
    too_big: AtomicU64,
}

impl RefusalLog {
    /// Counts a refused read. Returns what to say when a line is due: the first time, then once
    /// a minute at most, with what was counted since the last line (which this resets). Of any
    /// number of threads that find a line due, one gets it, and a refusal is in exactly one line.
    fn note(&self, why: Refused, now_ms: u64) -> Option<Tally> {
        match why {
            Refused::Full => self.full.fetch_add(1, Ordering::AcqRel),
            Refused::Spent => self.spent.fetch_add(1, Ordering::AcqRel),
            Refused::TooBig => self.too_big.fetch_add(1, Ordering::AcqRel),
        };
        let stamp = now_ms.saturating_add(1);
        let last = self.last.load(Ordering::Acquire);
        if last != 0 && stamp.saturating_sub(last) < SAY_EVERY_MS {
            return None;
        }
        self.last
            .compare_exchange(last, stamp, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Tally {
            full: self.full.swap(0, Ordering::AcqRel),
            spent: self.spent.swap(0, Ordering::AcqRel),
            too_big: self.too_big.swap(0, Ordering::AcqRel),
        })
    }

    /// What is counted and not yet said.
    #[cfg(test)]
    fn pending(&self) -> Tally {
        Tally {
            full: self.full.load(Ordering::Acquire),
            spent: self.spent.load(Ordering::Acquire),
            too_big: self.too_big.load(Ordering::Acquire),
        }
    }
}

/// One read is `ONE_READ` in the limiter's count, and one call is `1`: the reads waiting are
/// the count's high half and the calls in flight its low half (`CALL_MASK`).
const ONE_READ: u64 = 1 << 32;
const CALL_MASK: u64 = ONE_READ - 1;

/// What the federated reads of a process may hold at once: places among the reads waiting
/// ([`MAX_WAITS`]) and shares of the budget of calls in flight ([`CALL_BUDGET`]).
///
/// Both are one number, so one compare-and-swap takes a read's place and all of its calls' shares
/// together, or takes nothing: a refused read never holds a share for a moment, which two counts
/// taken one after the other could not promise (a read refused for lack of a place would, for an
/// instant, have held shares that made a read that fits look too big).
#[derive(Debug)]
pub(super) struct Limiter {
    /// Reads waiting (high half) and calls in flight (low half).
    state: Arc<AtomicU64>,
    max_reads: u64,
    budget: u64,
    log: RefusalLog,
    /// The refusal log's clock: monotonic, from when this was made.
    since: Instant,
}

/// One read's place among the reads waiting. It is given back when the read's last task ends
/// (not when the caller stops waiting: a task still queued on a stuck runtime still counts).
#[derive(Debug)]
struct Place(Arc<AtomicU64>);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.fetch_sub(ONE_READ, Ordering::AcqRel);
    }
}

/// One call's share of the budget. It is given back when the call's task ends or is dropped,
/// run or not (the runtime shutting down drops the tasks it never ran); a task still queued on a
/// stuck runtime keeps its share, as it keeps the read's place.
#[derive(Debug)]
struct Share(Arc<AtomicU64>);

impl Drop for Share {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The shares a read took and has not handed to a task yet. What is left when it is dropped goes
/// back to the budget, so a read that stops half-way (a panic) leaks nothing.
#[derive(Debug)]
struct Unissued {
    state: Arc<AtomicU64>,
    left: u64,
}

impl Unissued {
    fn issue(&mut self) -> Option<Share> {
        self.left = self.left.checked_sub(1)?;
        Some(Share(Arc::clone(&self.state)))
    }
}

impl Drop for Unissued {
    fn drop(&mut self) {
        if self.left > 0 {
            self.state.fetch_sub(self.left, Ordering::AcqRel);
        }
    }
}

/// What a read that was allowed holds.
#[derive(Debug)]
struct Grant {
    place: Arc<Place>,
    shares: Unissued,
}

impl Limiter {
    /// Up to `max_reads` reads waiting, and [`CALL_BUDGET`] calls in flight.
    pub(super) fn new(max_reads: usize) -> Self {
        Self::with_budget(max_reads, CALL_BUDGET)
    }

    pub(super) fn with_budget(max_reads: usize, budget: usize) -> Self {
        Self {
            state: Arc::new(AtomicU64::new(0)),
            max_reads: u64::try_from(max_reads).unwrap_or(u64::MAX),
            budget: u64::try_from(budget).unwrap_or(CALL_MASK).min(CALL_MASK),
            log: RefusalLog::default(),
            since: Instant::now(),
        }
    }

    /// REQ: CLU-004 (final review, F2) — counts a read that was not sent, and says so in the log
    /// as a warning: the first time, then at most once a minute, with how many were refused
    /// since the last line. A silent refusal showed as healthy peers in `missingNodes`.
    pub(super) fn refused(&self, why: Refused) {
        let now = u64::try_from(self.since.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(n) = self.log.note(why, now) {
            tracing::warn!(
                "federated reads are being refused and were not sent: {} because {} reads were \
                 already waiting (a slow or silent node), {} because the {} calls that reads may \
                 have in flight at once were spent, {} because they asked for too much at once \
                 (over {MAX_READ_CALLS} calls, or over {} windows of time a node); counted since \
                 the last line of this kind, at most one a minute. The nodes they were for show \
                 as not answering",
                n.full,
                self.max_reads,
                n.spent,
                self.budget,
                n.too_big,
                telltale_api::federation::TIMESERIES_MAX_WINDOWS,
            );
        }
    }

    /// REQ: CLU-004, CLU-002 (ADR-174) — a place among the reads and a share for each of `n`
    /// calls, all or nothing. Over the places: [`Refused::Full`]; over the budget:
    /// [`Refused::Spent`]. The caller has already kept `n` within [`MAX_READ_CALLS`].
    fn acquire(&self, n: usize) -> Result<Grant, Refused> {
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        self.state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |s| {
                let (reads, calls) = (s >> 32, s & CALL_MASK);
                // `calls + n` fits the low half: it is at most the budget, which does.
                (reads < self.max_reads && calls.saturating_add(n) <= self.budget)
                    .then_some(s + ONE_READ + n)
            })
            .map(|_| Grant {
                place: Arc::new(Place(Arc::clone(&self.state))),
                shares: Unissued {
                    state: Arc::clone(&self.state),
                    left: n,
                },
            })
            .map_err(|seen| {
                if seen >> 32 >= self.max_reads {
                    Refused::Full
                } else {
                    Refused::Spent
                }
            })
    }

    /// Reads waiting now.
    #[cfg(test)]
    fn in_flight(&self) -> usize {
        usize::try_from(self.state.load(Ordering::Acquire) >> 32).unwrap_or(usize::MAX)
    }

    /// Calls that are tasks now (started, and not ended or dropped).
    #[cfg(test)]
    fn calls_in_flight(&self) -> usize {
        usize::try_from(self.state.load(Ordering::Acquire) & CALL_MASK).unwrap_or(usize::MAX)
    }
}

/// One call's result: the peer's answer, or why there is none.
type Answer = Result<Vec<u8>, String>;

/// Calls started on the runtime, to be waited for on the calling thread.
#[derive(Debug)]
pub(super) struct Pending {
    peers: Vec<String>,
    /// What is in so far, by call.
    results: Vec<Option<Answer>>,
    rx: Option<mpsc::Receiver<(usize, Answer)>>,
    /// When the caller stops waiting.
    until: Instant,
    /// Set when the caller stops waiting: a call still queued then isn't sent.
    gave_up: Arc<AtomicBool>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.gave_up.store(true, Ordering::Release);
    }
}

/// Starts each of `calls` (a peer, and its RPC) as a task on `rt` and returns at once. `timeout`
/// is the RPCs' own; the wait ends [`GRACE`] after it at the latest. Over the cap of reads, with
/// more than [`MAX_READ_CALLS`] calls, or without room for all of them in the [`CALL_BUDGET`],
/// nothing is started and nothing is taken, and every call's result is "wasn't asked".
pub(super) fn start<Fut>(
    limiter: &Limiter,
    rt: &tokio::runtime::Handle,
    calls: Vec<(String, Fut)>,
    timeout: Duration,
) -> Pending
where
    Fut: Future<Output = Answer> + Send + 'static,
{
    let (peers, futures): (Vec<String>, Vec<Fut>) = calls.into_iter().unzip();
    let mut results: Vec<Option<Answer>> = vec![None; peers.len()];
    let pending = |peers, results, rx| Pending {
        peers,
        results,
        rx,
        until: Instant::now() + timeout + GRACE,
        gave_up: Arc::new(AtomicBool::new(false)),
    };
    if futures.is_empty() {
        return pending(peers, results, None);
    }
    // REQ: CLU-004, CLU-002 (ADR-174; final review, F1) — the places count reads, not the tasks
    // each makes, so the tasks are bounded here: a read of too many calls, or one the budget of
    // calls in flight has no room for, is not sent, and takes neither a place nor a share.
    let allowed = if futures.len() > MAX_READ_CALLS {
        Err(Refused::TooBig)
    } else {
        limiter.acquire(futures.len())
    };
    let mut grant = match allowed {
        Ok(grant) => grant,
        Err(why) => {
            limiter.refused(why);
            drop(futures);
            for (r, peer) in results.iter_mut().zip(&peers) {
                *r = Some(Err(format!("{peer} wasn't asked: {}", why.why())));
            }
            return pending(peers, results, None);
        }
    };
    let (tx, rx) = mpsc::channel();
    let started = pending(peers, results, Some(rx));
    for (i, call) in futures.into_iter().enumerate() {
        // One share for each task, and every task keeps the read's place: the share goes when
        // its task does, the place when the last of them does.
        let Some(share) = grant.shares.issue() else {
            break;
        };
        let (tx, place, gave_up) = (
            tx.clone(),
            Arc::clone(&grant.place),
            Arc::clone(&started.gave_up),
        );
        drop(rt.spawn(async move {
            let (_place, _share) = (place, share);
            if gave_up.load(Ordering::Acquire) {
                return;
            }
            let _ = tx.send((i, call.await));
        }));
    }
    started
}

impl Pending {
    /// Waits for every call, or until the deadline: one entry per call, in the order given, with
    /// the answer or why there is none (`didn't answer in time` for a call the runtime never
    /// finished). Blocks the calling thread only: it needs nothing from the runtime to wake.
    pub(super) fn wait(mut self) -> Vec<(String, Answer)> {
        let mut missing = self.results.iter().filter(|r| r.is_none()).count();
        let mut why = "didn't answer in time";
        if let Some(rx) = self.rx.take() {
            while missing > 0 {
                let left = self.until.saturating_duration_since(Instant::now());
                match rx.recv_timeout(left) {
                    Ok((i, r)) => self.put(i, r, &mut missing),
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        why = "wasn't answered: its call was dropped";
                        break;
                    }
                }
            }
            // What came in as the deadline passed.
            while let Ok((i, r)) = rx.try_recv() {
                self.put(i, r, &mut missing);
            }
        }
        let peers = std::mem::take(&mut self.peers);
        let results = std::mem::take(&mut self.results);
        peers
            .into_iter()
            .zip(results)
            .map(|(peer, r)| {
                let r = r.unwrap_or_else(|| Err(format!("{peer} {why}")));
                (peer, r)
            })
            .collect()
    }

    fn put(&mut self, i: usize, r: Answer, missing: &mut usize) {
        if let Some(slot) = self.results.get_mut(i)
            && slot.is_none()
        {
            *slot = Some(r);
            *missing -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::channel;

    use super::*;

    type Call = Pin<Box<dyn Future<Output = Answer> + Send>>;

    fn runtime(workers: usize) -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()
            .unwrap()
    }

    fn answer(text: &'static str) -> Call {
        Box::pin(async move { Ok(text.as_bytes().to_vec()) })
    }

    /// Parks the runtime's only worker until the returned sender is used, and returns once it is
    /// parked: the production wedge.
    fn wedge(rt: &tokio::runtime::Runtime) -> mpsc::Sender<()> {
        let (gate, shut) = channel::<()>();
        let (parked_tx, parked) = channel::<()>();
        drop(rt.spawn(async move {
            let _ = parked_tx.send(());
            // Blocking on purpose; it gives up after a minute so a failed test leaves nothing.
            let _ = shut.recv_timeout(Duration::from_secs(60));
        }));
        parked.recv_timeout(Duration::from_secs(10)).unwrap();
        gate
    }

    /// A bounded wait for a condition that another thread will make true.
    fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !ok() {
            assert!(Instant::now() < until, "not met in time: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A count the calls of a test share.
    fn count() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    /// Counts itself while it exists: held by a call (queued, running, or dropped without having
    /// run), so a test can see how many calls a limiter really let live without asking the
    /// limiter.
    #[derive(Debug)]
    struct Live(Arc<AtomicUsize>);

    impl Live {
        fn new(live: &Arc<AtomicUsize>) -> Self {
            live.fetch_add(1, Ordering::SeqCst);
            Self(Arc::clone(live))
        }
    }

    impl Drop for Live {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// A door the calls wait at: shut until `open`. (A `watch`, so a call that reaches the door
    /// after it was opened goes through, and a test can start any number of calls on it.)
    struct Gate(tokio::sync::watch::Sender<bool>);

    impl Gate {
        fn shut() -> Self {
            Self(tokio::sync::watch::channel(false).0)
        }

        fn open(&self) {
            self.0.send_replace(true);
        }

        /// A call that waits at the gate, then answers `ok`. It counts in `live` while it exists
        /// and in `ran` once it is run.
        fn call(&self, live: &Arc<AtomicUsize>, ran: &Arc<AtomicUsize>) -> Call {
            let (mut door, live, ran) = (self.0.subscribe(), Live::new(live), Arc::clone(ran));
            Box::pin(async move {
                let _live = live;
                let _ = door.wait_for(|open| *open).await;
                ran.fetch_add(1, Ordering::SeqCst);
                Ok(b"ok".to_vec())
            })
        }

        /// `n` such calls to peers named `<tag><i>`.
        fn calls(
            &self,
            n: usize,
            tag: &str,
            live: &Arc<AtomicUsize>,
            ran: &Arc<AtomicUsize>,
        ) -> Vec<(String, Call)> {
            (0..n)
                .map(|i| (format!("{tag}{i}"), self.call(live, ran)))
                .collect()
        }
    }

    /// Tells a watching thread to stop when it goes out of scope, a panic included.
    struct StopOnDrop(Arc<AtomicBool>);

    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }

    /// Whether `start` sent the read (it did not when it was refused: every call answered at once).
    fn was_sent(read: &Pending) -> bool {
        read.rx.is_some()
    }

    /// Whether every call of an answered read was refused with `wasn't asked` and `why`.
    fn refused_for(got: &[(String, Answer)], why: &str) -> bool {
        !got.is_empty()
            && got.iter().all(|(_, r)| {
                r.as_ref()
                    .is_err_and(|e| e.contains("wasn't asked") && e.contains(why))
            })
    }

    // REQ: CLU-002 — a read's results come back in the order its calls were given, whatever
    // order they finished in; an error is the peer's own answer and passes through.
    #[test]
    fn clu_002_the_bridge_answers_in_the_order_asked() {
        let rt = runtime(2);
        let limiter = Limiter::new(8);
        let (first_done, first) = tokio::sync::oneshot::channel::<()>();
        let calls: Vec<(String, Call)> = vec![
            (
                "a".into(),
                // Finishes last: it waits for "c".
                Box::pin(async move {
                    let _ = first.await;
                    Ok(b"A".to_vec())
                }),
            ),
            (
                "b".into(),
                Box::pin(async { Err("no such read".to_owned()) }),
            ),
            (
                "c".into(),
                Box::pin(async move {
                    let _ = first_done.send(());
                    Ok(b"C".to_vec())
                }),
            ),
        ];
        let got = start(&limiter, rt.handle(), calls, Duration::from_secs(5)).wait();
        let want: Vec<(String, Answer)> = vec![
            ("a".into(), Ok(b"A".to_vec())),
            ("b".into(), Err("no such read".into())),
            ("c".into(), Ok(b"C".to_vec())),
        ];
        assert_eq!(got, want);
        // Nothing to ask is nothing to wait for.
        let none = start::<Call>(&limiter, rt.handle(), Vec::new(), Duration::from_secs(5));
        assert!(none.wait().is_empty(), "no calls, no answers");
        wait_until("every place is given back", || limiter.in_flight() == 0);
    }

    // REQ: CLU-002, CLU-004 — a call that never finishes costs the read its deadline (and the
    // grace), and the calls that did finish still answer.
    #[test]
    fn clu_004_a_call_that_never_finishes_costs_only_its_deadline() {
        let rt = runtime(2);
        let limiter = Limiter::new(8);
        let calls: Vec<(String, Call)> = vec![
            ("quick".into(), answer("Q")),
            (
                "stuck".into(),
                Box::pin(async {
                    std::future::pending::<()>().await;
                    Ok(Vec::new())
                }),
            ),
        ];
        let started = Instant::now();
        let got = start(&limiter, rt.handle(), calls, Duration::from_millis(100)).wait();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(got[0], ("quick".to_owned(), Ok(b"Q".to_vec())));
        let (peer, r) = &got[1];
        assert_eq!(peer, "stuck");
        assert!(
            r.as_ref().unwrap_err().contains("didn't answer in time"),
            "{r:?}"
        );
    }

    // REQ: CLU-004 (ADR-174, GitHub issue #7) — every worker parked: the wait still ends at its
    // deadline, from a plain thread; and a call nobody ran is not sent once the caller has gone
    // (the runtime wakes to find the queue full of calls whose callers left).
    #[test]
    fn clu_004_a_wedged_runtime_costs_the_bridge_its_deadline_and_the_call_is_not_sent_late() {
        let rt = runtime(1);
        let limiter = Limiter::new(8);
        let gate = wedge(&rt);
        let sent = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&sent);
        let calls: Vec<(String, Call)> = vec![(
            "peer".into(),
            Box::pin(async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }),
        )];
        let started = Instant::now();
        let got = start(&limiter, rt.handle(), calls, Duration::from_millis(100)).wait();
        // Only an upper bound, far above the 350 ms: no race.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(
            got[0]
                .1
                .as_ref()
                .unwrap_err()
                .contains("didn't answer in time"),
            "{got:?}"
        );
        assert_eq!(
            limiter.in_flight(),
            1,
            "the queued task still holds its place"
        );
        // The runtime comes back: the task runs, finds nobody waiting, and sends nothing.
        gate.send(()).unwrap();
        wait_until("the queued task ends", || limiter.in_flight() == 0);
        assert_eq!(
            sent.load(Ordering::SeqCst),
            0,
            "a call was sent after its caller left"
        );
    }

    // REQ: CLU-004 (ADR-174) — over the cap a read is not sent: it is told "wasn't asked" at
    // once, without a wait; when the place is free again, reads go out.
    #[test]
    fn clu_004_over_the_cap_a_read_is_not_sent() {
        let rt = runtime(2);
        let limiter = Limiter::new(1);
        let (open, shut) = tokio::sync::oneshot::channel::<()>();
        let held: Vec<(String, Call)> = vec![(
            "held".into(),
            Box::pin(async move {
                let _ = shut.await;
                Ok(b"H".to_vec())
            }),
        )];
        let first = start(&limiter, rt.handle(), held, Duration::from_secs(30));
        assert_eq!(limiter.in_flight(), 1);
        let sent = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&sent);
        let second_calls: Vec<(String, Call)> = vec![(
            "late".into(),
            Box::pin(async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }),
        )];
        let started = Instant::now();
        let second = start(&limiter, rt.handle(), second_calls, Duration::from_secs(30)).wait();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(
            second[0].1.as_ref().unwrap_err().contains("wasn't asked"),
            "{second:?}"
        );
        assert_eq!(limiter.in_flight(), 1, "a refused read takes no place");
        // The held read finishes; its place comes back; the next read goes out.
        open.send(()).unwrap();
        assert_eq!(first.wait()[0].1, Ok(b"H".to_vec()));
        wait_until("the place is given back", || limiter.in_flight() == 0);
        let third = start(
            &limiter,
            rt.handle(),
            vec![("again".into(), answer("T"))],
            Duration::from_secs(5),
        )
        .wait();
        assert_eq!(third[0].1, Ok(b"T".to_vec()));
        assert_eq!(sent.load(Ordering::SeqCst), 0);
    }

    /// A call that counts the times it is run.
    fn counting(sent: &Arc<AtomicUsize>) -> Call {
        let sent = Arc::clone(sent);
        Box::pin(async move {
            sent.fetch_add(1, Ordering::SeqCst);
            Ok(b"ok".to_vec())
        })
    }

    // REQ: CLU-004, CLU-002 (ADR-174, final review F1) — a read of more than 4,096 calls is not
    // sent at all: none of its calls is run, it takes neither a place nor a share of the budget,
    // and every call is told "wasn't asked". Before: one task per call, however many (a viewer's
    // request for a long range of seconds made millions), under one place.
    #[test]
    fn clu_004_a_read_over_the_ceiling_is_not_sent() {
        let rt = runtime(2);
        let limiter = Limiter::new(8);
        let sent = count();
        assert_eq!(MAX_READ_CALLS, 4096, "the number the ADR and the docs give");
        let calls: Vec<(String, Call)> = (0..=MAX_READ_CALLS)
            .map(|i| (format!("p{i}"), counting(&sent)))
            .collect();
        let started = Instant::now();
        let got = start(&limiter, rt.handle(), calls, Duration::from_secs(30)).wait();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            got.len(),
            MAX_READ_CALLS + 1,
            "one answer for each call, in order"
        );
        assert!(
            got.iter().enumerate().all(|(i, (peer, r))| {
                *peer == format!("p{i}") && r.as_ref().is_err_and(|e| e.contains("wasn't asked"))
            }),
            "{:?}",
            got.first()
        );
        assert_eq!(limiter.in_flight(), 0, "a refused read takes no place");
        assert_eq!(
            limiter.calls_in_flight(),
            0,
            "a refused read takes no share"
        );
        assert_eq!(sent.load(Ordering::SeqCst), 0, "no call was run");
    }

    // REQ: CLU-004, CLU-002 (ADR-174, final review F1) — the ceiling is on what a read may ask,
    // not a cut in what is sent: a read of exactly 4,096 calls (a 47-node cluster's whole history by
    // the day) is sent in full and answered in order, and every share and place comes back.
    // Before: a flat cap of 1,024 refused it.
    #[test]
    fn clu_004_a_read_at_the_ceiling_is_sent_in_full() {
        let rt = runtime(2);
        let limiter = Limiter::new(8);
        let sent = count();
        let calls: Vec<(String, Call)> = (0..MAX_READ_CALLS)
            .map(|i| (format!("p{i}"), counting(&sent)))
            .collect();
        let got = start(&limiter, rt.handle(), calls, Duration::from_secs(30)).wait();
        assert_eq!(got.len(), MAX_READ_CALLS);
        assert!(
            got.iter()
                .enumerate()
                .all(|(i, (peer, r))| { *peer == format!("p{i}") && *r == Ok(b"ok".to_vec()) }),
            "{:?}",
            got.iter().find(|(_, r)| r.is_err())
        );
        assert_eq!(sent.load(Ordering::SeqCst), MAX_READ_CALLS);
        wait_until("every place and share is given back", || {
            limiter.in_flight() == 0 && limiter.calls_in_flight() == 0
        });
    }

    // REQ: CLU-004, CLU-002 (ADR-174) — the longest range any step documents, for a cluster of 40
    // peers, is sent in full: 40 peers x 4, 43, 41 and 87 windows (160, 1,720, 1,640 and 3,480
    // calls). Each call runs once and the answers come back in the order asked. Before: the flat
    // cap of 1,024 calls refused three of the four, whole (this is what a 12-peer cluster hit at
    // `step=day`). The edges are 47 peers at the largest range and no more.
    #[test]
    fn clu_002_the_documented_ranges_are_sent_in_full_to_forty_peers() {
        use telltale_api::federation::timeseries_chunks;
        use telltale_api::model::Step;
        let rt = runtime(2);
        let now = 1_791_072_000;
        let cases = [
            (Step::Second, now - 900, now + 1, 4),
            (Step::Minute, now - 7 * 86_400, now + 1, 43),
            (Step::Hour, now - 400 * 86_400, now + 1, 41),
            (Step::Day, 0, now + 1, 87),
        ];
        for (step, from, to, windows) in cases {
            let limiter = Limiter::new(MAX_WAITS);
            let chunks = timeseries_chunks(step, from, to).expect("a documented range");
            assert_eq!(chunks.len(), windows, "{step:?}");
            // One call per peer and window, in the order `peer_timeseries` makes them. Each
            // answers its own number, so a call run twice or out of order shows.
            let ran = count();
            let calls: Vec<(String, Call)> = (0..40)
                .flat_map(|p| (0..chunks.len()).map(move |w| (p, w)))
                .enumerate()
                .map(|(i, (p, w))| {
                    let ran = Arc::clone(&ran);
                    let call: Call = Box::pin(async move {
                        ran.fetch_add(1, Ordering::SeqCst);
                        Ok(i.to_le_bytes().to_vec())
                    });
                    (format!("peer{p}/window{w}"), call)
                })
                .collect();
            let total = calls.len();
            assert_eq!(total, 40 * windows);
            let got = start(&limiter, rt.handle(), calls, Duration::from_secs(30)).wait();
            assert_eq!(got.len(), total, "{step:?}");
            assert!(
                got.iter()
                    .enumerate()
                    .all(|(i, (_, r))| *r == Ok(i.to_le_bytes().to_vec())),
                "{step:?}: {:?}",
                got.iter().find(|(_, r)| r.is_err())
            );
            assert_eq!(ran.load(Ordering::SeqCst), total, "{step:?}");
            wait_until("every place and share is given back", || {
                limiter.in_flight() == 0 && limiter.calls_in_flight() == 0
            });
        }
        // The arithmetic the docs give: the peers whose calls fit in 4,096 at each range.
        assert_eq!(MAX_READ_CALLS / 87, 47, "all of history by the day");
        assert_eq!(MAX_READ_CALLS / 43, 95, "7 days of minutes");
        assert_eq!(MAX_READ_CALLS / 41, 99, "400 days of hours");
    }

    // REQ: CLU-004, CLU-002 (ADR-174) — the worst case did not grow. The budget is the number
    // the flat cap's worst case was (32 reads of 1,024 calls), and a ceiling above the budget's
    // 32nd part must not be a way past it.
    #[test]
    fn clu_004_the_budget_is_the_worst_case_the_flat_cap_allowed() {
        assert_eq!(MAX_WAITS, 32);
        assert_eq!(CALL_BUDGET, 32 * 1024, "32,768 calls in flight, as before");
        assert_eq!(CALL_BUDGET / MAX_READ_CALLS, 8, "eight whole reads fill it");
        assert_eq!(
            MAX_WAITS * MAX_READ_CALLS,
            4 * CALL_BUDGET,
            "the places times the ceiling is four times the budget: the budget is what bounds"
        );
    }

    // REQ: CLU-004, CLU-002 (ADR-174) — a big read is sent when there is room in the budget for
    // all of it, and refused (whole, taking nothing) when there is not. The budget is the
    // production one: eight reads of the ceiling use it all.
    #[test]
    fn clu_004_a_big_read_is_sent_when_the_budget_is_free_and_refused_when_it_is_not() {
        let rt = runtime(2);
        let limiter = Limiter::new(MAX_WAITS);
        let _ = limiter.log.note(Refused::Full, 0); // so the next refusals are only counted
        let (live, ran) = (count(), count());
        let (hold, free) = (Gate::shut(), Gate::shut());
        let long = Duration::from_secs(60);
        let mut parked = Vec::new();
        for r in 0..7 {
            let calls = hold.calls(MAX_READ_CALLS, &format!("hold{r}-"), &live, &ran);
            let read = start(&limiter, rt.handle(), calls, long);
            assert!(was_sent(&read), "read {r} fits");
            parked.push(read);
        }
        // The eighth is the one that will be let go: with it, the budget is spent.
        let last = free.calls(MAX_READ_CALLS, "free-", &live, &ran);
        let last = start(&limiter, rt.handle(), last, long);
        assert!(was_sent(&last));
        assert_eq!(limiter.calls_in_flight(), CALL_BUDGET);
        assert_eq!(limiter.in_flight(), 8);
        // A read of 4,000 calls: refused, whole, and taking nothing.
        let big = hold.calls(4_000, "big-", &live, &ran);
        let started = Instant::now();
        let got = start(&limiter, rt.handle(), big, long).wait();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            refused_for(&got, "calls are already in flight"),
            "{:?}",
            got.first()
        );
        assert_eq!(got.len(), 4_000);
        assert_eq!(limiter.calls_in_flight(), CALL_BUDGET, "it took no share");
        assert_eq!(limiter.in_flight(), 8, "it took no place");
        assert_eq!(
            live.load(Ordering::SeqCst),
            CALL_BUDGET,
            "its calls were dropped"
        );
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert_eq!(
            limiter.log.pending(),
            Tally {
                full: 0,
                spent: 1,
                too_big: 0
            }
        );
        // One read of the ceiling ends: there is room for 4,000 again.
        free.open();
        assert!(last.wait().iter().all(|(_, r)| r.is_ok()));
        wait_until("its shares are back", || {
            limiter.calls_in_flight() == CALL_BUDGET - MAX_READ_CALLS
        });
        let again = hold.calls(4_000, "again-", &live, &ran);
        let again = start(&limiter, rt.handle(), again, long);
        assert!(
            was_sent(&again),
            "4,000 calls fit in the room that was left"
        );
        assert_eq!(
            limiter.calls_in_flight(),
            CALL_BUDGET - MAX_READ_CALLS + 4_000
        );
        hold.open();
        for read in parked.into_iter().chain([again]) {
            assert!(read.wait().iter().all(|(_, r)| r.is_ok()));
        }
        wait_until("everything is given back", || {
            limiter.calls_in_flight() == 0 && limiter.in_flight() == 0
        });
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    // REQ: CLU-004 (ADR-174) — two reads that together are over the budget: the first is sent,
    // the second is refused whole and takes nothing (not a place, not a share, and not part of
    // what it asked: none of its calls runs), and a read that does fit still goes. Exact, with a
    // budget of ten.
    #[test]
    fn clu_004_two_reads_over_the_budget_the_second_is_refused_and_takes_nothing() {
        let rt = runtime(2);
        let limiter = Limiter::with_budget(8, 10);
        let _ = limiter.log.note(Refused::Full, 0);
        let (live, ran, gate) = (count(), count(), Gate::shut());
        let long = Duration::from_secs(60);
        let first = start(&limiter, rt.handle(), gate.calls(6, "a", &live, &ran), long);
        assert!(was_sent(&first));
        assert_eq!((limiter.in_flight(), limiter.calls_in_flight()), (1, 6));
        // 6 + 5 is over 10: refused, with nothing taken and nothing run.
        let second = start(&limiter, rt.handle(), gate.calls(5, "b", &live, &ran), long).wait();
        assert!(
            refused_for(&second, "calls are already in flight"),
            "{second:?}"
        );
        assert_eq!((limiter.in_flight(), limiter.calls_in_flight()), (1, 6));
        assert_eq!(live.load(Ordering::SeqCst), 6, "the refused calls are gone");
        assert_eq!(
            limiter.log.pending(),
            Tally {
                full: 0,
                spent: 1,
                too_big: 0
            }
        );
        // 6 + 4 is exactly 10: it fits. One call more does not.
        let third = start(&limiter, rt.handle(), gate.calls(4, "c", &live, &ran), long);
        assert!(was_sent(&third));
        assert_eq!((limiter.in_flight(), limiter.calls_in_flight()), (2, 10));
        let fourth = start(&limiter, rt.handle(), gate.calls(1, "d", &live, &ran), long).wait();
        assert!(refused_for(&fourth, "calls are already in flight"));
        assert_eq!(limiter.calls_in_flight(), 10);
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "nothing ran while the gate was shut"
        );
        gate.open();
        assert!(first.wait().iter().all(|(_, r)| r.is_ok()));
        assert!(third.wait().iter().all(|(_, r)| r.is_ok()));
        wait_until("everything is given back", || {
            limiter.calls_in_flight() == 0 && limiter.in_flight() == 0
        });
        assert_eq!(ran.load(Ordering::SeqCst), 10, "only the two reads ran");
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    // REQ: CLU-004 (ADR-174) — a call's share goes back to the budget when its task ends, not
    // only when the read's last one does: a read held up by one silent peer is not holding the
    // shares of the peers that answered. The read's place stays until its last task ends.
    #[test]
    fn clu_004_a_call_gives_its_share_back_when_its_task_ends() {
        let rt = runtime(2);
        let limiter = Limiter::with_budget(8, 100);
        let (live, ran, gate) = (count(), count(), Gate::shut());
        let mut calls = gate.calls(2, "held", &live, &ran);
        calls.push(("quick".into(), answer("Q")));
        let read = start(&limiter, rt.handle(), calls, Duration::from_secs(60));
        assert!(was_sent(&read));
        wait_until("the quick call's share is back", || {
            limiter.calls_in_flight() == 2
        });
        assert_eq!(limiter.in_flight(), 1, "the read still holds its place");
        gate.open();
        let got = read.wait();
        assert_eq!(got[2], ("quick".to_owned(), Ok(b"Q".to_vec())));
        assert!(got.iter().all(|(_, r)| r.is_ok()), "{got:?}");
        wait_until("everything is given back", || {
            limiter.calls_in_flight() == 0 && limiter.in_flight() == 0
        });
    }

    // REQ: CLU-004 (ADR-174) — tasks queued on a stuck runtime keep their shares and the place,
    // after the caller has given up, and give them back when the runtime runs them and finds
    // nobody waiting (none of the calls is sent). Same pattern as the place under the cap of reads.
    #[test]
    fn clu_004_shares_stay_with_queued_tasks_after_the_caller_gives_up() {
        let rt = runtime(1);
        let limiter = Limiter::with_budget(8, 100);
        let gate = wedge(&rt);
        let (live, ran, door) = (count(), count(), Gate::shut());
        door.open();
        let calls = door.calls(50, "q", &live, &ran);
        let got = start(&limiter, rt.handle(), calls, Duration::from_millis(100)).wait();
        assert_eq!(got.len(), 50);
        assert!(
            got.iter().all(|(_, r)| r
                .as_ref()
                .is_err_and(|e| e.contains("didn't answer in time"))),
            "{:?}",
            got.first()
        );
        // The caller is gone; the tasks are queued behind the wedge and still count.
        assert_eq!(limiter.calls_in_flight(), 50);
        assert_eq!(limiter.in_flight(), 1);
        gate.send(()).unwrap();
        wait_until("the queued tasks end", || {
            limiter.calls_in_flight() == 0 && limiter.in_flight() == 0
        });
        assert_eq!(ran.load(Ordering::SeqCst), 0, "no call was sent late");
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    // REQ: CLU-004 (ADR-174) — a runtime that is shutting down drops its tasks without running
    // them: their shares and the place come back, and so does a spawn onto a runtime that is
    // already gone.
    #[test]
    fn clu_004_shares_come_back_when_the_runtime_drops_the_tasks() {
        let rt = runtime(2);
        let limiter = Limiter::with_budget(8, 100);
        let (live, ran, gate) = (count(), count(), Gate::shut());
        let read = start(
            &limiter,
            rt.handle(),
            gate.calls(40, "p", &live, &ran),
            Duration::from_secs(60),
        );
        assert_eq!(limiter.calls_in_flight(), 40);
        let handle = rt.handle().clone();
        drop(rt);
        let got = read.wait();
        assert!(
            got.iter().all(|(_, r)| r
                .as_ref()
                .is_err_and(|e| e.contains("its call was dropped"))),
            "{:?}",
            got.first()
        );
        wait_until("the dropped tasks gave everything back", || {
            limiter.calls_in_flight() == 0 && limiter.in_flight() == 0
        });
        assert_eq!(live.load(Ordering::SeqCst), 0);
        // A read started on the dead runtime: its tasks are dropped as they are spawned.
        let late = start(
            &limiter,
            &handle,
            gate.calls(10, "late", &live, &ran),
            Duration::from_secs(60),
        )
        .wait();
        assert_eq!(late.len(), 10);
        wait_until("a spawn on a dead runtime gave everything back", || {
            limiter.calls_in_flight() == 0 && limiter.in_flight() == 0
        });
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    // REQ: CLU-004 (ADR-174) — the worst case never holds more than the budget. Thirty-two
    // reads of the ceiling are tried (131,072 calls): eight are sent, and the calls that exist,
    // counted by the calls themselves and not by the limiter, are exactly the budget: 32,768,
    // what 32 reads of the old flat cap could hold. With the old reads (32 of 1,024) all
    // 32 are sent, which is the budget again, and a 33rd is refused for want of a place.
    #[test]
    fn clu_004_the_worst_case_never_holds_more_than_the_budget() {
        let rt = runtime(2);
        let long = Duration::from_secs(60);
        // Big reads: the budget is what stops them.
        let limiter = Limiter::new(MAX_WAITS);
        let (live, ran, gate) = (count(), count(), Gate::shut());
        let mut sent = 0;
        let mut held = Vec::new();
        for r in 0..MAX_WAITS {
            let calls = gate.calls(MAX_READ_CALLS, &format!("r{r}-"), &live, &ran);
            let read = start(&limiter, rt.handle(), calls, long);
            if was_sent(&read) {
                sent += 1;
                held.push(read);
            }
            assert!(limiter.calls_in_flight() <= CALL_BUDGET);
            assert!(
                live.load(Ordering::SeqCst) <= CALL_BUDGET,
                "{} calls exist",
                live.load(Ordering::SeqCst)
            );
        }
        assert_eq!(sent, CALL_BUDGET / MAX_READ_CALLS);
        assert_eq!(live.load(Ordering::SeqCst), CALL_BUDGET);
        assert_eq!(limiter.calls_in_flight(), CALL_BUDGET);
        gate.open();
        for read in held {
            assert!(read.wait().iter().all(|(_, r)| r.is_ok()));
        }
        wait_until("everything is given back", || {
            limiter.calls_in_flight() == 0 && limiter.in_flight() == 0
        });
        assert_eq!(live.load(Ordering::SeqCst), 0);
        // Small reads: the places are what stops them, at the same total.
        let limiter = Limiter::new(MAX_WAITS);
        let gate = Gate::shut();
        let mut held = Vec::new();
        for r in 0..MAX_WAITS {
            let calls = gate.calls(1024, &format!("s{r}-"), &live, &ran);
            let read = start(&limiter, rt.handle(), calls, long);
            assert!(was_sent(&read), "read {r}");
            held.push(read);
        }
        assert_eq!(live.load(Ordering::SeqCst), CALL_BUDGET);
        assert_eq!(limiter.calls_in_flight(), CALL_BUDGET);
        let more = start(
            &limiter,
            rt.handle(),
            gate.calls(1, "more", &live, &ran),
            long,
        )
        .wait();
        assert!(refused_for(&more, "reads are already waiting"), "{more:?}");
        assert_eq!(live.load(Ordering::SeqCst), CALL_BUDGET);
        gate.open();
        for read in held {
            assert!(read.wait().iter().all(|(_, r)| r.is_ok()));
        }
        wait_until("everything is given back", || {
            limiter.calls_in_flight() == 0 && limiter.in_flight() == 0
        });
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    // REQ: CLU-004 (ADR-174) — from many threads at once, reads of every size never get past
    // the budget or the places; a refused read leaves nothing behind, and what was sent is exactly
    // what the limiter says is in flight. A thread watches the counts all the while.
    #[test]
    fn clu_004_reads_started_from_many_threads_never_pass_the_budget() {
        const SIZES: [usize; 6] = [1, 50, 1_000, 4_096, 4_096, 3_000];
        let rt = runtime(2);
        let limiter = Arc::new(Limiter::new(MAX_WAITS));
        let (live, ran, gate) = (count(), count(), Arc::new(Gate::shut()));
        let watching = Arc::new(AtomicBool::new(true));
        // A failed assertion below must not leave the watcher spinning.
        let _stop = StopOnDrop(Arc::clone(&watching));
        let watcher = {
            let (limiter, watching) = (Arc::clone(&limiter), Arc::clone(&watching));
            std::thread::spawn(move || {
                let mut worst = (0, 0);
                while watching.load(Ordering::SeqCst) {
                    let (calls, reads) = (limiter.calls_in_flight(), limiter.in_flight());
                    worst = (worst.0.max(calls), worst.1.max(reads));
                    std::thread::yield_now();
                }
                worst
            })
        };
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let (limiter, gate, live, ran) = (
                    Arc::clone(&limiter),
                    Arc::clone(&gate),
                    Arc::clone(&live),
                    Arc::clone(&ran),
                );
                let handle = rt.handle().clone();
                std::thread::spawn(move || {
                    let (mut held, mut accepted) = (Vec::new(), 0);
                    for i in 0..30 {
                        let n = SIZES[(t * 7 + i * 5) % SIZES.len()];
                        let calls = gate.calls(n, &format!("t{t}-{i}-"), &live, &ran);
                        let read = start(&limiter, &handle, calls, Duration::from_secs(60));
                        if was_sent(&read) {
                            accepted += n;
                            held.push(read);
                        }
                        assert!(limiter.calls_in_flight() <= CALL_BUDGET);
                    }
                    (held, accepted)
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let accepted: usize = results.iter().map(|(_, n)| n).sum();
        let reads: usize = results.iter().map(|(h, _)| h.len()).sum();
        assert!(accepted <= CALL_BUDGET && reads <= MAX_WAITS);
        assert_eq!(limiter.calls_in_flight(), accepted, "what was sent is held");
        assert_eq!(limiter.in_flight(), reads);
        assert_eq!(
            live.load(Ordering::SeqCst),
            accepted,
            "the calls that exist are the ones that were sent"
        );
        // A read that was refused is not lost to the budget: it is the room that ran out.
        assert!(accepted > CALL_BUDGET - MAX_READ_CALLS || reads == MAX_WAITS);
        gate.open();
        for (held, _) in results {
            for read in held {
                assert!(read.wait().iter().all(|(_, r)| r.is_ok()));
            }
        }
        wait_until("everything is given back", || {
            limiter.calls_in_flight() == 0 && limiter.in_flight() == 0
        });
        watching.store(false, Ordering::SeqCst);
        let (calls, places) = watcher.join().unwrap();
        assert!(calls <= CALL_BUDGET, "{calls} calls in flight");
        assert!(places <= MAX_WAITS, "{places} reads waiting");
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    // REQ: CLU-004 (final review, F2) — a refused read is said out loud (a warning) the first
    // time, then at most once a minute, with how many were refused for each reason since the
    // last line. The clock is the test's own.
    #[test]
    fn clu_004_refused_reads_are_logged_once_a_minute_with_their_count() {
        let log = RefusalLog::default();
        let tally = |full, spent, too_big| {
            Some(Tally {
                full,
                spent,
                too_big,
            })
        };
        // The first one is said at once.
        assert_eq!(log.note(Refused::Full, 5), tally(1, 0, 0));
        // Then a minute of counting and no line.
        assert_eq!(log.note(Refused::Full, 6), None);
        assert_eq!(log.note(Refused::TooBig, 30_000), None);
        assert_eq!(log.note(Refused::Spent, 40_000), None);
        assert_eq!(log.note(Refused::Full, 59_004), None);
        // A minute after the last line, one line with what came in since (and this one).
        assert_eq!(log.note(Refused::TooBig, 60_005), tally(2, 1, 2));
        assert_eq!(log.pending(), Tally::default(), "a line resets the count");
        // And quiet for another minute.
        assert_eq!(log.note(Refused::Full, 60_006), None);
        assert_eq!(log.note(Refused::Full, 120_004), None);
        assert_eq!(log.note(Refused::Spent, 120_005), tally(2, 1, 0));
    }

    // REQ: CLU-004 (final review, F2) — however many threads refuse at once, no refusal is lost
    // or counted twice: what the lines said, and what is still waiting for the next line, add up
    // to every refusal.
    #[test]
    fn clu_004_refusals_are_counted_exactly_under_concurrency() {
        let log = Arc::new(RefusalLog::default());
        let (threads, each) = (8_u64, 2_000_u64);
        let said: u64 = (0..threads)
            .map(|t| {
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    let mut said = 0;
                    for i in 0..each {
                        // A virtual second per refusal, so a line is due about every 60th.
                        let why = match (t + i) % 3 {
                            0 => Refused::TooBig,
                            1 => Refused::Spent,
                            _ => Refused::Full,
                        };
                        if let Some(n) = log.note(why, i * 1000) {
                            said += n.full + n.spent + n.too_big;
                        }
                    }
                    said
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .sum();
        let waiting = log.pending();
        assert_eq!(
            said + waiting.full + waiting.spent + waiting.too_big,
            threads * each
        );
    }

    // REQ: CLU-004 (final review, F2) — a read that `start` does not send is counted for the
    // line, for each of the three reasons.
    #[test]
    fn clu_004_a_refused_read_is_counted_for_the_log() {
        let rt = runtime(2);
        // One place, and room for two calls.
        let limiter = Limiter::with_budget(1, 2);
        // The first refusal is the one that is said at once; take it, so the next are only counted.
        let _ = limiter.log.note(Refused::Full, 0);
        let (open, shut) = tokio::sync::oneshot::channel::<()>();
        let held: Vec<(String, Call)> = vec![(
            "held".into(),
            Box::pin(async move {
                let _ = shut.await;
                Ok(b"H".to_vec())
            }),
        )];
        let first = start(&limiter, rt.handle(), held, Duration::from_secs(30));
        let sent = count();
        // Over the cap of reads.
        let _ = start(
            &limiter,
            rt.handle(),
            vec![("late".into(), counting(&sent))],
            Duration::from_secs(30),
        )
        .wait();
        assert_eq!(
            limiter.log.pending(),
            Tally {
                full: 1,
                spent: 0,
                too_big: 0
            }
        );
        open.send(()).unwrap();
        assert_eq!(first.wait()[0].1, Ok(b"H".to_vec()));
        wait_until("the place is given back", || limiter.in_flight() == 0);
        // Over the budget of calls: three calls, with room for two.
        let three: Vec<(String, Call)> =
            (0..3).map(|i| (format!("q{i}"), counting(&sent))).collect();
        let _ = start(&limiter, rt.handle(), three, Duration::from_secs(30)).wait();
        assert_eq!(
            limiter.log.pending(),
            Tally {
                full: 1,
                spent: 1,
                too_big: 0
            }
        );
        // Over the ceiling on the calls of a read.
        let calls: Vec<(String, Call)> = (0..=MAX_READ_CALLS)
            .map(|i| (format!("p{i}"), counting(&sent)))
            .collect();
        let _ = start(&limiter, rt.handle(), calls, Duration::from_secs(30)).wait();
        assert_eq!(
            limiter.log.pending(),
            Tally {
                full: 1,
                spent: 1,
                too_big: 1
            }
        );
        assert_eq!(sent.load(Ordering::SeqCst), 0);
    }

    // REQ: CLU-004 (ADR-174) — any calling context: from a current-thread runtime's own thread
    // (nothing else to run the call) the wait times out and returns; it neither panics nor
    // hangs. Before: a deadlock.
    #[test]
    fn clu_004_the_bridge_returns_from_a_current_thread_runtime() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let limiter = Limiter::new(8);
        let got = rt.block_on(async {
            let calls: Vec<(String, Call)> = vec![("peer".into(), answer("never run"))];
            start(
                &limiter,
                &tokio::runtime::Handle::current(),
                calls,
                Duration::from_millis(50),
            )
            .wait()
        });
        assert!(
            got[0]
                .1
                .as_ref()
                .unwrap_err()
                .contains("didn't answer in time"),
            "{got:?}"
        );
    }
}
