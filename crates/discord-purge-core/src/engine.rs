//! The deletion engine.
//!
//! It takes a list of per-channel jobs and deletes each item, honoring
//! Discord's rate limits. The design follows the strongest reference tool,
//! `discord-delete`:
//!
//! - Each channel has configurable request spacing (seconds between deletes),
//!   several channels in parallel. Pacing is adaptive (AIMD): a channel widens
//!   its spacing when Discord throttles it and narrows back toward a floor on
//!   clean deletes.
//! - `DiscordClient` keeps an account-wide request floor and a gate for each
//!   Discord bucket, so one channel's exhausted bucket does not hold the others.
//! - A 429 is never an error. The throttled worker waits `retry_after`; a
//!   global 429 pauses every worker. Repeated shared or global 429s collapse
//!   the run to one worker, as a conservative response to repeated throttling.
//!   Shared means per resource, and does not by itself mean account-wide.
//! - A rolling count of invalid responses (401/403/non-shared 429) pauses the
//!   run just under Discord's ~10k-per-10-minutes Cloudflare cap.
//! - A 401 aborts: the token is dead and waiting will not fix it.
//!
//! The front ends (headless, TUI, web) only read a `Snapshot` and send
//! pause/resume/stop signals; all the fragile logic lives here once.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand::Rng;

use crate::api::{DeleteOutcome, DiscordClient, Scope};

/// `items` holds message IDs; each ID is also its key in the resume log.
#[derive(Debug, Clone)]
pub struct ChannelJob {
    pub channel_id: String,
    pub label: String,
    pub items: Vec<u64>,
    /// Discovery read each item's author and pin state from Discord, so the
    /// engine sends the DELETE without a history check first.
    pub verified: bool,
}

/// Runtime knobs for one run. The account-wide request floor is in
/// `DiscordClient`, so every request shares it.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub workers: usize,
    /// Per-channel spacing floor (the fastest we will ever go in one channel).
    pub delay: Duration,
    pub jitter: f64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            workers: 4,
            delay: Duration::from_millis(1100),
            jitter: 0.4,
        }
    }
}

impl EngineConfig {
    pub fn new(workers: usize, delay: f64, jitter: f64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=16).contains(&workers),
            "You set an invalid worker count. Use 1 to 16 workers."
        );
        anyhow::ensure!(
            delay.is_finite() && (0.0..=3600.0).contains(&delay),
            "You set an invalid delay. Use a finite number from 0 to 3600 seconds."
        );
        anyhow::ensure!(
            jitter.is_finite() && (0.0..=1.0).contains(&jitter),
            "You set an invalid jitter. Use a finite fraction from 0 to 1."
        );
        Ok(Self {
            workers,
            delay: Duration::from_secs_f64(delay),
            jitter,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkerStatus {
    pub active: bool,
    pub channel: String,
    pub done: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ForbiddenStat {
    pub count: usize,
    pub reason: String,
    pub label: String,
}

pub struct Stats {
    total: i64,
    deleted: AtomicI64,
    skipped: AtomicI64,
    failed: AtomicI64,
    start: Instant,
    finished: AtomicBool,
    aborted: AtomicBool,
    active_limit: AtomicI64,
    inner: Mutex<StatsInner>,
}

#[derive(Default)]
struct StatsInner {
    workers: Vec<WorkerStatus>,
    errors: Vec<String>,
    status: String,
    forbidden: std::collections::HashMap<String, ForbiddenStat>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub total: i64,
    pub deleted: i64,
    pub skipped: i64,
    pub failed: i64,
    pub processed: i64,
    pub elapsed: Duration,
    pub rate: f64,
    pub eta: Option<Duration>,
    pub workers: Vec<WorkerStatus>,
    pub errors: Vec<String>,
    pub status: String,
    pub finished: bool,
    pub aborted: bool,
    pub active_limit: i64,
    pub forbidden: Vec<ForbiddenStat>,
}

impl Stats {
    pub fn new(total: usize, workers: usize) -> Arc<Self> {
        Arc::new(Self {
            total: total as i64,
            deleted: AtomicI64::new(0),
            skipped: AtomicI64::new(0),
            failed: AtomicI64::new(0),
            start: Instant::now(),
            finished: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            active_limit: AtomicI64::new(workers as i64),
            inner: Mutex::new(StatsInner {
                workers: vec![WorkerStatus::default(); workers],
                ..Default::default()
            }),
        })
    }

    fn add_deleted(&self) {
        self.deleted.fetch_add(1, Ordering::Relaxed);
    }
    fn add_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }
    fn add_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    fn set_worker(&self, index: usize, status: WorkerStatus) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = guard.workers.get_mut(index) {
            *slot = status;
        }
    }

    fn set_status(&self, line: impl Into<String>) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status = line.into();
    }

    fn log_error(&self, line: impl Into<String>) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.errors.push(line.into());
        let len = guard.errors.len();
        if len > 200 {
            guard.errors.drain(0..len - 200);
        }
    }

    fn log_forbidden(&self, channel_id: &str, label: &str, reason: &str) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.forbidden.entry(channel_id.to_string()).or_default();
        entry.count += 1;
        if entry.reason.is_empty() {
            entry.reason = reason.to_string();
        }
        if entry.label.is_empty() {
            entry.label = label.to_string();
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let deleted = self.deleted.load(Ordering::Relaxed);
        let skipped = self.skipped.load(Ordering::Relaxed);
        let failed = self.failed.load(Ordering::Relaxed);
        let processed = deleted + skipped + failed;
        let total = self.total;
        let elapsed = self.start.elapsed();
        let rate = if elapsed.as_secs_f64() > 0.0 {
            deleted as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        let remaining = (total - processed).max(0);
        let eta = if rate > 0.01 && remaining > 0 {
            Some(Duration::from_secs_f64(remaining as f64 / rate))
        } else {
            None
        };
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Snapshot {
            total,
            deleted,
            skipped,
            failed,
            processed,
            elapsed,
            rate,
            eta,
            workers: guard.workers.clone(),
            errors: guard.errors.clone(),
            status: guard.status.clone(),
            finished: self.finished.load(Ordering::Relaxed),
            aborted: self.aborted.load(Ordering::Relaxed),
            active_limit: self.active_limit.load(Ordering::Relaxed),
            forbidden: guard.forbidden.values().cloned().collect(),
        }
    }
}

struct Control {
    paused: AtomicBool,
    stopped: AtomicBool,
    /// Account-wide pause-until, in millis since the engine's epoch. A 429
    /// parks every worker here.
    pause_until_ms: AtomicU64,
    epoch: Instant,
    active_limit: AtomicI64,
    in_flight: AtomicI64,
    collapse_score: AtomicI64,
    /// Rolling list of invalid-response timestamps (ms since epoch) for the
    /// Cloudflare budget.
    cloudflare_times: Mutex<Vec<u64>>,
    blocked_channels: Mutex<std::collections::HashSet<String>>,
}

const COLLAPSE_THRESHOLD: i64 = 4;
const COLLAPSE_PENALTY: i64 = 2;
const CF_WINDOW: Duration = Duration::from_secs(600);
const CF_PAUSE_AT: usize = 9500;
const CF_RESUME_AT: usize = 9000;

const ADAPTIVE_BACKOFF: f64 = 1.6;
const ADAPTIVE_CEIL_MULT: f64 = 8.0;
const ADAPTIVE_CEIL_MAX: Duration = Duration::from_secs(30);
const ADAPTIVE_RECOVER: f64 = 0.2;

impl Control {
    fn new(workers: usize) -> Self {
        Self {
            paused: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            pause_until_ms: AtomicU64::new(0),
            epoch: Instant::now(),
            active_limit: AtomicI64::new(workers as i64),
            in_flight: AtomicI64::new(0),
            collapse_score: AtomicI64::new(0),
            cloudflare_times: Mutex::new(Vec::new()),
            blocked_channels: Mutex::new(std::collections::HashSet::new()),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn pause_global(&self, duration: Duration) {
        if duration.is_zero() {
            return;
        }
        // Keep the max: a shorter pause never shrinks a longer one.
        self.pause_until_ms.fetch_max(
            self.now_ms() + duration.as_millis() as u64,
            Ordering::Relaxed,
        );
    }

    fn cloudflare_record(&self) {
        let now = self.now_ms();
        let mut guard = self
            .cloudflare_times
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.push(now);
        let cutoff = now.saturating_sub(CF_WINDOW.as_millis() as u64);
        guard.retain(|time| *time >= cutoff);
    }

    fn cloudflare_wait(&self) -> Option<Duration> {
        let now = self.now_ms();
        let mut guard = self
            .cloudflare_times
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cutoff = now.saturating_sub(CF_WINDOW.as_millis() as u64);
        guard.retain(|time| *time >= cutoff);
        let count = guard.len();
        if count < CF_PAUSE_AT {
            return None;
        }
        let drop = (count - CF_RESUME_AT + 1).clamp(1, count);
        let wake = guard[drop - 1] + CF_WINDOW.as_millis() as u64;
        Some(Duration::from_millis(wake.saturating_sub(now).max(1000)))
    }
}

struct RequestPermit(Arc<Control>);

impl Drop for RequestPermit {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct Controls {
    control: Arc<Control>,
}

impl Controls {
    pub fn pause(&self) {
        self.control.paused.store(true, Ordering::Relaxed);
    }
    pub fn resume(&self) {
        self.control.paused.store(false, Ordering::Relaxed);
    }
    pub fn toggle_pause(&self) {
        self.control.paused.fetch_xor(true, Ordering::Relaxed);
    }
    pub fn stop(&self) {
        self.control.stopped.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct Engine {
    config: EngineConfig,
    client: DiscordClient,
    stats: Arc<Stats>,
    control: Arc<Control>,
}

impl Engine {
    pub fn new(config: EngineConfig, client: DiscordClient, stats: Arc<Stats>) -> (Self, Controls) {
        let control = Arc::new(Control::new(config.workers));
        let controls = Controls {
            control: control.clone(),
        };
        (
            Self {
                config,
                client,
                stats,
                control,
            },
            controls,
        )
    }

    /// Delete every item in every job. `on_done` is called with the resume key
    /// of each item that needs no further request (deleted, already gone, or a
    /// system-message 403). Returns when all jobs finish or stop is requested.
    pub async fn run<F>(&self, jobs: Vec<ChannelJob>, on_done: F)
    where
        F: Fn(&str) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        let on_done = Arc::new(on_done);
        let queue = Arc::new(Mutex::new(jobs.into_iter()));

        let mut handles = Vec::new();
        for worker_id in 0..self.config.workers {
            handles.push(tokio::spawn({
                let queue = queue.clone();
                let this = self.clone();
                let on_done = on_done.clone();
                async move {
                    loop {
                        let Some(job) = queue
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .next()
                        else {
                            break;
                        };
                        if this.control.stopped.load(Ordering::Relaxed) {
                            break;
                        }
                        this.run_channel(worker_id, job, on_done.clone()).await;
                        this.stats.set_worker(worker_id, WorkerStatus::default());
                    }
                }
            }));
        }
        for handle in handles {
            if let Err(error) = handle.await {
                self.stats.log_error(format!("A deletion worker stopped: {error}. Run the app again to retry unprocessed messages."));
                self.abort();
            }
        }
        if self.control.stopped.load(Ordering::Relaxed) {
            self.stats.aborted.store(true, Ordering::Relaxed);
        }
        self.stats.finished.store(true, Ordering::Relaxed);
    }

    async fn run_channel<F>(&self, worker_id: usize, job: ChannelJob, on_done: Arc<F>)
    where
        F: Fn(&str) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        let total = job.items.len();
        let mut current_delay = self.config.delay;
        for (index, &message_id) in job.items.iter().enumerate() {
            if self.control.stopped.load(Ordering::Relaxed) {
                return;
            }
            self.stats.set_worker(
                worker_id,
                WorkerStatus {
                    active: true,
                    channel: job.label.clone(),
                    done: index,
                    total,
                },
            );
            let throttled = self.delete_one(&job, message_id, &on_done).await;
            current_delay = adapt_spacing(current_delay, self.config.delay, throttled);
            if self
                .control
                .blocked_channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&job.channel_id)
            {
                // No more requests can be made for this channel in this run.
                // Account for the remaining items without sleeping between them.
                let remaining = total - index - 1;
                self.stats
                    .skipped
                    .fetch_add(remaining as i64, Ordering::Relaxed);
                return;
            }

            if index + 1 < total {
                let wait = self.jitter(current_delay);
                if !self.sleep(wait).await {
                    return;
                }
            }
        }
    }

    async fn delete_one<F>(&self, job: &ChannelJob, message_id: u64, on_done: &Arc<F>) -> bool
    where
        F: Fn(&str) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        let (channel_id, label) = (job.channel_id.as_str(), job.label.as_str());
        let message_id = message_id.to_string();

        const MAX_ERR_RETRIES: u32 = 8;
        const MAX_RATE_RETRIES: u32 = 12;
        let mut backoff = Duration::from_secs(1);
        let mut error_retries = 0u32;
        let mut rate_retries = 0u32;
        let mut throttled = false;

        loop {
            if self.control.stopped.load(Ordering::Relaxed) {
                return throttled;
            }
            if let Some(duration) = self.control.cloudflare_wait() {
                self.stats.set_status(format!(
                    "Cloudflare safety: holding {:.0}s, near the invalid-response cap",
                    duration.as_secs_f64()
                ));
                if !self.sleep(duration).await {
                    return throttled;
                }
            }
            if self
                .control
                .blocked_channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(channel_id)
            {
                self.stats.add_skipped();
                return throttled;
            }
            // Admission counts requests, not worker numbers. Every worker can
            // finish its channel after the request limit becomes one.
            let permit = loop {
                let active = self.control.in_flight.load(Ordering::SeqCst);
                let limit = self.control.active_limit.load(Ordering::SeqCst);
                if active < limit
                    && self
                        .control
                        .in_flight
                        .compare_exchange(active, active + 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    break RequestPermit(self.control.clone());
                }
                if !self.sleep(Duration::from_millis(20)).await {
                    return throttled;
                }
            };
            if !self.gate().await {
                return throttled;
            }
            let outcome = tokio::select! {
                result = self.client.delete_message(channel_id, &message_id, job.verified) => result,
                _ = async {
                    while !self.control.stopped.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                } => return throttled,
            };
            drop(permit);
            match outcome {
                DeleteOutcome::Deleted | DeleteOutcome::AlreadyGone => {
                    self.stats.add_deleted();
                    self.save_progress(on_done.as_ref(), &message_id);
                    return throttled;
                }
                DeleteOutcome::RateLimited { retry_after, scope } => {
                    throttled = true;
                    if scope != Scope::Shared {
                        self.control.cloudflare_record();
                    }
                    // The client already holds this bucket; only a global
                    // limit holds the other channels too.
                    if scope == Scope::Global {
                        self.control.pause_global(retry_after);
                    }
                    if scope.should_reduce_concurrency() {
                        self.collapse_if_needed();
                        self.stats.set_status(format!(
                            "{} rate limit on {label}; easing off {:.1}s",
                            if scope == Scope::Global {
                                "global"
                            } else {
                                "shared"
                            },
                            retry_after.as_secs_f64()
                        ));
                    } else {
                        self.stats.set_status(format!(
                            "rate limited on {label}; easing off {:.1}s",
                            retry_after.as_secs_f64()
                        ));
                    }
                    rate_retries += 1;
                    if rate_retries >= MAX_RATE_RETRIES {
                        self.stats.set_status(format!(
                            "deferring {label} to a later run; still throttled after {rate_retries} tries"
                        ));
                        self.stats.add_skipped();
                        return throttled;
                    }
                    if !self.sleep(retry_after + Duration::from_millis(50)).await {
                        return throttled;
                    }
                }
                DeleteOutcome::SystemForbidden(reason) => {
                    self.control.cloudflare_record();
                    self.stats.log_forbidden(channel_id, label, &reason);
                    self.stats.add_skipped();
                    // Undeletable forever; never ask again.
                    self.save_progress(on_done.as_ref(), &message_id);
                    return throttled;
                }
                DeleteOutcome::LostAccess(reason) => {
                    self.control
                        .blocked_channels
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(channel_id.to_string());
                    self.control.cloudflare_record();
                    self.stats.log_forbidden(channel_id, label, &reason);
                    let message = format!(
                        "{label}: {reason}. Remaining messages in this channel are deferred; check access and retry."
                    );
                    self.stats.set_status(message.clone());
                    self.stats.log_error(message);
                    self.stats.add_skipped(); // retryable after rejoin; not logged
                    return throttled;
                }
                DeleteOutcome::Unauthorized => {
                    self.control.cloudflare_record();
                    self.stats
                        .log_error(format!("401 on {}; token invalid or expired?", message_id));
                    self.stats.add_failed();
                    self.stats.set_status(
                        "ABORTED: 401 Unauthorized. The token looks invalid or expired.",
                    );
                    self.abort();
                    return throttled;
                }
                DeleteOutcome::Protected(reason) => {
                    self.stats.set_status(reason);
                    self.stats.add_skipped();
                    return throttled;
                }
                DeleteOutcome::Fatal(reason) => {
                    self.stats.set_status(reason.clone());
                    self.stats.log_error(reason);
                    self.stats.add_failed();
                    self.abort();
                    return throttled;
                }
                DeleteOutcome::Permanent(reason) => {
                    self.stats.log_error(reason);
                    self.stats.add_failed();
                    return throttled;
                }
                DeleteOutcome::Transient(reason) => {
                    error_retries += 1;
                    if error_retries >= MAX_ERR_RETRIES {
                        self.stats.log_error(format!(
                            "gave up on {} after {error_retries} errors: {reason}",
                            message_id
                        ));
                        self.stats.add_failed();
                        return throttled;
                    }
                    self.stats.set_status(format!(
                        "{reason} on {label}; retry in {:.0}s",
                        backoff.as_secs_f64()
                    ));
                    if !self.sleep(backoff).await {
                        return throttled;
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    async fn gate(&self) -> bool {
        loop {
            let wait = if self.control.paused.load(Ordering::Relaxed) {
                150
            } else {
                let until = self.control.pause_until_ms.load(Ordering::Relaxed);
                until.saturating_sub(self.control.now_ms())
            };
            if wait == 0 {
                return !self.control.stopped.load(Ordering::Relaxed);
            }
            if !self.sleep(Duration::from_millis(wait)).await {
                return false;
            }
        }
    }

    fn collapse_if_needed(&self) {
        if self.control.active_limit.load(Ordering::Relaxed) <= 1 {
            return;
        }
        let score = self
            .control
            .collapse_score
            .fetch_add(COLLAPSE_PENALTY, Ordering::Relaxed)
            + COLLAPSE_PENALTY;
        if score >= COLLAPSE_THRESHOLD {
            self.control.active_limit.store(1, Ordering::Relaxed);
            self.stats.active_limit.store(1, Ordering::Relaxed);
            self.stats
                .set_status("shared or global rate limit: reduced to 1 worker");
        }
    }

    /// Record a finished message. A failed write stops the run, because a later
    /// run would otherwise repeat the delete.
    fn save_progress(&self, on_done: &impl Fn(&str) -> anyhow::Result<()>, message_id: &str) {
        if let Err(error) = on_done(message_id) {
            self.stats.log_error(format!(
                "Cannot save progress: {error}. Check state directory permissions before you retry."
            ));
            self.abort();
        }
    }

    fn abort(&self) {
        self.stats.aborted.store(true, Ordering::Relaxed);
        self.control.stopped.store(true, Ordering::Relaxed);
    }

    fn jitter(&self, duration: Duration) -> Duration {
        if self.config.jitter <= 0.0 {
            return duration;
        }
        let factor = (rand::thread_rng().r#gen::<f64>() * 2.0 - 1.0) * self.config.jitter;
        let ms = (duration.as_secs_f64() * (1.0 + factor)).max(0.0);
        Duration::from_secs_f64(ms)
    }

    async fn sleep(&self, duration: Duration) -> bool {
        if duration.is_zero() {
            return !self.control.stopped.load(Ordering::Relaxed);
        }
        let step = Duration::from_millis(150);
        let mut left = duration;
        while left > Duration::ZERO {
            if self.control.stopped.load(Ordering::Relaxed) {
                return false;
            }
            let pause = left.min(step);
            tokio::time::sleep(pause).await;
            left = left.saturating_sub(pause);
        }
        !self.control.stopped.load(Ordering::Relaxed)
    }
}

/// AIMD: widen multiplicatively when throttled (up to a ceiling), narrow
/// additively back toward the floor on a clean delete. A floor of 0 disables
/// adaptation (pacing off).
pub fn adapt_spacing(current_delay: Duration, floor: Duration, throttled: bool) -> Duration {
    if floor.is_zero() {
        return Duration::ZERO;
    }
    if throttled {
        let next = current_delay.mul_f64(ADAPTIVE_BACKOFF);
        let ceiling = floor
            .mul_f64(ADAPTIVE_CEIL_MULT)
            .min(ADAPTIVE_CEIL_MAX)
            .max(floor);
        return next.min(ceiling);
    }
    let step = floor.mul_f64(ADAPTIVE_RECOVER);
    current_delay.saturating_sub(step).max(floor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aimd_widens_then_recovers() {
        let floor = Duration::from_millis(1000);
        let a = adapt_spacing(floor, floor, true);
        assert!(a > floor, "throttle should widen spacing");
        let b = adapt_spacing(a, floor, false);
        assert!(
            b < a && b >= floor,
            "clean delete should recover toward floor"
        );
    }

    #[test]
    fn aimd_respects_ceiling() {
        let floor = Duration::from_millis(1000);
        let mut current_delay = floor;
        for _ in 0..50 {
            current_delay = adapt_spacing(current_delay, floor, true);
        }
        assert!(current_delay <= ADAPTIVE_CEIL_MAX);
        assert!(current_delay <= floor.mul_f64(ADAPTIVE_CEIL_MULT) + Duration::from_millis(1));
    }

    #[test]
    fn zero_floor_disables() {
        assert_eq!(
            adapt_spacing(Duration::ZERO, Duration::ZERO, true),
            Duration::ZERO
        );
    }
}
