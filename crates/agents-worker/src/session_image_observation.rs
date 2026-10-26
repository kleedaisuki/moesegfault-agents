//! Volatile original-observer witnesses are not public response authority.
use super::*;
use crate::response_image_projection::ResponseFrame;
use futures::Stream;
use std::collections::BTreeMap;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use wasm_bindgen::JsValue;

enum Phase {
    Pending,
    Stream(mpsc::Sender<Result<ResponseFrame>>),
    Json(crate::observer_transport::Sender),
    Draining,
    Closed,
}

struct Observation {
    id: String,
    phase: RefCell<Phase>,
    receipt_needed: Cell<bool>,
}

pub(super) struct ImageObservations {
    generation: String,
    entries: Rc<RefCell<BTreeMap<String, Rc<Observation>>>>,
}

impl Default for ImageObservations {
    fn default() -> Self {
        Self {
            generation: uuid::Uuid::new_v4().to_string(),
            entries: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }
}

pub(super) struct OriginalImageGuard {
    observation: Rc<Observation>,
    armed: bool,
    registry: Rc<RefCell<BTreeMap<String, Rc<Observation>>>>,
    response: String,
}

impl Drop for OriginalImageGuard {
    fn drop(&mut self) {
        if self.armed && matches!(*self.observation.phase.borrow(), Phase::Pending) {
            *self.observation.phase.borrow_mut() = Phase::Closed;
        }
        if self.armed && matches!(*self.observation.phase.borrow(), Phase::Closed) {
            let _ = self.registry.borrow_mut().remove(&self.response);
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageObservationCallback {
    namespace: String,
    actor_name: String,
    response_id: String,
    observation_id: String,
    generation: String,
    #[serde(skip)]
    local: Option<Rc<Observation>>,
}

impl ImageObservationCallback {
    pub(crate) async fn close(&self, env: &Env) -> Result<()> {
        if let Some(observation) = &self.local
            && !observation.receipt_needed.get()
        {
            *observation.phase.borrow_mut() = Phase::Closed;
            return Ok(());
        }
        if self
            .local
            .as_ref()
            .is_some_and(|observation| matches!(*observation.phase.borrow(), Phase::Closed))
        {
            return Ok(());
        }
        let stub = env
            .durable_object(&self.namespace)?
            .id_from_name(&self.actor_name)?
            .get_stub()?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_body(Some(JsValue::from_str(&serde_json::to_string(self)?)));
        let request = Request::new_with_init(
            &format!(
                "https://session.internal/image-observations/{}/close",
                self.observation_id
            ),
            &init,
        )?;
        let response = stub.fetch_with_request(request).await?;
        if response.status_code() != 200 {
            return Err(Error::RustError("image_observation_close_failed".into()));
        }
        Ok(())
    }
}

pub(super) struct ImageObservationBody<S> {
    source: S,
    env: Env,
    state: JsValue,
    callback: Option<ImageObservationCallback>,
}

impl<S> ImageObservationBody<S> {
    pub(super) fn new(
        source: S,
        env: Env,
        state: JsValue,
        callback: Option<ImageObservationCallback>,
    ) -> Self {
        Self {
            source,
            env,
            state,
            callback,
        }
    }

    fn close(&mut self) {
        let Some(callback) = self.callback.take() else {
            return;
        };
        let env = self.env.clone();
        let promise = wasm_bindgen_futures::future_to_promise(async move {
            callback
                .close(&env)
                .await
                .map_err(|_| JsValue::from_str("image_observation_close_failed"))?;
            Ok(JsValue::UNDEFINED)
        });
        use wasm_bindgen::JsCast;
        if let Ok(method) = js_sys::Reflect::get(&self.state, &JsValue::from_str("waitUntil"))
            && let Ok(method) = method.dyn_into::<js_sys::Function>()
        {
            let _ = method.call1(&self.state, &promise);
        }
    }
}

impl<S: Stream<Item = Result<Vec<u8>>> + Unpin> Stream for ImageObservationBody<S> {
    type Item = Result<Vec<u8>>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = Pin::new(&mut self.source).poll_next(cx);
        if matches!(next, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            self.close();
        }
        next
    }
}

impl<S> Drop for ImageObservationBody<S> {
    fn drop(&mut self) {
        self.close();
    }
}

impl SessionInner {
    pub(super) fn begin_original_image_observation(
        &self,
        response: &str,
        expected: bool,
    ) -> OriginalImageGuard {
        let mut entries = self.image_observations.entries.borrow_mut();
        let fresh = !entries.contains_key(response);
        let observation = entries
            .entry(response.into())
            .or_insert_with(|| {
                Rc::new(Observation {
                    id: uuid::Uuid::new_v4().to_string(),
                    receipt_needed: Cell::new(false),
                    phase: RefCell::new(if expected {
                        Phase::Pending
                    } else {
                        Phase::Closed
                    }),
                })
            })
            .clone();
        OriginalImageGuard {
            observation,
            armed: fresh,
            registry: self.image_observations.entries.clone(),
            response: response.into(),
        }
    }

    pub(super) fn original_image_identity(&self, response: &str) -> Option<(String, String)> {
        self.image_observations
            .entries
            .borrow()
            .get(response)
            .map(|observation| {
                (
                    observation.id.clone(),
                    self.image_observations.generation.clone(),
                )
            })
    }

    pub(super) fn attach_original_image_stream(
        &self,
        response: &str,
        sender: &mpsc::Sender<Result<ResponseFrame>>,
    ) {
        if let Some(observation) = self.image_observations.entries.borrow().get(response)
            && matches!(*observation.phase.borrow(), Phase::Pending)
        {
            *observation.phase.borrow_mut() = Phase::Stream(sender.clone());
        }
    }

    pub(super) fn attach_original_image_json(
        &self,
        response: &str,
        sender: &crate::observer_transport::Sender,
    ) {
        if let Some(observation) = self.image_observations.entries.borrow().get(response)
            && matches!(*observation.phase.borrow(), Phase::Pending)
        {
            *observation.phase.borrow_mut() = Phase::Json(sender.clone());
        }
    }

    pub(super) fn original_image_observation_closed(
        &self,
        response: &str,
        generation: &str,
    ) -> bool {
        if generation != self.image_observations.generation {
            return true;
        }
        let entries = self.image_observations.entries.borrow();
        let Some(observation) = entries.get(response) else {
            return true;
        };
        let closed = match &*observation.phase.borrow() {
            Phase::Closed => true,
            Phase::Stream(sender) => sender.is_closed(),
            Phase::Json(sender) => sender.is_closed(),
            Phase::Pending | Phase::Draining => false,
        };
        closed
    }

    pub(super) fn mark_original_image_closed(&self, response: &str) {
        if let Some(observation) = self.image_observations.entries.borrow().get(response) {
            *observation.phase.borrow_mut() = Phase::Closed;
        }
    }

    pub(super) fn arm_original_image_observation(&self, response: &str) {
        if let Some(observation) = self.image_observations.entries.borrow().get(response) {
            observation.receipt_needed.set(true);
        }
    }

    pub(super) fn forget_original_image_observation(&self, response: &str) {
        let closed = self
            .image_observations
            .entries
            .borrow()
            .get(response)
            .is_some_and(|observation| matches!(*observation.phase.borrow(), Phase::Closed));
        if closed {
            let _ = self
                .image_observations
                .entries
                .borrow_mut()
                .remove(response);
        }
    }

    pub(super) fn original_image_callback(
        &self,
        response: &str,
    ) -> Option<ImageObservationCallback> {
        let entries = self.image_observations.entries.borrow();
        let observation = entries.get(response)?;
        let data = self.data.borrow();
        let record = &data.as_ref()?.record;
        Some(ImageObservationCallback {
            namespace: crate::runtime_profiles::binding(record.runtime_profile).into(),
            actor_name: format!("{}:{}", record.owner_id, record.id),
            response_id: response.into(),
            observation_id: observation.id.clone(),
            generation: self.image_observations.generation.clone(),
            local: Some(observation.clone()),
        })
    }

    pub(super) fn drain_original_image_observation(
        &self,
        response: &str,
    ) -> Option<ImageObservationCallback> {
        let entries = self.image_observations.entries.borrow();
        let observation = entries.get(response)?;
        if matches!(*observation.phase.borrow(), Phase::Closed) {
            return None;
        }
        *observation.phase.borrow_mut() = Phase::Draining;
        let data = self.data.borrow();
        let record = &data.as_ref()?.record;
        Some(ImageObservationCallback {
            namespace: crate::runtime_profiles::binding(record.runtime_profile).into(),
            actor_name: format!("{}:{}", record.owner_id, record.id),
            response_id: response.into(),
            observation_id: observation.id.clone(),
            generation: self.image_observations.generation.clone(),
            local: Some(observation.clone()),
        })
    }

    pub(super) async fn image_observation_close_route(
        &self,
        mut request: Request,
    ) -> Result<Response> {
        let callback: ImageObservationCallback = request.json().await?;
        let data = self.data.borrow();
        let record = &data
            .as_ref()
            .ok_or_else(|| Error::RustError("session_not_initialized".into()))?
            .record;
        if callback.actor_name != format!("{}:{}", record.owner_id, record.id)
            || callback.namespace != crate::runtime_profiles::binding(record.runtime_profile)
            || request.path() != format!("/image-observations/{}/close", callback.observation_id)
        {
            return Response::empty().map(|response| response.with_status(404));
        }
        drop(data);
        self.close_original_image_delivery(
            &callback.response_id,
            &callback.observation_id,
            &callback.generation,
        )
        .await?;
        Ok(Response::empty()?.with_status(200))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn creation_drop_closes_only_its_pending_observation() {
        let observation = Rc::new(Observation {
            id: "known".into(),
            receipt_needed: Cell::new(false),
            phase: RefCell::new(Phase::Pending),
        });
        drop(OriginalImageGuard {
            observation: observation.clone(),
            armed: true,
            registry: Rc::new(RefCell::new(BTreeMap::new())),
            response: "resp".into(),
        });
        assert!(matches!(*observation.phase.borrow(), Phase::Closed));
        *observation.phase.borrow_mut() = Phase::Draining;
        drop(OriginalImageGuard {
            observation: observation.clone(),
            armed: true,
            registry: Rc::new(RefCell::new(BTreeMap::new())),
            response: "resp".into(),
        });
        assert!(matches!(*observation.phase.borrow(), Phase::Draining));
    }

    #[test]
    fn draining_drops_controller_sender_without_claiming_receiver_finished() {
        use futures::{FutureExt, StreamExt};
        let (sender, mut receiver) = mpsc::channel::<Result<ResponseFrame>>(1);
        let observation = Observation {
            id: "known".into(),
            receipt_needed: Cell::new(false),
            phase: RefCell::new(Phase::Stream(sender.clone())),
        };
        drop(sender);
        assert!(receiver.next().now_or_never().is_none());
        *observation.phase.borrow_mut() = Phase::Draining;
        assert!(matches!(*observation.phase.borrow(), Phase::Draining));
        assert!(matches!(receiver.next().now_or_never(), Some(None)));
    }
}
