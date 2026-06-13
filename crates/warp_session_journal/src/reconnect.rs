//! Reconnect timing policy + crash-loop detection.
//!
//! Pure logic: takes wall-clock timestamps as input, produces
//! [`ReconnectDecision`]s. No async runtime, no transport, no
//! threads — so the timing rules can be unit-tested directly and
//! a future R3.4b loop can wrap them in whatever tokio / threading
//! shape fits the call site.
//!
//! Two pieces, intentionally separate:
//!
//! 1. [`ReconnectPolicy`] + [`BackoffState`] — exponential backoff
//!    schedule. Knobs (initial / max / multiplier) live in
//!    [`ReconnectConfig`]; per-session bookkeeping (next-attempt
//!    timestamp, current backoff, attempt count) lives in
//!    [`BackoffState`]. A failed attempt doubles the window; a
//!    successful one resets it. Callers ask the policy "is now
//!    a good time to try?" via [`ReconnectPolicy::decide`].
//!
//! 2. [`CrashLoopDetector`] + [`CrashLoopConfig`] — sliding-window
//!    counter that flags "this isn't a transient blip, it's a crash
//!    loop". The reconnect loop checks this BEFORE scheduling more
//!    attempts; once it trips, the policy yields
//!    [`ReconnectDecision::GiveUp`] until the detector clears.
//!
//! Pairs with [`crate::ManagedSession`] (which owns the lifecycle
//! state machine) — the typical wiring is:
//!
//! ```ignore
//! match session.lifecycle() {
//!     SessionLifecycle::Disconnected { .. } => match policy.decide(&backoff, now_ms) {
//!         ReconnectDecision::Attempt => try_connect(),
//!         ReconnectDecision::Wait { .. } => continue,
//!         ReconnectDecision::GiveUp { reason } => alert_user(reason),
//!     },
//!     _ => (),
//! }
//! ```

use std::collections::VecDeque;

/// Default first backoff window — 5 seconds.
pub const DEFAULT_INITIAL_BACKOFF_MS: u64 = 5_000;

/// Default ceiling on the backoff window — 5 minutes.
pub const DEFAULT_MAX_BACKOFF_MS: u64 = 5 * 60 * 1_000;

/// Default multiplier between successive backoff windows. 2.0 →
/// classic exponential. Stored as u32 for `checked_mul` semantics.
pub const DEFAULT_BACKOFF_MULTIPLIER: u32 = 2;

/// Knobs for [`ReconnectPolicy`]. Built once at startup; tests
/// override individual fields via [`ReconnectConfig::instant_test`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectConfig {
    /// First backoff window (ms) — applied between the disconnect
    /// signal and the first reconnect attempt.
    pub initial_backoff_ms: u64,
    /// Upper bound on the backoff window (ms). Prevents the cadence
    /// from drifting into "hours between attempts" on stubborn
    /// outages.
    pub max_backoff_ms: u64,
    /// Multiplier applied to the current window after each failure.
    /// 2 → classic exponential; 1 → linear retries; 3+ → aggressive
    /// backoff. Capped at `max_backoff_ms` regardless.
    pub backoff_multiplier: u32,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_backoff_ms: DEFAULT_INITIAL_BACKOFF_MS,
            max_backoff_ms: DEFAULT_MAX_BACKOFF_MS,
            backoff_multiplier: DEFAULT_BACKOFF_MULTIPLIER,
        }
    }
}

impl ReconnectConfig {
    /// Test-only config that fires attempts immediately and caps
    /// the backoff at 1 second so the full transition matrix runs
    /// without burning real clock time.
    pub fn instant_test() -> Self {
        Self {
            initial_backoff_ms: 0,
            max_backoff_ms: 1_000,
            backoff_multiplier: 2,
        }
    }
}

/// Per-session reconnect bookkeeping. Created when a session
/// transitions to `Disconnected`, dropped when it returns to
/// `Active`/`Reattached` or expires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackoffState {
    /// Wall-clock timestamp (ms) after which the next reconnect
    /// attempt is permitted.
    next_attempt_at_ms: i64,
    /// Current backoff window in ms. Doubles after each failure,
    /// capped at `config.max_backoff_ms`.
    current_backoff_ms: u64,
    /// Total attempts the loop has made for this session. Used by
    /// the UI to render "attempt N" or trigger a one-shot toast on
    /// the first failure.
    attempts: u32,
}

impl BackoffState {
    pub fn next_attempt_at_ms(&self) -> i64 {
        self.next_attempt_at_ms
    }

    pub fn current_backoff_ms(&self) -> u64 {
        self.current_backoff_ms
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

/// What a [`ReconnectPolicy::decide`] call produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconnectDecision {
    /// Now is the time to fire a reconnect attempt. The caller
    /// should attempt the connection and then call
    /// [`ReconnectPolicy::after_failure`] or
    /// [`ReconnectPolicy::after_success`] to advance the state.
    Attempt,
    /// Backoff window hasn't elapsed yet. `ms_remaining` is how
    /// long to sleep before the next `decide` call.
    Wait { ms_remaining: i64 },
    /// Crash-loop detector tripped, or some other terminal
    /// condition — stop retrying and surface to the user.
    GiveUp { reason: String },
}

/// Stateless decision-maker. Each instance is cheap to construct
/// and holds only the config; per-session state lives in
/// [`BackoffState`].
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    config: ReconnectConfig,
}

impl ReconnectPolicy {
    pub fn new(config: ReconnectConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &ReconnectConfig {
        &self.config
    }

    /// Build fresh state for a session that just entered
    /// `Disconnected`. The first attempt is scheduled
    /// `initial_backoff_ms` into the future so the network has a
    /// moment to settle before the loop slams it with a retry.
    pub fn initial_state(&self, now_ms: i64) -> BackoffState {
        BackoffState {
            next_attempt_at_ms: now_ms.saturating_add(self.config.initial_backoff_ms as i64),
            current_backoff_ms: self.config.initial_backoff_ms,
            attempts: 0,
        }
    }

    /// Should the caller attempt right now? Pure function — does
    /// not mutate state. Callers fold the result via
    /// [`Self::after_failure`] / [`Self::after_success`] after
    /// actually attempting.
    pub fn decide(&self, state: &BackoffState, now_ms: i64) -> ReconnectDecision {
        let ms_remaining = state.next_attempt_at_ms.saturating_sub(now_ms);
        if ms_remaining <= 0 {
            ReconnectDecision::Attempt
        } else {
            ReconnectDecision::Wait { ms_remaining }
        }
    }

    /// Record a failed reconnect attempt: bump `attempts`, double
    /// the backoff window (capped at `max_backoff_ms`), and
    /// schedule the next allowed attempt.
    pub fn after_failure(&self, state: &mut BackoffState, now_ms: i64) {
        state.attempts = state.attempts.saturating_add(1);
        // Double (or apply the configured multiplier) and cap.
        let next_backoff = if state.current_backoff_ms == 0 {
            // Tests using instant_test() start at 0; bootstrap to
            // a sensible minimum so the next attempt isn't a
            // tight-loop. 1ms is enough to be observable + still
            // testable.
            1
        } else {
            state
                .current_backoff_ms
                .checked_mul(self.config.backoff_multiplier as u64)
                .unwrap_or(self.config.max_backoff_ms)
                .min(self.config.max_backoff_ms)
        };
        state.current_backoff_ms = next_backoff;
        state.next_attempt_at_ms = now_ms.saturating_add(next_backoff as i64);
    }

    /// Record a successful reconnect: reset bookkeeping to the
    /// initial state so a future drop+reconnect cycle starts fresh.
    pub fn after_success(&self, state: &mut BackoffState, now_ms: i64) {
        *state = self.initial_state(now_ms);
    }
}

// ── crash-loop detector ─────────────────────────────────────────────

/// Default crash-loop threshold — 3 failures inside the window.
pub const DEFAULT_CRASH_LOOP_THRESHOLD: u32 = 3;

/// Default sliding window — 5 minutes.
pub const DEFAULT_CRASH_LOOP_WINDOW_MS: i64 = 5 * 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashLoopConfig {
    /// How many failures in the window trigger the alert.
    pub threshold: u32,
    /// Sliding window length in ms. Failures older than `now -
    /// window_ms` are evicted before the check.
    pub window_ms: i64,
}

impl Default for CrashLoopConfig {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_CRASH_LOOP_THRESHOLD,
            window_ms: DEFAULT_CRASH_LOOP_WINDOW_MS,
        }
    }
}

/// Tracks recent failure timestamps, flags crash-loops, and emits
/// the alert at most once per "loop episode" (a contiguous stretch
/// where the failure count stays at or above the threshold).
#[derive(Debug, Clone)]
pub struct CrashLoopDetector {
    config: CrashLoopConfig,
    /// Failure timestamps (ms) in chronological order. The oldest
    /// is evicted on each `record_failure` once the window slides
    /// past it.
    recent_failures: VecDeque<i64>,
    /// `true` once we've emitted an alert for the current loop
    /// episode. Set by [`Self::should_emit_alert`] returning
    /// `true`; cleared by [`Self::record_recovery`] when the
    /// failure count drops below the threshold.
    alerted: bool,
}

impl CrashLoopDetector {
    pub fn new(config: CrashLoopConfig) -> Self {
        Self {
            config,
            recent_failures: VecDeque::new(),
            alerted: false,
        }
    }

    pub fn config(&self) -> &CrashLoopConfig {
        &self.config
    }

    /// Returns the number of failures inside the current window.
    /// Mostly for tests + diagnostics surfaces.
    pub fn failures_in_window(&self) -> usize {
        self.recent_failures.len()
    }

    /// Record a failure at `now_ms`. Evicts any failures older than
    /// `now_ms - window_ms` so the deque stays bounded.
    pub fn record_failure(&mut self, now_ms: i64) {
        self.evict_old(now_ms);
        self.recent_failures.push_back(now_ms);
    }

    /// Mark the session as recovered. Clears the alerted flag if
    /// the failure count is now below the threshold (so a future
    /// loop episode can re-fire). Does NOT clear the recent failure
    /// list — those still count if another failure lands within
    /// the window.
    pub fn record_recovery(&mut self, now_ms: i64) {
        self.evict_old(now_ms);
        if (self.recent_failures.len() as u32) < self.config.threshold {
            self.alerted = false;
        }
    }

    /// `true` iff the failure count inside the window meets or
    /// exceeds the threshold. Doesn't change `alerted` — callers
    /// who only want to throttle UI noise should use
    /// [`Self::should_emit_alert`] instead.
    pub fn is_in_crash_loop(&mut self, now_ms: i64) -> bool {
        self.evict_old(now_ms);
        (self.recent_failures.len() as u32) >= self.config.threshold
    }

    /// `true` iff we're in a crash loop AND we haven't already
    /// emitted the alert for this episode. Sets `alerted = true`
    /// on the first `true` return so subsequent calls inside the
    /// same episode return `false`.
    ///
    /// The intended use is the reconnect loop calling this once
    /// per tick: at most one alert per episode, with the next
    /// episode starting after a `record_recovery` that drops the
    /// count below the threshold.
    pub fn should_emit_alert(&mut self, now_ms: i64) -> bool {
        if self.is_in_crash_loop(now_ms) && !self.alerted {
            self.alerted = true;
            true
        } else {
            false
        }
    }

    fn evict_old(&mut self, now_ms: i64) {
        let cutoff = now_ms.saturating_sub(self.config.window_ms);
        while let Some(&oldest) = self.recent_failures.front() {
            if oldest < cutoff {
                self.recent_failures.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
#[path = "reconnect_tests.rs"]
mod tests;
