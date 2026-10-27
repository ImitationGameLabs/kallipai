//! The upstream name resolver: one bounded lookup per host, cached for a
//! short fresh window and served stale out of a longer one.
//!
//! Every forwarded request needs an address for its credential's host.
//! Resolving per request costs a system lookup each time and turns a
//! resolver hiccup into a 503; this module answers from a positive cache,
//! collapses the concurrent lookups for one host into a single query, and
//! keeps serving the last good answer for [`STALE_WINDOW`] past its fresh
//! life when a refresh fails. A failed refresh is still counted -- the
//! dashboard has to see the resolver going bad -- and cools the host down
//! for [`UPSTREAM_RETRY_AFTER`], so one outage costs one query per window
//! instead of one per request.
//!
//! The key is the host alone: a name's address set does not depend on the
//! port, so the caller re-attaches its own port to the answer. A literal
//! address short-circuits before the cache -- there is nothing to look up.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::metrics::{Metrics, ResolveErrorClass};

/// How long one system lookup may take before it counts as a timeout: a dead
/// resolver must not hold a request open, while a slow but live one still has
/// to land. The value is fixed rather than configurable until an operator asks
/// for a knob.
const UPSTREAM_RESOLVE_BOUND: Duration = Duration::from_secs(5);

/// How long a resolved answer stays fresh: inside this window no lookup
/// runs.
const POSITIVE_TTL: Duration = Duration::from_secs(30);

/// How long a stale answer keeps serving past its fresh life. A refresh
/// that fails inside this window leaves the last good addresses in service:
/// a resolver outage degrades to the addresses that worked a minute ago
/// instead of to a 503.
const STALE_WINDOW: Duration = Duration::from_secs(60);

/// The refresh cooldown after a failed lookup. One number carries both the
/// window a failed lookup stays cached for -- the same window gates the next
/// attempt -- and the 503 retry hint, so the two cannot drift. Five seconds is
/// long enough to outlast a resolver hiccup, short enough that a client
/// honouring the hint does not back off for a whole outage.
pub(crate) const UPSTREAM_RETRY_AFTER: Duration = Duration::from_secs(5);

/// The cache key bound. Keys come from configured credentials -- a bounded
/// set -- so the cap only guards against a configuration mistake.
const MAX_ENTRIES: usize = 256;

/// How long a waiter waits for another caller's in-flight lookup before it
/// takes the refresh over. The margin over the bound covers a lookup that
/// answers just as its own timeout fires.
const WAIT_GRACE: Duration = Duration::from_secs(1);

/// A failed resolution: the class the metric counts plus the resolver's own
/// text, which the failure line records.
#[derive(Clone, Debug)]
pub(crate) struct ResolveFailure {
    pub(crate) class: ResolveErrorClass,
    pub(crate) detail: String,
}

/// The query boundary: the real one asks the system resolver, tests inject
/// a scripted one. A trait, so an implementation that honours authoritative
/// TTLs later replaces the query without touching the cache.
#[async_trait]
pub(crate) trait Lookup: Send + Sync {
    async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>>;
}

/// The system resolver: `getaddrinfo`, so `/etc/hosts`, search domains and a
/// local stub resolver are consulted. A change they report reaches a cached
/// host once the fresh window above has passed, or later while a failing
/// refresh keeps the last answer inside its stale window.
struct SystemLookup;

#[async_trait]
impl Lookup for SystemLookup {
    async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        // Port 0: the answer's port is the caller's business. Duplicates are
        // the cache's business, so they are filtered there, not here.
        Ok(tokio::net::lookup_host((host, 0))
            .await?
            .map(|address| address.ip())
            .collect())
    }
}

/// One host's cache line: the last answer, the refresh cooldown, and the
/// caller currently querying for it.
#[derive(Default)]
struct Entry {
    last: Option<Answer>,
    negative: Option<Negative>,
    refresh: Option<Arc<Notify>>,
}

/// A resolved address set with the two deadlines it serves under.
#[derive(Clone)]
struct Answer {
    addresses: Arc<[IpAddr]>,
    fresh_until: Instant,
    stale_until: Instant,
}

/// A failed lookup, remembered as the refresh cooldown: the read path serves
/// the stale answer (when there is one) and replays this failure otherwise.
struct Negative {
    class: ResolveErrorClass,
    detail: String,
    until: Instant,
}

/// What a read of the cache decides.
enum Decision {
    /// Answer from the cache.
    Serve(Arc<[IpAddr]>),
    /// Replay a remembered failure.
    Fail(ResolveFailure),
    /// Query, owning the refresh slot.
    Query(Arc<Notify>),
    /// Another caller is querying; wait for its wake-up.
    Wait(Arc<Notify>),
}

/// The process-local resolution cache: one entry per host, one query per
/// entry at a time, and a stale window that keeps a failing resolver from
/// turning into a 503 for every request behind it.
pub(crate) struct Resolver {
    entries: Mutex<HashMap<String, Entry>>,
    metrics: Metrics,
    bound: Duration,
    lookup: Arc<dyn Lookup>,
}

impl Resolver {
    /// The production resolver over the system lookups.
    pub(crate) fn system(metrics: Metrics) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            metrics,
            bound: UPSTREAM_RESOLVE_BOUND,
            lookup: Arc::new(SystemLookup),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_lookup(metrics: Metrics, bound: Duration, lookup: Arc<dyn Lookup>) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            metrics,
            bound,
            lookup,
        }
    }

    /// The addresses for `host`, cached and collapsed. `label` is the
    /// metric's `upstream` label: the caller's authority text, passed in
    /// rather than rebuilt here.
    /// On success the slice holds at least one address, so a caller may take
    /// `first()` without carrying a panic path of its own.
    pub(crate) async fn resolve(
        &self,
        host: &str,
        label: &str,
    ) -> Result<Arc<[IpAddr]>, ResolveFailure> {
        if let Ok(literal) = host.parse::<IpAddr>() {
            return Ok(Arc::from(vec![literal]));
        }
        let mut waited: Option<Arc<Notify>> = None;
        loop {
            match self.decide(host, waited.as_ref()) {
                Decision::Serve(addresses) => return Ok(addresses),
                Decision::Fail(failure) => return Err(failure),
                Decision::Query(mine) => return self.refresh(host, label, mine).await,
                Decision::Wait(in_flight) => {
                    let notified = in_flight.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    // The slot may have been written back and notified
                    // between the decision above and the registration here;
                    // the recheck turns that into another read instead of a
                    // wait for a wake-up that already happened.
                    if !self.is_refreshing(host, &in_flight) {
                        continue;
                    }
                    let _ = tokio::time::timeout(self.bound + WAIT_GRACE, notified).await;
                    waited = Some(Arc::clone(&in_flight));
                }
            }
        }
    }

    /// Read the cache line and decide. Every `Decision` other than `Query`
    /// leaves the map untouched: only a caller that takes a slot inserts a line.
    fn decide(&self, host: &str, waited: Option<&Arc<Notify>>) -> Decision {
        let now = Instant::now();
        // A poisoned lock reads as a miss: one uncached query, no panic.
        let Ok(mut entries) = self.entries.lock() else {
            return Decision::Query(Arc::new(Notify::new()));
        };
        // The hit path borrows the line the map already holds: a served
        // request must not pay for a key it has.
        if let Some(entry) = entries.get_mut(host) {
            return Self::decide_on(entry, now, waited);
        }
        let mine = Arc::new(Notify::new());
        entries.entry(host.to_owned()).or_default().refresh = Some(Arc::clone(&mine));
        Decision::Query(mine)
    }

    /// The decision inside one existing line. `Query` means the caller now
    /// owns the slot it returns.
    fn decide_on(entry: &mut Entry, now: Instant, waited: Option<&Arc<Notify>>) -> Decision {
        if let Some(answer) = &entry.last
            && now < answer.fresh_until
        {
            return Decision::Serve(answer.addresses.clone());
        }
        if let Some(negative) = &entry.negative
            && now < negative.until
        {
            if let Some(answer) = &entry.last
                && now < answer.stale_until
            {
                return Decision::Serve(answer.addresses.clone());
            }
            return Decision::Fail(ResolveFailure {
                class: negative.class,
                detail: negative.detail.clone(),
            });
        }
        if let Some(in_flight) = entry.refresh.clone() {
            if waited.is_some_and(|waited| Arc::ptr_eq(&in_flight, waited)) {
                // Takeover. The timers keep this off the normal path: a
                // live query returns at `bound` at the latest and a waiter
                // only reaches `bound + grace`, so the owner writes back
                // first. Two things break that: a runtime stall or lock
                // wait longer than the grace, and a dangling slot -- an
                // owner dropped or cancelled mid-query. On the first, an
                // owner that still writes back is kept out by `ptr_eq`, so
                // the cost is one extra query; on the second there is no
                // write-back to lose and the slot simply passes to the
                // next caller. The `ptr_eq` check guards both.
                let mine = Arc::new(Notify::new());
                entry.refresh = Some(Arc::clone(&mine));
                return Decision::Query(mine);
            }
            return Decision::Wait(in_flight);
        }
        let mine = Arc::new(Notify::new());
        entry.refresh = Some(Arc::clone(&mine));
        Decision::Query(mine)
    }

    fn is_refreshing(&self, host: &str, expected: &Arc<Notify>) -> bool {
        let Ok(entries) = self.entries.lock() else {
            return false;
        };
        entries
            .get(host)
            .and_then(|entry| entry.refresh.as_ref())
            .is_some_and(|current| Arc::ptr_eq(current, expected))
    }

    /// Run the query for a slot this caller owns, write the outcome back,
    /// and wake the waiters.
    async fn refresh(
        &self,
        host: &str,
        label: &str,
        mine: Arc<Notify>,
    ) -> Result<Arc<[IpAddr]>, ResolveFailure> {
        let outcome = self.query(host, label).await;
        let served = self.commit(host, &mine, &outcome);
        // Waking a waiter whose slot we no longer own only costs it one more
        // read: every waiter re-reads the line after a wake-up.
        mine.notify_waiters();
        served
    }

    /// Write `outcome` into the line this caller owns. A caller that lost
    /// the slot writes nothing and returns its own result.
    fn commit(
        &self,
        host: &str,
        mine: &Arc<Notify>,
        outcome: &Result<Arc<[IpAddr]>, ResolveFailure>,
    ) -> Result<Arc<[IpAddr]>, ResolveFailure> {
        let now = Instant::now();
        let Ok(mut entries) = self.entries.lock() else {
            return outcome.clone();
        };
        let served = {
            let Some(entry) = entries.get_mut(host) else {
                return outcome.clone();
            };
            let owned = entry
                .refresh
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, mine));
            if !owned {
                return outcome.clone();
            }
            entry.refresh = None;
            match outcome {
                Ok(addresses) => {
                    entry.last = Some(Answer {
                        addresses: addresses.clone(),
                        fresh_until: now + POSITIVE_TTL,
                        stale_until: now + POSITIVE_TTL + STALE_WINDOW,
                    });
                    entry.negative = None;
                    outcome.clone()
                }
                Err(failure) => {
                    entry.negative = Some(Negative {
                        class: failure.class,
                        detail: failure.detail.clone(),
                        until: now + UPSTREAM_RETRY_AFTER,
                    });
                    // A stale answer keeps serving through the cooldown: the
                    // caller that asked for a refresh gets the last good
                    // addresses, not the failure.
                    match entry
                        .last
                        .as_ref()
                        .filter(|answer| now < answer.stale_until)
                    {
                        Some(answer) => Ok(answer.addresses.clone()),
                        None => outcome.clone(),
                    }
                }
            }
        };
        prune(&mut entries, host);
        served
    }

    /// One bounded lookup, mapped into the failure vocabulary the metric
    /// counts. Every failure is recorded here, once: a stale answer served
    /// later replays the failure without counting it again.
    /// The answer is de-duplicated here, so every `Lookup` implementation
    /// inherits one entry per address.
    async fn query(&self, host: &str, label: &str) -> Result<Arc<[IpAddr]>, ResolveFailure> {
        let answered = tokio::time::timeout(self.bound, self.lookup.lookup(host)).await;
        let addresses = match answered {
            Err(_) => {
                return Err(self.failed(
                    label,
                    ResolveErrorClass::Timeout,
                    format!("the resolver did not answer within {:?}", self.bound),
                ));
            }
            Ok(Err(error)) => {
                return Err(self.failed(label, ResolveErrorClass::LookupError, error.to_string()));
            }
            Ok(Ok(addresses)) => addresses,
        };
        let mut unique: Vec<IpAddr> = Vec::with_capacity(addresses.len());
        for address in addresses {
            if !unique.contains(&address) {
                unique.push(address);
            }
        }
        if unique.is_empty() {
            return Err(self.failed(
                label,
                ResolveErrorClass::NoAddresses,
                "the resolver returned no address".to_string(),
            ));
        }
        Ok(Arc::from(unique))
    }

    fn failed(&self, label: &str, class: ResolveErrorClass, detail: String) -> ResolveFailure {
        self.metrics.record_resolution_failure(label, class);
        ResolveFailure { class, detail }
    }
}

/// Drop the lines with nothing left to say, then evict the ones closest to
/// the end of their useful life. A line with a lookup in flight is left
/// alone: its write-back is the only thing that can fill it. The key set
/// is configuration-bounded, so this is a guard rather than a working set.
fn prune(entries: &mut HashMap<String, Entry>, keep: &str) {
    if entries.len() <= MAX_ENTRIES {
        return;
    }
    let now = Instant::now();
    entries.retain(|key, entry| {
        key.as_str() == keep
            || entry
                .last
                .as_ref()
                .is_some_and(|answer| now < answer.stale_until)
            || entry
                .negative
                .as_ref()
                .is_some_and(|negative| now < negative.until)
            || entry.refresh.is_some()
    });
    while entries.len() > MAX_ENTRIES {
        let victim = entries
            .iter()
            .filter(|(key, _)| key.as_str() != keep)
            .filter(|(_, entry)| entry.refresh.is_none())
            .min_by_key(|(_, entry)| entry.last.as_ref().map(|answer| answer.stale_until))
            .map(|(key, _)| key.clone());
        let Some(victim) = victim else { break };
        entries.remove(&victim);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::future::pending;

    const HOST: &str = "upstream.test";
    const LABEL: &str = "upstream.test:443";

    /// One scripted answer. The variants are the resolver behaviours the
    /// cache has to survive; a missing answer hangs like a dead resolver.
    enum ScriptedAnswer {
        Addresses(Vec<IpAddr>),
        Error,
        /// Never answer.
        Hang,
    }

    #[derive(Default)]
    struct Scripted {
        answers: Mutex<VecDeque<ScriptedAnswer>>,
        lookups: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn answer(&self, answer: ScriptedAnswer) {
            self.answers
                .lock()
                .expect("scripted answers")
                .push_back(answer);
        }

        fn lookups(&self) -> usize {
            self.lookups.lock().expect("scripted lookups").len()
        }

        fn hosts(&self) -> Vec<String> {
            self.lookups.lock().expect("scripted lookups").clone()
        }
    }

    #[async_trait]
    impl Lookup for Scripted {
        async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>> {
            self.lookups
                .lock()
                .expect("scripted lookups")
                .push(host.to_owned());
            let answer = self.answers.lock().expect("scripted answers").pop_front();
            match answer {
                Some(ScriptedAnswer::Addresses(addresses)) => {
                    // One yield lets the other callers of this host register
                    // as waiters before this answer lands.
                    tokio::task::yield_now().await;
                    Ok(addresses)
                }
                Some(ScriptedAnswer::Error) => {
                    tokio::task::yield_now().await;
                    Err(io::Error::other("scripted lookup failure"))
                }
                Some(ScriptedAnswer::Hang) | None => pending().await,
            }
        }
    }

    fn address(last: u8) -> IpAddr {
        IpAddr::from([192, 0, 2, last])
    }

    fn harness(bound: Duration) -> (Resolver, Arc<Scripted>, Metrics) {
        let scripted = Arc::new(Scripted::default());
        let metrics = Metrics::private();
        let resolver = Resolver::with_lookup(metrics.clone(), bound, scripted.clone());
        (resolver, scripted, metrics)
    }

    /// Drive the run queue until the scripted resolver has been asked at
    /// least `expected` times.
    async fn until_lookups(scripted: &Scripted, expected: usize) {
        while scripted.lookups() < expected {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_cached_answer_avoids_a_second_lookup() {
        let (resolver, scripted, _metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Addresses(vec![address(1)]));
        let first = resolver.resolve(HOST, LABEL).await.expect("an answer");
        let second = resolver
            .resolve(HOST, LABEL)
            .await
            .expect("the cached answer");
        assert_eq!(first.to_vec(), vec![address(1)]);
        assert_eq!(second.to_vec(), first.to_vec());
        assert_eq!(scripted.lookups(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_entry_is_refreshed() {
        let (resolver, scripted, _metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Addresses(vec![address(1)]));
        scripted.answer(ScriptedAnswer::Addresses(vec![address(2)]));
        let first = resolver.resolve(HOST, LABEL).await.expect("an answer");
        tokio::time::advance(POSITIVE_TTL + Duration::from_secs(1)).await;
        let second = resolver
            .resolve(HOST, LABEL)
            .await
            .expect("the refreshed answer");
        assert_eq!(first.to_vec(), vec![address(1)]);
        assert_eq!(second.to_vec(), vec![address(2)]);
        assert_eq!(scripted.lookups(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_share_one_lookup() {
        let (resolver, scripted, _metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Addresses(vec![address(1)]));
        let resolver = Arc::new(resolver);
        let mut callers = Vec::new();
        let started = tokio::time::Instant::now();
        for _ in 0..10 {
            let resolver = Arc::clone(&resolver);
            callers.push(tokio::spawn(
                async move { resolver.resolve(HOST, LABEL).await },
            ));
        }
        for caller in callers {
            let answer = caller.await.expect("the task").expect("an answer");
            assert_eq!(answer.to_vec(), vec![address(1)]);
        }
        // The nine waiters are woken by the write-back. Were a wake-up
        // missing, each of them would sit out `bound + WAIT_GRACE`, and
        // the paused clock would advance far enough to show it here.
        assert!(
            started.elapsed() < WAIT_GRACE,
            "waiters wake on the write-back, not on the grace timeout"
        );
        assert_eq!(scripted.lookups(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_lookup_is_negatively_cached() {
        let (resolver, scripted, metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Error);
        let first = resolver.resolve(HOST, LABEL).await.expect_err("a failure");
        assert_eq!(first.class, ResolveErrorClass::LookupError);
        assert!(!first.detail.is_empty(), "the failure line needs the text");
        let second = resolver
            .resolve(HOST, LABEL)
            .await
            .expect_err("the cooled-down replay");
        assert_eq!(second.class, ResolveErrorClass::LookupError);
        assert_eq!(scripted.lookups(), 1);
        assert_eq!(
            metrics.resolution_failure_count(LABEL, ResolveErrorClass::LookupError),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_loading_slot_does_not_wedge_waiters() {
        let (resolver, scripted, _metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Hang);
        scripted.answer(ScriptedAnswer::Addresses(vec![address(2)]));
        let resolver = Arc::new(resolver);
        let leader = tokio::spawn({
            let resolver = Arc::clone(&resolver);
            async move { resolver.resolve(HOST, LABEL).await }
        });
        until_lookups(&scripted, 1).await;
        // The caller that owned the slot goes away without writing back:
        // the line keeps a token nobody will notify.
        leader.abort();
        let answer = resolver
            .resolve(HOST, LABEL)
            .await
            .expect("the waiter's own query");
        assert_eq!(answer.to_vec(), vec![address(2)]);
        assert_eq!(scripted.lookups(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_answer_survives_a_failed_refresh() {
        let (resolver, scripted, metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Addresses(vec![address(1)]));
        scripted.answer(ScriptedAnswer::Error);
        resolver.resolve(HOST, LABEL).await.expect("an answer");
        tokio::time::advance(POSITIVE_TTL + Duration::from_secs(1)).await;
        let stale = resolver
            .resolve(HOST, LABEL)
            .await
            .expect("the stale answer");
        assert_eq!(stale.to_vec(), vec![address(1)]);
        assert_eq!(scripted.lookups(), 2);
        assert_eq!(
            metrics.resolution_failure_count(LABEL, ResolveErrorClass::LookupError),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_answer_expires() {
        let (resolver, scripted, _metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Addresses(vec![address(1)]));
        scripted.answer(ScriptedAnswer::Error);
        resolver.resolve(HOST, LABEL).await.expect("an answer");
        tokio::time::advance(POSITIVE_TTL + STALE_WINDOW + Duration::from_secs(1)).await;
        let failure = resolver
            .resolve(HOST, LABEL)
            .await
            .expect_err("the stale window is over");
        assert_eq!(failure.class, ResolveErrorClass::LookupError);
        assert_eq!(scripted.lookups(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_literal_address_skips_the_lookup() {
        let (resolver, scripted, _metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        let answer = resolver
            .resolve("203.0.113.7", LABEL)
            .await
            .expect("the literal");
        assert_eq!(answer.to_vec(), vec![IpAddr::from([203, 0, 113, 7])]);
        assert_eq!(scripted.lookups(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_lookup_that_overruns_the_bound_is_a_timeout() {
        let (resolver, scripted, metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Hang);
        let failure = resolver.resolve(HOST, LABEL).await.expect_err("a timeout");
        assert_eq!(failure.class, ResolveErrorClass::Timeout);
        assert_eq!(
            metrics.resolution_failure_count(LABEL, ResolveErrorClass::Timeout),
            1
        );
        let replay = resolver
            .resolve(HOST, LABEL)
            .await
            .expect_err("the cooled-down timeout");
        assert_eq!(replay.class, ResolveErrorClass::Timeout);
        assert_eq!(scripted.lookups(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_refresh_cools_down_around_the_stale_answer() {
        let (resolver, scripted, metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Addresses(vec![address(1)]));
        scripted.answer(ScriptedAnswer::Error);
        resolver.resolve(HOST, LABEL).await.expect("an answer");
        tokio::time::advance(POSITIVE_TTL + Duration::from_secs(1)).await;
        resolver
            .resolve(HOST, LABEL)
            .await
            .expect("the failed refresh serves the stale answer");
        let cooled = resolver
            .resolve(HOST, LABEL)
            .await
            .expect("the cooldown serves the stale answer");
        assert_eq!(cooled.to_vec(), vec![address(1)]);
        assert_eq!(scripted.lookups(), 2, "the cooldown must not query");
        assert_eq!(
            metrics.resolution_failure_count(LABEL, ResolveErrorClass::LookupError),
            1,
            "one outage is one failure, not one per request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn waiters_behind_a_timed_out_waiter_get_the_fresh_answer() {
        let (resolver, scripted, _metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Hang);
        scripted.answer(ScriptedAnswer::Addresses(vec![address(3)]));
        let resolver = Arc::new(resolver);
        let leader = tokio::spawn({
            let resolver = Arc::clone(&resolver);
            async move { resolver.resolve(HOST, LABEL).await }
        });
        until_lookups(&scripted, 1).await;
        leader.abort();
        // Two waiters outlive the owner: the one that takes the slot over
        // performs the second lookup, the other wakes to the fresh answer.
        let (first, second) =
            tokio::join!(resolver.resolve(HOST, LABEL), resolver.resolve(HOST, LABEL));
        assert_eq!(first.expect("the takeover").to_vec(), vec![address(3)]);
        assert_eq!(
            second.expect("the waiter behind the takeover").to_vec(),
            vec![address(3)]
        );
        assert_eq!(scripted.lookups(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_does_not_take_over_from_a_live_owner() {
        let (resolver, scripted, metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Hang);
        let resolver = Arc::new(resolver);
        let owner = tokio::spawn({
            let resolver = Arc::clone(&resolver);
            async move { resolver.resolve(HOST, LABEL).await }
        });
        until_lookups(&scripted, 1).await;
        // The owner's own bound fires before the waiter's grace runs out,
        // so the failure lands first and the waiter wakes to it instead of
        // taking a slot the owner still holds.
        let waiter = resolver
            .resolve(HOST, LABEL)
            .await
            .expect_err("the owner's failure");
        assert!(matches!(waiter.class, ResolveErrorClass::Timeout));
        let owned = owner
            .await
            .expect("the owner")
            .expect_err("its own overrun");
        assert!(matches!(owned.class, ResolveErrorClass::Timeout));
        assert_eq!(scripted.lookups(), 1);
        assert_eq!(
            metrics.resolution_failure_count(LABEL, ResolveErrorClass::Timeout),
            1,
            "one overrun is one failure, not one per waiter"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_writer_does_not_overwrite_a_taken_over_slot() {
        let (resolver, _scripted, _metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        // A write-back whose token a takeover replaced is kept off the
        // public paths by the timers: a query returns at `bound` at the
        // latest, a waiter only reaches `bound + grace`, so the owner
        // writes back first and finds its own slot. A runtime stall or a
        // lock wait longer than that grace breaks the invariant, and that
        // is when this state is reachable -- the late write is then
        // dropped by `ptr_eq` instead of overwriting the takeover. The
        // state is exercised directly here; the request paths cover the
        // rest.
        let owner = Arc::new(Notify::new());
        let late = Arc::new(Notify::new());
        resolver
            .entries
            .lock()
            .expect("entries")
            .entry(HOST.to_owned())
            .or_default()
            .refresh = Some(Arc::clone(&owner));
        let late_answer: Result<Arc<[IpAddr]>, ResolveFailure> = Ok(Arc::from(vec![address(4)]));
        let served = resolver
            .commit(HOST, &late, &late_answer)
            .expect("its own answer");
        assert_eq!(served.to_vec(), vec![address(4)]);
        let entries = resolver.entries.lock().expect("entries");
        let entry = entries.get(HOST).expect("the line");
        assert!(entry.last.is_none(), "a late write must not fill the cache");
        let owner_kept = entry
            .refresh
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &owner));
        assert!(owner_kept, "a late write must not clear the owner's slot");
    }

    #[tokio::test(start_paused = true)]
    async fn the_cache_key_is_the_host_not_the_label() {
        let (resolver, scripted, _metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Addresses(vec![address(1)]));
        let first = resolver
            .resolve(HOST, "upstream.test:443")
            .await
            .expect("an answer");
        let second = resolver
            .resolve(HOST, "upstream.test:8080")
            .await
            .expect("the cached answer");
        assert_eq!(first.to_vec(), vec![address(1)]);
        assert_eq!(second.to_vec(), vec![address(1)]);
        assert_eq!(scripted.lookups(), 1, "the port must stay out of the key");
        assert_eq!(scripted.hosts(), vec![HOST.to_owned()]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_answer_is_a_failure_that_cools_down() {
        let (resolver, scripted, metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Addresses(Vec::new()));
        let failure = resolver
            .resolve(HOST, LABEL)
            .await
            .expect_err("an empty answer is no answer");
        assert!(matches!(failure.class, ResolveErrorClass::NoAddresses));
        let again = resolver
            .resolve(HOST, LABEL)
            .await
            .expect_err("the cooldown replays the failure");
        assert!(matches!(again.class, ResolveErrorClass::NoAddresses));
        assert_eq!(scripted.lookups(), 1, "the cooldown must not query");
        assert_eq!(
            metrics.resolution_failure_count(LABEL, ResolveErrorClass::NoAddresses),
            1,
            "an empty answer is counted once, not once per request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_share_one_failure() {
        let (resolver, scripted, metrics) = harness(UPSTREAM_RESOLVE_BOUND);
        scripted.answer(ScriptedAnswer::Error);
        let resolver = Arc::new(resolver);
        let mut callers = Vec::new();
        for _ in 0..10 {
            let resolver = Arc::clone(&resolver);
            callers.push(tokio::spawn(
                async move { resolver.resolve(HOST, LABEL).await },
            ));
        }
        for caller in callers {
            let failure = caller
                .await
                .expect("the task")
                .expect_err("the scripted failure");
            assert!(matches!(failure.class, ResolveErrorClass::LookupError));
        }
        assert_eq!(scripted.lookups(), 1);
        assert_eq!(
            metrics.resolution_failure_count(LABEL, ResolveErrorClass::LookupError),
            1,
            "one failed lookup is one failure, whatever the fan-out"
        );
    }

    /// The capacity guard drops the cheapest lines to lose and stops when
    /// nothing is droppable: an in-flight line is the only thing that can
    /// fill itself, so it survives; a map where every line is in flight is
    /// left over the cap rather than emptied.
    #[test]
    fn prune_keeps_an_in_flight_line_and_stops_when_nothing_is_droppable() {
        let now = Instant::now();
        let answer = Answer {
            addresses: Arc::from(vec![address(1)]),
            fresh_until: now + POSITIVE_TTL,
            stale_until: now + POSITIVE_TTL + STALE_WINDOW,
        };
        let mut entries: HashMap<String, Entry> = HashMap::new();
        for index in 0..=MAX_ENTRIES {
            entries.insert(
                format!("host-{index}"),
                Entry {
                    last: Some(answer.clone()),
                    ..Entry::default()
                },
            );
        }
        entries.insert(
            "in-flight".to_string(),
            Entry {
                refresh: Some(Arc::new(Notify::new())),
                ..Entry::default()
            },
        );
        prune(&mut entries, "another-host");
        assert!(
            entries.contains_key("in-flight"),
            "an in-flight line must survive eviction"
        );
        assert_eq!(entries.len(), MAX_ENTRIES);

        let mut all_in_flight: HashMap<String, Entry> = HashMap::new();
        for index in 0..=MAX_ENTRIES {
            all_in_flight.insert(
                format!("host-{index}"),
                Entry {
                    refresh: Some(Arc::new(Notify::new())),
                    ..Entry::default()
                },
            );
        }
        prune(&mut all_in_flight, "another-host");
        assert_eq!(
            all_in_flight.len(),
            MAX_ENTRIES + 1,
            "with nothing droppable the guard stops instead of looping"
        );
    }

    /// The retain pass drops expired lines (those past their stale window) and
    /// keeps the ones that are past their fresh window but still inside it,
    /// before the capacity guard has to evict anything.
    #[test]
    fn prune_drops_expired_lines_and_keeps_a_stale_but_live_one() {
        let now = Instant::now();
        let answer = Answer {
            addresses: Arc::from(vec![address(1)]),
            fresh_until: now + POSITIVE_TTL,
            stale_until: now + POSITIVE_TTL + STALE_WINDOW,
        };

        // The retain pass does the work before the cap has to evict: 253 fresh
        // + 1 past its fresh window but still inside its stale window + 2
        // expired + 1 in flight is 257, and the retain drops exactly the two
        // expired lines, leaving 255 -- so the eviction pass never runs. Tighten
        // the answer clause to fresh-only and the stale-but-alive line goes with
        // it (254); replace the whole guard with `has an answer` and the
        // in-flight line goes too (256). Each shape below reds its own
        // assertions.
        let expired = Answer {
            addresses: Arc::from(vec![address(2)]),
            fresh_until: now - POSITIVE_TTL,
            stale_until: now - POSITIVE_TTL,
        };
        let mut with_stale: HashMap<String, Entry> = HashMap::new();
        for index in 0..(MAX_ENTRIES - 3) {
            with_stale.insert(
                format!("fresh-{index}"),
                Entry {
                    last: Some(answer.clone()),
                    ..Entry::default()
                },
            );
        }
        for index in 0..2 {
            with_stale.insert(
                format!("expired-{index}"),
                Entry {
                    last: Some(expired.clone()),
                    ..Entry::default()
                },
            );
        }
        with_stale.insert(
            "stale-but-alive".to_string(),
            Entry {
                last: Some(Answer {
                    addresses: Arc::from(vec![address(3)]),
                    fresh_until: now - POSITIVE_TTL,
                    stale_until: now + STALE_WINDOW,
                }),
                ..Entry::default()
            },
        );
        with_stale.insert(
            "in-flight".to_string(),
            Entry {
                refresh: Some(Arc::new(Notify::new())),
                ..Entry::default()
            },
        );
        prune(&mut with_stale, "another-host");
        assert!(
            !with_stale.keys().any(|key| key.starts_with("expired-")),
            "a line past its stale window is dropped by the retain"
        );
        assert!(
            with_stale.contains_key("stale-but-alive"),
            "a line past its fresh window but inside its stale window must survive the retain"
        );
        assert!(with_stale.contains_key("in-flight"));
        assert_eq!(
            with_stale.len(),
            MAX_ENTRIES - 1,
            "the retain dropped the expired lines, so no eviction was needed"
        );
    }
}
