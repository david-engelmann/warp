use serde_json::{json, Value};

use super::{ManagedSession, SessionError, SessionLifecycle};

fn payload(label: &str) -> Value {
    json!({ "type": "test", "label": label })
}

// ── lifecycle defaults ──────────────────────────────────────────────

#[test]
fn new_session_starts_active() {
    let s = ManagedSession::<Value>::new("sess-1");
    assert_eq!(s.session_id(), "sess-1");
    assert_eq!(s.lifecycle(), &SessionLifecycle::Active);
    assert!(!s.is_dead());
}

#[test]
fn lifecycle_predicate_helpers() {
    assert!(SessionLifecycle::Active.is_appendable());
    assert!(SessionLifecycle::Reattached.is_appendable());
    assert!(!SessionLifecycle::Disconnected { since_ms: 0 }.is_appendable());
    assert!(!SessionLifecycle::Dead { reason: "x".into() }.is_appendable());

    assert!(!SessionLifecycle::Active.is_dead());
    assert!(!SessionLifecycle::Disconnected { since_ms: 0 }.is_dead());
    assert!(SessionLifecycle::Dead { reason: "x".into() }.is_dead());
}

// ── append gating ───────────────────────────────────────────────────

#[test]
fn append_works_in_active_state() {
    let mut s = ManagedSession::new("sess");
    let seq = s.append(payload("a")).expect("append");
    assert_eq!(seq, 1);
    assert_eq!(s.head_seq(), 1);
}

#[test]
fn append_rejected_when_disconnected() {
    let mut s = ManagedSession::new("sess");
    s.append(payload("a")).unwrap();
    s.mark_disconnected(1_000);

    let err = s.append(payload("b")).expect_err("append should fail");
    assert!(matches!(err, SessionError::NotAppendable { .. }));
    // head_seq unchanged
    assert_eq!(s.head_seq(), 1);
}

#[test]
fn append_rejected_when_dead() {
    let mut s = ManagedSession::<Value>::new("sess").with_reattach_window_ms(100);
    s.mark_disconnected(0);
    s.tick(200); // beyond window

    let err = s.append(payload("a")).expect_err("append should fail");
    assert!(matches!(err, SessionError::Dead { .. }));
}

#[test]
fn append_resumes_after_reattach() {
    let mut s = ManagedSession::new("sess");
    s.append(payload("a")).unwrap();
    s.mark_disconnected(1_000);
    let _snapshot = s.handle_attach(Some(0)).expect("attach succeeds");
    assert_eq!(s.lifecycle(), &SessionLifecycle::Reattached);

    let seq = s.append(payload("b")).expect("append after reattach");
    assert_eq!(seq, 2);
}

// ── mark_disconnected ───────────────────────────────────────────────

#[test]
fn mark_disconnected_transitions_from_active() {
    let mut s = ManagedSession::<Value>::new("sess");
    s.mark_disconnected(1_000);
    assert_eq!(
        s.lifecycle(),
        &SessionLifecycle::Disconnected { since_ms: 1_000 }
    );
}

#[test]
fn mark_disconnected_idempotent_does_not_reset_timestamp() {
    let mut s = ManagedSession::<Value>::new("sess");
    s.mark_disconnected(1_000);
    s.mark_disconnected(5_000);
    // The original timestamp survives — a second disconnect signal
    // must not extend the reattach window.
    assert_eq!(
        s.lifecycle(),
        &SessionLifecycle::Disconnected { since_ms: 1_000 }
    );
}

#[test]
fn mark_disconnected_transitions_from_reattached() {
    let mut s = ManagedSession::<Value>::new("sess");
    s.mark_disconnected(1_000);
    s.handle_attach(None).unwrap();
    assert_eq!(s.lifecycle(), &SessionLifecycle::Reattached);

    s.mark_disconnected(2_000);
    assert_eq!(
        s.lifecycle(),
        &SessionLifecycle::Disconnected { since_ms: 2_000 }
    );
}

#[test]
fn mark_disconnected_noop_on_dead_session() {
    let mut s = ManagedSession::<Value>::new("sess").with_reattach_window_ms(100);
    s.mark_disconnected(0);
    s.tick(200);
    assert!(s.is_dead());

    s.mark_disconnected(500);
    // Still Dead — the dead reason isn't overwritten.
    assert!(s.is_dead());
}

// ── tick / reattach window ─────────────────────────────────────────

#[test]
fn tick_within_window_keeps_session_disconnected() {
    let mut s = ManagedSession::<Value>::new("sess").with_reattach_window_ms(1_000);
    s.mark_disconnected(0);
    s.tick(500);
    assert!(matches!(
        s.lifecycle(),
        SessionLifecycle::Disconnected { .. }
    ));
}

#[test]
fn tick_at_or_past_window_transitions_to_dead() {
    let mut s = ManagedSession::<Value>::new("sess").with_reattach_window_ms(1_000);
    s.mark_disconnected(0);
    s.tick(1_000);
    assert!(s.is_dead());
}

#[test]
fn tick_well_past_window_still_transitions_to_dead_with_correct_reason() {
    let mut s = ManagedSession::<Value>::new("sess").with_reattach_window_ms(100);
    s.mark_disconnected(0);
    s.tick(10_000);
    match s.lifecycle() {
        SessionLifecycle::Dead { reason } => assert!(reason.contains("100ms")),
        other => panic!("expected Dead, got {other:?}"),
    }
}

#[test]
fn tick_on_active_session_is_noop() {
    let mut s = ManagedSession::<Value>::new("sess");
    s.tick(1_000_000);
    assert_eq!(s.lifecycle(), &SessionLifecycle::Active);
}

#[test]
fn tick_on_reattached_session_is_noop() {
    let mut s = ManagedSession::<Value>::new("sess");
    s.mark_disconnected(0);
    s.handle_attach(None).unwrap();
    s.tick(1_000_000);
    assert_eq!(s.lifecycle(), &SessionLifecycle::Reattached);
}

// ── handle_attach ───────────────────────────────────────────────────

#[test]
fn handle_attach_from_disconnected_transitions_to_reattached() {
    let mut s = ManagedSession::new("sess");
    s.append(payload("a")).unwrap();
    s.append(payload("b")).unwrap();
    s.mark_disconnected(1_000);

    let snapshot = s.handle_attach(Some(1)).expect("attach");
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.entries[0].seq, 2);
    assert_eq!(snapshot.head_seq, 2);
    assert_eq!(s.lifecycle(), &SessionLifecycle::Reattached);
}

#[test]
fn handle_attach_on_active_session_returns_snapshot_without_state_change() {
    let mut s = ManagedSession::new("sess");
    s.append(payload("a")).unwrap();
    let snapshot = s.handle_attach(None).expect("attach");
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(s.lifecycle(), &SessionLifecycle::Active);
}

#[test]
fn handle_attach_on_dead_session_errors() {
    let mut s = ManagedSession::<Value>::new("sess").with_reattach_window_ms(100);
    s.mark_disconnected(0);
    s.tick(200);

    let err = s.handle_attach(Some(0)).expect_err("dead session attach");
    assert!(matches!(err, SessionError::Dead { .. }));
}

#[test]
fn handle_attach_reports_replay_gap_when_cursor_below_evicted_floor() {
    let mut s = ManagedSession::<Value>::with_capacity("sess", 2);
    s.append(payload("a")).unwrap(); // seq 1, evicted
    s.append(payload("b")).unwrap(); // seq 2
    s.append(payload("c")).unwrap(); // seq 3 — evicts seq 1
    s.mark_disconnected(1_000);

    let snapshot = s.handle_attach(Some(0)).expect("attach");
    assert_eq!(snapshot.replay_gap, Some(2));
}
