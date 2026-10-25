//! Bounded, volatile terminal followers. Callers serialize snapshot/register and
//! canonical commit/publication with their persistence mutex.

use std::cell::RefCell;

use serde_json::Value;
use worker::{Response, Result};

use crate::{observer_transport, util};

const MAX_FOLLOWERS: usize = 32;
const MAX_RUN_FOLLOWERS: usize = 8;

#[derive(Clone, Copy)]
pub(crate) enum TerminalKind {
    Chat,
    Responses,
    Compact,
}

impl TerminalKind {
    fn marker(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
            Self::Compact => "compact",
        }
    }
}

struct Follower {
    id: String,
    sender: observer_transport::Sender,
}

#[derive(Default)]
pub(crate) struct TerminalFollowers {
    followers: RefCell<Vec<Follower>>,
}

impl TerminalFollowers {
    pub(crate) fn subscribe(&self, id: &str, kind: TerminalKind) -> Result<Response> {
        // Reject before creating a native stream/timer for a full registry.
        if !self.has_room(id) {
            return limit();
        }
        let (sender, response) = observer_transport::channel(kind.marker())?;
        if !self.register(id, sender) {
            return limit();
        }
        Ok(response)
    }

    pub(crate) fn has(&self, id: &str) -> bool {
        let mut followers = self.followers.borrow_mut();
        prune(&mut followers);
        followers.iter().any(|follower| follower.id == id)
    }

    pub(crate) fn publish(&self, id: &str, status: u16, body: &Value) {
        let targets = {
            let mut followers = self.followers.borrow_mut();
            prune(&mut followers);
            let (targets, remaining) = std::mem::take(&mut *followers)
                .into_iter()
                .partition::<Vec<_>, _>(|follower| follower.id == id);
            *followers = remaining;
            targets
        };
        if targets.is_empty() {
            return;
        }
        // Serialize once; fanout copies only the wire bytes, not the source
        // journal or its potentially large reconstructed JSON value.
        let Some(frame) = observer_transport::encode_terminal(status, body) else {
            return;
        };
        for target in targets {
            observer_transport::publish_frame(target.sender, &frame);
        }
    }

    fn has_room(&self, id: &str) -> bool {
        let mut followers = self.followers.borrow_mut();
        prune(&mut followers);
        has_room(&followers, id)
    }

    fn register(&self, id: &str, sender: observer_transport::Sender) -> bool {
        let mut followers = self.followers.borrow_mut();
        prune(&mut followers);
        if !has_room(&followers, id) {
            return false;
        }
        followers.push(Follower {
            id: id.into(),
            sender,
        });
        true
    }
}

fn prune(followers: &mut Vec<Follower>) {
    followers.retain(|follower| !follower.sender.is_closed());
}

fn has_room(followers: &[Follower], id: &str) -> bool {
    followers.len() < MAX_FOLLOWERS
        && followers
            .iter()
            .filter(|follower| follower.id == id)
            .count()
            < MAX_RUN_FOLLOWERS
}

fn limit() -> Result<Response> {
    util::json_error(429, "observer_limit", "Too many active terminal observers")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{channel::mpsc, executor::block_on};
    use serde_json::json;

    fn register(
        followers: &TerminalFollowers,
        id: &str,
    ) -> Option<mpsc::UnboundedReceiver<Result<Vec<u8>>>> {
        let (sender, receiver) = mpsc::unbounded();
        followers.register(id, sender).then_some(receiver)
    }

    #[test]
    fn per_run_limit_and_closed_follower_pruning_are_independent() {
        let followers = TerminalFollowers::default();
        let mut readers = (0..MAX_RUN_FOLLOWERS)
            .map(|_| register(&followers, "first").unwrap())
            .collect::<Vec<_>>();
        assert!(register(&followers, "first").is_none());
        let other = register(&followers, "second").unwrap();
        assert!(followers.has("first") && followers.has("second"));
        drop(readers.pop());
        readers.push(register(&followers, "first").unwrap());
        assert!(register(&followers, "first").is_none());
        drop(readers);
        assert!(!followers.has("first"));
        assert!(followers.has("second"));
        drop(other);
        assert!(!followers.has("second"));
    }

    #[test]
    fn session_limit_bounds_many_distinct_run_ids() {
        let followers = TerminalFollowers::default();
        let readers = (0..MAX_FOLLOWERS)
            .map(|index| register(&followers, &format!("run-{index}")).unwrap())
            .collect::<Vec<_>>();
        assert!(!followers.has_room("new"));
        assert!(register(&followers, "new").is_none());
        drop(readers);
        assert!(followers.has_room("new"));
    }

    #[test]
    fn publication_drains_only_its_original_run_once() {
        let followers = TerminalFollowers::default();
        let first = register(&followers, "old").unwrap();
        let second = register(&followers, "old").unwrap();
        let next = register(&followers, "new").unwrap();
        let body = json!({"run_id":"old","text":"雪🙂","status":"completed"});
        followers.publish("old", 200, &body);
        assert!(!followers.has("old"));
        assert!(followers.has("new"));
        followers.publish("old", 502, &json!({"error":"not the original result"}));
        for reader in [first, second] {
            let result = block_on(observer_transport::decode(reader)).unwrap();
            assert_eq!(result.status, 200);
            assert_eq!(result.body, body);
        }
        followers.publish("new", 503, &json!({"error":{"code":"run_detached"}}));
        assert_eq!(
            block_on(observer_transport::decode(next)).unwrap().status,
            503
        );
        assert!(!followers.has("new"));
    }

    #[test]
    fn rejected_encoding_closes_matching_followers_without_touching_other_runs() {
        let followers = TerminalFollowers::default();
        let rejected = register(&followers, "old").unwrap();
        let next = register(&followers, "new").unwrap();
        followers.publish("old", 201, &json!({}));
        assert!(block_on(observer_transport::decode(rejected)).is_err());
        assert!(!followers.has("old") && followers.has("new"));
        drop(next);
        assert!(!followers.has("new"));
    }

    #[test]
    fn terminal_kinds_use_only_the_private_transport_markers() {
        assert_eq!(TerminalKind::Chat.marker(), "chat");
        assert_eq!(TerminalKind::Responses.marker(), "responses");
        assert_eq!(TerminalKind::Compact.marker(), "compact");
    }
}
