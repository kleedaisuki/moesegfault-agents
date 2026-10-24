//! Wake owning sessions to reconcile attachment references from canonical state.
//! The cursor schedules work; neither age nor routing metadata permits release.

use serde::{Deserialize, Serialize};
use std::{future::Future, time::Duration};
use wasm_bindgen::JsValue;
use worker::{AbortController, Delay, Env, Method, Request, RequestInit, Result};

const CURSOR_KEY: &str = "file_attachment_pins_cursor_v1";
const BATCH_LIMIT: usize = 32;
const DISPATCH_TIMEOUT_MS: u64 = 5_000;

#[derive(Clone, Default, Deserialize, Serialize)]
struct Position {
    account_id: String,
    session_id: String,
}

#[derive(Deserialize)]
struct Cursor {
    value: String,
}

pub async fn run(env: &Env) -> Result<()> {
    let db = env.d1("DB")?;
    let position = db
        .prepare("SELECT value FROM resource_system_state WHERE key=?")
        .bind(&[text(CURSOR_KEY)])?
        .first::<Cursor>(None)
        .await?
        .map(|row| serde_json::from_str::<Position>(&row.value))
        .transpose()?
        .unwrap_or_default();
    let sessions = db.prepare("SELECT DISTINCT account_id,session_id FROM file_attachment_pins WHERE (account_id,session_id)>(?,?) ORDER BY account_id,session_id LIMIT CAST(? AS INTEGER)")
        .bind(&[text(&position.account_id),text(&position.session_id),text(&BATCH_LIMIT.to_string())])?
        .all().await?.results::<Position>()?;
    let mut completed = 0;
    for session in &sessions {
        if matches!(dispatch(env, session).await, Ok(true)) {
            completed += 1;
        }
    }
    // A failed session must not monopolize later owners. Retained references
    // remain visible for the next pass; an observation failure never unpins.
    let next = if sessions.len() == BATCH_LIMIT {
        sessions.last().cloned().unwrap_or_default()
    } else {
        Position::default()
    };
    db.prepare("INSERT INTO resource_system_state(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(&[text(CURSOR_KEY),text(&serde_json::to_string(&next)?)])?.run().await?;
    if !sessions.is_empty() {
        crate::util::emit(
            "space.attachment_pin_sweep",
            serde_json::json!({"sessions":sessions.len(),"completed":completed,"deferred":sessions.len()-completed}),
        );
    }
    Ok(())
}

async fn dispatch(env: &Env, session: &Position) -> Result<bool> {
    let namespace = env.durable_object("SESSIONS")?;
    let id = namespace.id_from_name(&format!("{}:{}", session.account_id, session.session_id))?;
    let controller = AbortController::default();
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    let native = web_sys::RequestInit::from(&init);
    native.set_signal(Some(&controller.signal()));
    let request = Request::from(web_sys::Request::new_with_str_and_init(
        "https://session.internal/file-pins/reconcile",
        &native,
    )?);
    let stub = id.get_stub()?;
    let response = observe_dispatch(
        stub.fetch_with_request(request),
        Delay::from(Duration::from_millis(DISPATCH_TIMEOUT_MS)),
        || controller.abort(),
    )
    .await?;
    Ok(response.is_some_and(|response| (200..300).contains(&response.status_code())))
}

async fn observe_dispatch<T>(
    operation: impl Future<Output = Result<T>>,
    timer: impl Future<Output = ()>,
    abort: impl FnOnce(),
) -> Result<Option<T>> {
    match futures::future::select(Box::pin(operation), Box::pin(timer)).await {
        futures::future::Either::Left((result, _)) => result.map(Some),
        futures::future::Either::Right((_, operation)) => {
            abort();
            drop(operation);
            Ok(None)
        }
    }
}

fn text(value: &str) -> JsValue {
    JsValue::from_str(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{executor::block_on, future};
    use std::cell::Cell;

    #[test]
    fn timed_out_session_is_aborted_before_its_observer_drops() {
        struct Observer<'a>(&'a Cell<bool>);
        impl Drop for Observer<'_> {
            fn drop(&mut self) {
                assert!(self.0.get());
            }
        }
        let aborted = Cell::new(false);
        let operation = async {
            let _observer = Observer(&aborted);
            future::pending::<Result<u8>>().await
        };
        let result = block_on(observe_dispatch(operation, future::ready(()), || {
            aborted.set(true)
        }));
        assert_eq!(result.unwrap(), None);
        assert!(aborted.get());
    }

    #[test]
    fn completed_reconciliation_does_not_abort_or_become_a_timeout() {
        let aborted = Cell::new(false);
        let result = block_on(observe_dispatch(
            future::ready(Ok(7)),
            future::pending(),
            || aborted.set(true),
        ));
        assert_eq!(result.unwrap(), Some(7));
        assert!(!aborted.get());
    }

    #[test]
    fn failed_observation_remains_an_error_not_successful_reconciliation() {
        let result = block_on(observe_dispatch(
            future::ready(Err::<u8, _>(worker::Error::RustError("unavailable".into()))),
            future::pending(),
            || {},
        ));
        assert!(result.is_err());
    }
}
