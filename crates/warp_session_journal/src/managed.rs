//! Session lifecycle state machine on top of [`EventJournal`].
//!
//! The journal alone records and replays events. `ManagedSession`
//! adds the *when* — when is the session live, when is it
//! disconnected (but reattach is still possible), when has the
//! reattach window expired, and when is the session dead.
//!
//! Pure logic, no transport coupling. R3.2 of the SSH connection
//! management roadmap. Future phases wire the state transitions into
//! the actual remote-server protocol (Attach RPC) and an
//! auto-reconnect loop, but the lifecycle decisions live here so
//! they can be unit-tested without a real connection.

use serde::Serialize;

use crate::{EventJournal, JournalDiskWriter, ReplaySnapshot};

/// Default reattach window (5 minutes). A session that's been
/// disconnected longer than this is considered dead and its
/// journal is torn down.
pub const DEFAULT_REATTACH_WINDOW_MS: i64 = 5 * 60 * 1000;

/// Lifecycle state of a managed session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLifecycle {
    /// Session is live; the journal accepts appends.
    Active,
    /// Transport dropped at `since_ms`. The journal still holds
    /// recent entries for an eventual reattach. New appends are
    /// rejected.
    Disconnected { since_ms: i64 },
    /// A reattach occurred after a Disconnected period. The journal
    /// resumes accepting appends and the desktop has caught up via
    /// `handle_attach`.
    Reattached,
    /// The reattach window elapsed without a successful reattach;
    /// the session is being torn down. The journal entries remain
    /// readable until the owner drops the [`ManagedSession`] so a
    /// caller can dump them to durable storage if desired.
    Dead { reason: String },
}

impl SessionLifecycle {
    /// `true` when the session is in a terminal state.
    pub fn is_dead(&self) -> bool {
        matches!(self, Self::Dead { .. })
    }

    /// `true` when the session accepts new appends.
    pub fn is_appendable(&self) -> bool {
        matches!(self, Self::Active | Self::Reattached)
    }
}

/// Errors returned from [`ManagedSession`] state-changing methods.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    #[error("session is not accepting appends (current state: {state:?})")]
    NotAppendable { state: SessionLifecycle },
    #[error("session is dead: {reason}")]
    Dead { reason: String },
}

/// A journal paired with lifecycle state. Built on top of
/// [`EventJournal`] so the in-memory ring + optional disk mirror
/// stay reusable for non-managed callers.
pub struct ManagedSession<P> {
    session_id: String,
    journal: EventJournal<P>,
    lifecycle: SessionLifecycle,
    reattach_window_ms: i64,
}

impl<P> std::fmt::Debug for ManagedSession<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedSession")
            .field("session_id", &self.session_id)
            .field("lifecycle", &self.lifecycle)
            .field("journal", &self.journal)
            .field("reattach_window_ms", &self.reattach_window_ms)
            .finish()
    }
}

impl<P> ManagedSession<P> {
    /// Build a session in [`SessionLifecycle::Active`] with the
    /// default reattach window and default journal capacity.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            journal: EventJournal::default(),
            lifecycle: SessionLifecycle::Active,
            reattach_window_ms: DEFAULT_REATTACH_WINDOW_MS,
        }
    }

    /// Build a session with an explicit journal capacity.
    pub fn with_capacity(session_id: impl Into<String>, capacity: usize) -> Self {
        Self {
            session_id: session_id.into(),
            journal: EventJournal::with_capacity(capacity),
            lifecycle: SessionLifecycle::Active,
            reattach_window_ms: DEFAULT_REATTACH_WINDOW_MS,
        }
    }

    /// Override the reattach window (ms). Useful for tests; production
    /// should stick with [`DEFAULT_REATTACH_WINDOW_MS`].
    pub fn with_reattach_window_ms(mut self, window_ms: i64) -> Self {
        self.reattach_window_ms = window_ms;
        self
    }

    /// Attach a disk-backed JSONL mirror to the underlying journal.
    pub fn with_disk_writer(mut self, writer: JournalDiskWriter<P>) -> Self
    where
        P: Serialize,
    {
        self.journal = self.journal.with_disk_writer(writer);
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn lifecycle(&self) -> &SessionLifecycle {
        &self.lifecycle
    }

    pub fn head_seq(&self) -> u64 {
        self.journal.head_seq()
    }

    pub fn is_dead(&self) -> bool {
        self.lifecycle.is_dead()
    }

    /// Mark the session as disconnected at `now_ms`. Idempotent —
    /// subsequent calls don't reset the disconnect timestamp.
    /// Calling on a `Dead` session is a no-op.
    pub fn mark_disconnected(&mut self, now_ms: i64) {
        match &self.lifecycle {
            SessionLifecycle::Active | SessionLifecycle::Reattached => {
                self.lifecycle = SessionLifecycle::Disconnected { since_ms: now_ms };
            }
            // Already disconnected, or dead — no-op.
            SessionLifecycle::Disconnected { .. } | SessionLifecycle::Dead { .. } => {}
        }
    }

    /// Advance the lifecycle clock. If the session is currently
    /// `Disconnected` and `now_ms - since_ms >= reattach_window_ms`,
    /// transition to `Dead`. Otherwise no-op.
    ///
    /// Callers should `tick` periodically (e.g. every 30s) to expire
    /// orphaned sessions.
    pub fn tick(&mut self, now_ms: i64) {
        if let SessionLifecycle::Disconnected { since_ms } = self.lifecycle {
            if now_ms.saturating_sub(since_ms) >= self.reattach_window_ms {
                self.lifecycle = SessionLifecycle::Dead {
                    reason: format!("reattach window of {}ms elapsed", self.reattach_window_ms),
                };
            }
        }
    }
}

impl<P: Clone + Serialize> ManagedSession<P> {
    /// Append an event to the journal. Returns the assigned seq.
    /// Fails when the session is not in [`SessionLifecycle::Active`]
    /// or [`SessionLifecycle::Reattached`].
    pub fn append(&mut self, payload: P) -> Result<u64, SessionError> {
        match &self.lifecycle {
            SessionLifecycle::Active | SessionLifecycle::Reattached => {
                Ok(self.journal.append(payload))
            }
            SessionLifecycle::Disconnected { .. } => Err(SessionError::NotAppendable {
                state: self.lifecycle.clone(),
            }),
            SessionLifecycle::Dead { reason } => Err(SessionError::Dead {
                reason: reason.clone(),
            }),
        }
    }
}

impl<P: Clone> ManagedSession<P> {
    /// Reattach a client at `since_seq`. Transitions
    /// [`SessionLifecycle::Disconnected`] → [`SessionLifecycle::Reattached`]
    /// and returns the replay snapshot. Calling on an already-active
    /// session is allowed (e.g. a client that lost track of its own
    /// connection state) and returns the same snapshot; calling on a
    /// dead session returns an error.
    pub fn handle_attach(
        &mut self,
        since_seq: Option<u64>,
    ) -> Result<ReplaySnapshot<P>, SessionError> {
        if let SessionLifecycle::Dead { reason } = &self.lifecycle {
            return Err(SessionError::Dead {
                reason: reason.clone(),
            });
        }
        let snapshot = self.journal.replay_since(since_seq);
        // Disconnected → Reattached; Active stays Active.
        if matches!(self.lifecycle, SessionLifecycle::Disconnected { .. }) {
            self.lifecycle = SessionLifecycle::Reattached;
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
#[path = "managed_tests.rs"]
mod tests;
