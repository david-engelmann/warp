use super::{
    CrashLoopConfig, CrashLoopDetector, ReconnectConfig, ReconnectDecision, ReconnectPolicy,
};

// ── ReconnectConfig ─────────────────────────────────────────────────

#[test]
fn default_config_uses_5s_initial_and_5min_cap() {
    let c = ReconnectConfig::default();
    assert_eq!(c.initial_backoff_ms, 5_000);
    assert_eq!(c.max_backoff_ms, 5 * 60 * 1_000);
    assert_eq!(c.backoff_multiplier, 2);
}

#[test]
fn instant_test_config_uses_zero_initial() {
    let c = ReconnectConfig::instant_test();
    assert_eq!(c.initial_backoff_ms, 0);
}

// ── initial_state ──────────────────────────────────────────────────

#[test]
fn initial_state_schedules_first_attempt_after_initial_backoff() {
    let policy = ReconnectPolicy::new(ReconnectConfig::default());
    let state = policy.initial_state(1_000);
    assert_eq!(state.next_attempt_at_ms(), 6_000); // 1000 + 5000
    assert_eq!(state.current_backoff_ms(), 5_000);
    assert_eq!(state.attempts(), 0);
}

#[test]
fn initial_state_with_zero_backoff_schedules_attempt_now() {
    let policy = ReconnectPolicy::new(ReconnectConfig::instant_test());
    let state = policy.initial_state(1_000);
    assert_eq!(state.next_attempt_at_ms(), 1_000);
}

// ── decide ─────────────────────────────────────────────────────────

#[test]
fn decide_returns_wait_when_window_open() {
    let policy = ReconnectPolicy::new(ReconnectConfig::default());
    let state = policy.initial_state(1_000);
    match policy.decide(&state, 3_000) {
        ReconnectDecision::Wait { ms_remaining } => assert_eq!(ms_remaining, 3_000),
        other => panic!("expected Wait, got {other:?}"),
    }
}

#[test]
fn decide_returns_attempt_at_exact_window_close() {
    let policy = ReconnectPolicy::new(ReconnectConfig::default());
    let state = policy.initial_state(1_000);
    assert_eq!(policy.decide(&state, 6_000), ReconnectDecision::Attempt,);
}

#[test]
fn decide_returns_attempt_after_window_close() {
    let policy = ReconnectPolicy::new(ReconnectConfig::default());
    let state = policy.initial_state(1_000);
    assert_eq!(policy.decide(&state, 10_000), ReconnectDecision::Attempt,);
}

// ── after_failure ──────────────────────────────────────────────────

#[test]
fn after_failure_increments_attempts_and_doubles_backoff() {
    let policy = ReconnectPolicy::new(ReconnectConfig::default());
    let mut state = policy.initial_state(1_000);
    assert_eq!(state.attempts(), 0);

    policy.after_failure(&mut state, 6_000);
    assert_eq!(state.attempts(), 1);
    assert_eq!(state.current_backoff_ms(), 10_000); // 5_000 * 2
    assert_eq!(state.next_attempt_at_ms(), 16_000); // 6_000 + 10_000

    policy.after_failure(&mut state, 16_000);
    assert_eq!(state.attempts(), 2);
    assert_eq!(state.current_backoff_ms(), 20_000); // 10_000 * 2
}

#[test]
fn after_failure_caps_backoff_at_max() {
    let policy = ReconnectPolicy::new(ReconnectConfig {
        initial_backoff_ms: 100,
        max_backoff_ms: 1_000,
        backoff_multiplier: 2,
    });
    let mut state = policy.initial_state(0);
    // 100 → 200 → 400 → 800 → 1000 (capped) → 1000 (still capped)
    for _ in 0..10 {
        policy.after_failure(&mut state, 0);
    }
    assert_eq!(state.current_backoff_ms(), 1_000);
}

#[test]
fn after_failure_with_zero_starting_backoff_bootstraps_to_one_ms() {
    let policy = ReconnectPolicy::new(ReconnectConfig::instant_test());
    let mut state = policy.initial_state(0);
    assert_eq!(state.current_backoff_ms(), 0);
    policy.after_failure(&mut state, 0);
    // Bootstraps to 1ms so subsequent retries aren't a tight loop.
    assert_eq!(state.current_backoff_ms(), 1);
}

#[test]
fn after_failure_handles_overflow_multiplier_by_clamping_to_max() {
    let policy = ReconnectPolicy::new(ReconnectConfig {
        initial_backoff_ms: u64::MAX / 2,
        max_backoff_ms: u64::MAX,
        backoff_multiplier: 4,
    });
    let mut state = policy.initial_state(0);
    // (u64::MAX / 2) * 4 overflows; the checked_mul fallback yields
    // `max_backoff_ms` which is u64::MAX. We just verify no panic.
    policy.after_failure(&mut state, 0);
    assert_eq!(state.current_backoff_ms(), u64::MAX);
}

// ── after_success ──────────────────────────────────────────────────

#[test]
fn after_success_resets_to_initial_state() {
    let policy = ReconnectPolicy::new(ReconnectConfig::default());
    let mut state = policy.initial_state(0);
    for _ in 0..5 {
        policy.after_failure(&mut state, 0);
    }
    assert!(state.attempts() > 0);
    assert!(state.current_backoff_ms() > 5_000);

    policy.after_success(&mut state, 100_000);
    assert_eq!(state.attempts(), 0);
    assert_eq!(state.current_backoff_ms(), 5_000);
    assert_eq!(state.next_attempt_at_ms(), 105_000);
}

// ── CrashLoopDetector ──────────────────────────────────────────────

fn detector_with(threshold: u32, window_ms: i64) -> CrashLoopDetector {
    CrashLoopDetector::new(CrashLoopConfig {
        threshold,
        window_ms,
    })
}

#[test]
fn default_detector_uses_three_failures_in_five_minutes() {
    let c = CrashLoopConfig::default();
    assert_eq!(c.threshold, 3);
    assert_eq!(c.window_ms, 5 * 60 * 1_000);
}

#[test]
fn fresh_detector_reports_no_crash_loop() {
    let mut d = detector_with(3, 60_000);
    assert!(!d.is_in_crash_loop(0));
    assert_eq!(d.failures_in_window(), 0);
}

#[test]
fn below_threshold_failures_do_not_trip_crash_loop() {
    let mut d = detector_with(3, 60_000);
    d.record_failure(1_000);
    d.record_failure(2_000);
    assert!(!d.is_in_crash_loop(3_000));
}

#[test]
fn threshold_failures_inside_window_trip_crash_loop() {
    let mut d = detector_with(3, 60_000);
    d.record_failure(1_000);
    d.record_failure(2_000);
    d.record_failure(3_000);
    assert!(d.is_in_crash_loop(3_000));
}

#[test]
fn failures_outside_window_are_evicted() {
    let mut d = detector_with(3, 60_000); // 1-minute window
    d.record_failure(0);
    d.record_failure(10_000);
    d.record_failure(20_000);
    assert!(d.is_in_crash_loop(20_000));

    // Slide the window forward — first failure (at 0) is now older
    // than `now - window_ms = 60_001 - 60_000 = 1`, so it evicts.
    // Two failures remain, below threshold.
    assert!(!d.is_in_crash_loop(60_001));
    assert_eq!(d.failures_in_window(), 2);
}

#[test]
fn should_emit_alert_fires_at_most_once_per_episode() {
    let mut d = detector_with(3, 60_000);
    d.record_failure(1_000);
    d.record_failure(2_000);
    d.record_failure(3_000);

    assert!(d.should_emit_alert(3_000));
    // Second call inside the same episode: no.
    assert!(!d.should_emit_alert(4_000));
    // Third, fourth, ... still no.
    assert!(!d.should_emit_alert(5_000));
}

#[test]
fn recovery_clears_alerted_flag_when_failures_drop_below_threshold() {
    let mut d = detector_with(3, 60_000);
    d.record_failure(1_000);
    d.record_failure(2_000);
    d.record_failure(3_000);
    assert!(d.should_emit_alert(3_000));

    // After enough time passes, two failures expire and recovery
    // clears the alert latch. The next loop episode CAN re-fire.
    d.record_recovery(70_000); // window evicted everything
    assert!(!d.is_in_crash_loop(70_000));

    // New crash loop later in the day — alert can fire again.
    d.record_failure(100_000);
    d.record_failure(101_000);
    d.record_failure(102_000);
    assert!(d.should_emit_alert(102_000));
}

#[test]
fn recovery_does_not_clear_alerted_if_still_at_threshold() {
    let mut d = detector_with(3, 60_000);
    d.record_failure(1_000);
    d.record_failure(2_000);
    d.record_failure(3_000);
    assert!(d.should_emit_alert(3_000));

    // Recovery signal but all three failures are still in the
    // window. The alerted flag should stay set — emitting again
    // would be UI noise.
    d.record_recovery(4_000);
    assert!(!d.should_emit_alert(4_000));
}

#[test]
fn detector_handles_high_failure_burst_without_unbounded_growth() {
    let mut d = detector_with(3, 1_000);
    // 1000 failures inside the window
    for ts in 0..1_000 {
        d.record_failure(ts);
    }
    assert!(d.is_in_crash_loop(999));
    // Slide far past the window — all evicted.
    assert!(!d.is_in_crash_loop(10_000));
    assert_eq!(d.failures_in_window(), 0);
}

#[test]
fn is_in_crash_loop_evicts_before_checking() {
    let mut d = detector_with(3, 1_000);
    d.record_failure(0);
    d.record_failure(100);
    d.record_failure(200);
    assert!(d.is_in_crash_loop(200));
    // Now well past the window — should NOT report a crash loop
    // even though we haven't called record_recovery.
    assert!(!d.is_in_crash_loop(10_000));
}
