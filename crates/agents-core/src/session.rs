use serde::{Deserialize, Serialize};

use crate::{
    PinnedContext, PromptLayout, RegisteredTool, SESSION_IDLE_TIMEOUT_MS, SessionConfiguration,
};

/// The deployed Container application is selected once, before Session admission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeProfile {
    #[default]
    #[serde(rename = "basic1g")]
    Basic1g,
    #[serde(rename = "standard4g")]
    Standard4g,
}

impl RuntimeProfile {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Basic1g => "basic1g",
            Self::Standard4g => "standard4g",
        }
    }

    pub const fn memory_bytes(self) -> u64 {
        match self {
            Self::Basic1g => 1 << 30,
            Self::Standard4g => 4 << 30,
        }
    }

    pub const fn disk_bytes(self) -> u64 {
        match self {
            Self::Basic1g => 4_000_000_000,
            Self::Standard4g => 8_000_000_000,
        }
    }
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<SessionConfiguration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_layout: Option<PromptLayout>,
    pub workspace_snapshot: Option<WorkspaceSnapshot>,
    #[serde(default)]
    pub runtime_profile: RuntimeProfile,
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
    pub fn tools(&self) -> &[RegisteredTool] {
        self.configuration
            .as_ref()
            .map_or(&self.pinned.tools, |configuration| &configuration.tools)
    }

    pub fn configuration_revision(&self) -> u64 {
        self.configuration
            .as_ref()
            .map_or(self.pinned.agent_revision, |configuration| {
                configuration.agent_revision
            })
    }

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
            configuration: None,
            context_layout: None,
            workspace_snapshot: None,
            runtime_profile: RuntimeProfile::Basic1g,
        }
    }

    #[test]
    fn session_configuration_overlay_preserves_legacy_pin_and_user_clock() {
        let mut state = session(42);
        let legacy = serde_json::to_value(&state).unwrap();
        assert!(legacy.get("configuration").is_none());
        let hash = state.pinned.prefix_hash();
        state.configuration = Some(SessionConfiguration {
            agent_revision: 9,
            instructions: "new".into(),
            tools: vec![],
            unavailable_tools: vec![],
        });
        assert_eq!(state.configuration_revision(), 9);
        assert_eq!(state.last_input_at_ms, 42);
        assert_eq!(state.input_revision, 0);
        assert_eq!(state.pinned.prefix_hash(), hash);
        assert!(!state.pinned.workspace);
        let restored: SessionRecord = serde_json::from_value(legacy).unwrap();
        assert!(restored.configuration.is_none());
        assert_eq!(restored.pinned.prefix_hash(), hash);
    }

    proptest! {
        #[test]
        fn session_configuration_overlay_round_trips_without_mutating_source_identity(text in ".{0,4096}",revision in 2u64..1_000_000) {
            let mut state = session(42);let original = state.pinned.clone();
            state.configuration = Some(SessionConfiguration{agent_revision:revision,instructions:text.clone(),tools:vec![],unavailable_tools:vec![]});
            let restored:SessionRecord = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
            prop_assert_eq!(restored.configuration.unwrap().instructions,text);
            prop_assert_eq!(restored.pinned,original);prop_assert_eq!(restored.last_input_at_ms,42);
            prop_assert_eq!(restored.input_revision,0);
        }

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
