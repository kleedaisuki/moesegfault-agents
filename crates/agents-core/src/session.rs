use serde::{Deserialize, Serialize};

use crate::{PinnedContext, SESSION_IDLE_TIMEOUT_MS};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Idle,
    Running,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub generation: u64,
    pub manifest_key: String,
    pub sha256: String,
    pub format_version: u32,
    pub created_at_ms: u64,
    pub logical_bytes: u64,
    pub archive_bytes: u64,
    pub entries: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub owner_id: String,
    pub agent_id: String,
    pub status: SessionStatus,
    pub created_at_ms: u64,
    pub last_input_at_ms: u64,
    pub input_revision: u64,
    pub pinned: PinnedContext,
    pub workspace_snapshot: Option<WorkspaceSnapshot>,
}

pub type SessionSnapshot = SessionRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleAction {
    Archive,
    CheckpointRunning,
    WaitUntil(u64),
    AlreadyClosed,
}

impl SessionRecord {
    pub fn idle_deadline_ms(&self) -> u64 {
        self.last_input_at_ms
            .saturating_add(SESSION_IDLE_TIMEOUT_MS)
    }

    pub fn idle_action(&self, now_ms: u64) -> IdleAction {
        match self.status {
            SessionStatus::Closed => IdleAction::AlreadyClosed,
            SessionStatus::Running if now_ms >= self.idle_deadline_ms() => {
                IdleAction::CheckpointRunning
            }
            SessionStatus::Running | SessionStatus::Idle if now_ms < self.idle_deadline_ms() => {
                IdleAction::WaitUntil(self.idle_deadline_ms())
            }
            SessionStatus::Idle => IdleAction::Archive,
            SessionStatus::Running => IdleAction::CheckpointRunning,
        }
    }

    /// Only accepted user appends, including steering, advance the idle clock.
    pub fn append_user_input(&mut self, now_ms: u64) -> Result<u64, &'static str> {
        if self.status == SessionStatus::Closed {
            return Err("session is closed");
        }
        let revision = self
            .input_revision
            .checked_add(1)
            .ok_or("input revision exhausted")?;
        self.last_input_at_ms = self.last_input_at_ms.max(now_ms);
        self.input_revision = revision;
        Ok(revision)
    }

    pub fn start_run(&mut self) -> Result<(), &'static str> {
        if self.status != SessionStatus::Idle {
            return Err("session is not idle");
        }
        self.status = SessionStatus::Running;
        Ok(())
    }

    pub fn complete_run(&mut self, now_ms: u64) -> Result<IdleAction, &'static str> {
        if self.status != SessionStatus::Running {
            return Err("session has no active run");
        }
        self.status = SessionStatus::Idle;
        Ok(self.idle_action(now_ms))
    }

    /// Compare the revision again after external snapshot I/O before committing closure.
    pub fn close_if_idle(&mut self, now_ms: u64, expected_input_revision: u64) -> bool {
        if self.input_revision != expected_input_revision
            || self.idle_action(now_ms) != IdleAction::Archive
        {
            return false;
        }
        self.status = SessionStatus::Closed;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateAgent, PinnedContext};
    use proptest::prelude::*;

    fn session(now: u64) -> SessionRecord {
        let agent = serde_json::from_value::<CreateAgent>(serde_json::json!({"name":"test"}))
            .unwrap()
            .into_config("a".into(), "u".into(), now);
        SessionRecord {
            id: "s".into(),
            owner_id: "u".into(),
            agent_id: "a".into(),
            status: SessionStatus::Idle,
            created_at_ms: now,
            last_input_at_ms: now,
            input_revision: 0,
            pinned: PinnedContext::resolve(&agent, vec![], vec![]),
            workspace_snapshot: None,
        }
    }

    proptest! {
        #[test]
        fn long_running_work_is_never_archived(start in 0u64..=u64::MAX-SESSION_IDLE_TIMEOUT_MS, elapsed in SESSION_IDLE_TIMEOUT_MS..=u64::MAX) {
            let mut s = session(start);
            s.start_run().unwrap();
            prop_assert_ne!(s.idle_action(start.saturating_add(elapsed)), IdleAction::Archive);
            prop_assert_eq!(s.last_input_at_ms, start);
        }

        #[test]
        fn new_steering_invalidates_a_pending_close(start in 0u64..=u64::MAX-SESSION_IDLE_TIMEOUT_MS-1) {
            let mut s = session(start);
            let revision = s.input_revision;
            let now = start + SESSION_IDLE_TIMEOUT_MS;
            s.append_user_input(now).unwrap();
            prop_assert!(!s.close_if_idle(now + 1, revision));
            prop_assert_eq!(s.status, SessionStatus::Idle);
        }

        #[test]
        fn appends_never_move_idle_clock_backwards(first in any::<u64>(), second in any::<u64>()) {
            let mut s = session(first);
            s.append_user_input(second).unwrap();
            prop_assert_eq!(s.last_input_at_ms, first.max(second));
        }
    }

    #[test]
    fn completion_after_idle_deadline_archives_without_extending_lifetime() {
        let mut s = session(10);
        s.start_run().unwrap();
        assert_eq!(
            s.complete_run(SESSION_IDLE_TIMEOUT_MS + 11).unwrap(),
            IdleAction::Archive
        );
        assert_eq!(s.last_input_at_ms, 10);
        assert!(s.close_if_idle(SESSION_IDLE_TIMEOUT_MS + 11, 0));
        assert!(s.append_user_input(SESSION_IDLE_TIMEOUT_MS + 12).is_err());
    }
}
