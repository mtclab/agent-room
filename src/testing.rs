//! Test-only traffic generator, inert unless `AGENT_ROOM_TEST_SPAM` is set.
//!
//! Live gate G3 needs one connector to hammer another with mentions faster than
//! the budgets allow. Rather than a second fake client with its own bugs, the
//! connector itself can be told to do it: set
//! `AGENT_ROOM_TEST_SPAM=@someone:server` and it posts a burst of mentions into
//! a fresh thread at startup. Without the variable nothing here runs.

use std::time::Duration;

use matrix_sdk::Client;
use matrix_sdk::ruma::{OwnedEventId, RoomId};
use serde_json::json;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

pub const SPAM_ENV: &str = "AGENT_ROOM_TEST_SPAM";
pub const SPAM_COUNT_ENV: &str = "AGENT_ROOM_TEST_SPAM_COUNT";
pub const SPAM_INTERVAL_ENV: &str = "AGENT_ROOM_TEST_SPAM_INTERVAL";

pub const DEFAULT_COUNT: u32 = 12;
pub const DEFAULT_INTERVAL_S: f64 = 1.0;

/// The user id to spam, or None when the connector must behave normally.
#[must_use]
pub fn spam_target() -> Option<String> {
    std::env::var(SPAM_ENV)
        .ok()
        .filter(|value| !value.is_empty())
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Post `count` mentions of `target` into one fresh thread.
async fn run_spam(
    client: Client,
    room_id: &RoomId,
    target: String,
    count: u32,
    interval: Duration,
) {
    let Some(room) = client.get_room(room_id) else {
        error!("{room_id}: cannot spam a room the client does not know");
        return;
    };
    let mut thread: Option<OwnedEventId> = None;
    let mut previous: Option<OwnedEventId> = None;
    for index in 1..=count {
        let mut content = json!({
            "msgtype": "m.notice",
            "body": format!("{target}: spam {index}/{count}"),
            "m.mentions": { "user_ids": [target] },
        });
        if let (Some(thread), Some(previous)) = (thread.as_ref(), previous.as_ref()) {
            content["m.relates_to"] = json!({
                "rel_type": "m.thread",
                "event_id": thread,
                "is_falling_back": true,
                "m.in_reply_to": { "event_id": previous },
            });
        }
        match room.send_raw("m.room.message", content).await {
            Ok(sent) => {
                let event_id = sent.response.event_id;
                info!("test spam {index}/{count} posted as {event_id}");
                thread.get_or_insert_with(|| event_id.clone());
                previous = Some(event_id);
            }
            Err(exc) => error!("test spam {index}/{count} failed: {exc}"),
        }
        tokio::time::sleep(interval).await;
    }
}

/// Start the spam task when the env var is set; otherwise do nothing.
pub fn maybe_start_spam(client: &Client, room_ids: &[String]) -> Option<JoinHandle<()>> {
    let target = spam_target()?;
    let Some(room_id) = room_ids.first() else {
        error!("{SPAM_ENV} is set but no rooms are configured");
        return None;
    };
    let Ok(room_id) = RoomId::parse(room_id) else {
        error!("{SPAM_ENV} is set but {room_id} is not a room id");
        return None;
    };
    let count = env_or(SPAM_COUNT_ENV, DEFAULT_COUNT);
    let interval = Duration::from_secs_f64(env_or(SPAM_INTERVAL_ENV, DEFAULT_INTERVAL_S));
    warn!(
        "{SPAM_ENV} is set: posting {count} mentions of {target} into {room_id} at {:.1} s \
         intervals",
        interval.as_secs_f64()
    );
    let client = client.clone();
    Some(tokio::spawn(async move {
        run_spam(client, &room_id, target, count, interval).await;
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_runs_without_the_environment_variable() {
        // The whole safety property of this module: an operator who has never
        // heard of it cannot make their agent spam a room by accident.
        assert!(
            std::env::var(SPAM_ENV).is_err(),
            "the test suite must not be run with {SPAM_ENV} set"
        );
        assert!(spam_target().is_none());
    }

    #[test]
    fn the_defaults_are_the_burst_g3_is_written_around() {
        assert_eq!(env_or(SPAM_COUNT_ENV, DEFAULT_COUNT), 12);
        assert!((env_or(SPAM_INTERVAL_ENV, DEFAULT_INTERVAL_S) - 1.0).abs() < f64::EPSILON);
    }
}
