use std::{cell::RefCell, future::Future, rc::Rc, time::Duration};

use serde::de::DeserializeOwned;
use wasm_bindgen::{JsCast, closure::Closure};
use worker::{AbortController, Delay, Error, Response, ResponseBody, ResponseBuilder, Result};

pub(super) struct Deadline {
    expires_ms: u64,
    controller: Rc<RefCell<Option<AbortController>>>,
    timer: i32,
    _callback: Closure<dyn FnMut()>,
}

impl Deadline {
    pub fn new(expires_ms: u64) -> Result<Self> {
        let controller = Rc::new(RefCell::new(Some(AbortController::default())));
        let timer_controller = controller.clone();
        let callback = Closure::wrap(
            Box::new(move || abort_controller(&timer_controller)) as Box<dyn FnMut()>
        );
        let remaining = expires_ms
            .saturating_sub(worker::Date::now().as_millis())
            .min(i32::MAX as u64);
        let global: worker::web_sys::WorkerGlobalScope = js_sys::global().unchecked_into();
        let timer = global.set_timeout_with_callback_and_timeout_and_arguments_0(
            callback.as_ref().unchecked_ref(),
            remaining as i32,
        )?;
        Ok(Self {
            expires_ms,
            controller,
            timer,
            _callback: callback,
        })
    }

    pub fn signal(&self) -> worker::AbortSignal {
        self.controller
            .borrow()
            .as_ref()
            .expect("deadline not yet aborted")
            .signal()
    }

    fn abort(&self) {
        abort_controller(&self.controller);
    }

    pub async fn wait<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        let remaining = self
            .expires_ms
            .saturating_sub(worker::Date::now().as_millis());
        if remaining == 0 {
            self.abort();
            return Err(timeout());
        }
        cancel_on_timeout(
            future,
            Delay::from(Duration::from_millis(remaining)),
            || self.abort(),
        )
        .await
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        let global: worker::web_sys::WorkerGlobalScope = js_sys::global().unchecked_into();
        global.clear_timeout_with_handle(self.timer);
        self.abort();
    }
}

fn abort_controller(controller: &RefCell<Option<AbortController>>) {
    let controller = controller.borrow_mut().take();
    if let Some(controller) = controller {
        controller.abort();
    }
}

pub(super) struct TimedResponse {
    pub response: Response,
    pub deadline: Deadline,
}

impl TimedResponse {
    pub fn status_code(&self) -> u16 {
        self.response.status_code()
    }
    pub fn headers(&self) -> &worker::Headers {
        self.response.headers()
    }
    pub async fn json<T: DeserializeOwned>(&mut self) -> Result<T> {
        self.deadline.wait(self.response.json()).await
    }
    pub fn into_parts(self) -> (ResponseBuilder, ResponseBody, Deadline) {
        let (builder, body) = self.response.into_parts();
        (builder, body, self.deadline)
    }
}

async fn cancel_on_timeout<T>(
    operation: impl Future<Output = Result<T>>,
    timer: impl Future<Output = ()>,
    cancel: impl FnOnce(),
) -> Result<T> {
    match futures::future::select(Box::pin(operation), Box::pin(timer)).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => {
            cancel();
            Err(timeout())
        }
    }
}

fn timeout() -> Error {
    Error::RustError("workspace_transport_timeout".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_stalled_transport_is_cancelled_before_timeout_is_returned() {
        let cancelled = Cell::new(false);
        let result = futures::executor::block_on(cancel_on_timeout(
            futures::future::pending::<Result<()>>(),
            futures::future::ready(()),
            || cancelled.set(true),
        ));
        assert!(result.is_err());
        assert!(cancelled.get());
    }

    #[test]
    fn a_completed_operation_is_not_reported_as_a_timeout() {
        let cancelled = Cell::new(false);
        let result = futures::executor::block_on(cancel_on_timeout(
            futures::future::ready(Ok(42)),
            futures::future::pending(),
            || cancelled.set(true),
        ));
        assert_eq!(result.unwrap(), 42);
        assert!(!cancelled.get());
    }
}
