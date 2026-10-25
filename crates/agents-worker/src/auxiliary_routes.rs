//! Authenticated edge dispatch after the shared Responses context resolver.

use worker::{Env, Method, Request, RequestInit, Response, Result};

use crate::{
    model_operation::{PreparedCompact, operation_id},
    observer_transport, util,
};

pub(crate) async fn input_tokens(
    env: &Env,
    owner: &str,
    config: &crate::provider::ProviderConfig,
    prepared: &crate::provider::responses::PreparedRequest,
) -> Result<Response> {
    Response::from_json(
        &crate::responses_auxiliary::count_prepared(env, owner, config, prepared).await?,
    )
}

/// `prepared` comes from the same owner/pin/source resolution as generation.
/// Its request hash excludes transport tracing and the retry key itself.
pub(crate) async fn compact(
    request: &Request,
    env: &Env,
    mut prepared: PreparedCompact,
) -> Result<Response> {
    let key = match request.headers().get("Idempotency-Key")? {
        Some(key) if valid_key(&key) => key,
        Some(_) => {
            return util::json_error(
                400,
                "invalid_request",
                "Idempotency-Key must contain 1–256 visible ASCII characters",
            );
        }
        None => uuid::Uuid::new_v4().to_string(),
    };
    prepared.operation = operation_id(&prepared.owner, &key);
    let namespace = env.durable_object("MODEL_OPERATIONS")?;
    let stub = namespace.id_from_name(&prepared.operation)?.get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    init.with_body(Some(serde_json::to_string(&prepared)?.into()));
    init.headers.set("Content-Type", "application/json")?;
    let response = stub
        .fetch_with_request(Request::new_with_init(
            "https://model-operation/compact",
            &init,
        )?)
        .await?;
    let mut response = observer_transport::resolve(response).await?;
    response.headers_mut().set("Idempotency-Key", &key)?;
    response
        .headers_mut()
        .set("X-Agents-Operation", &prepared.operation)?;
    Ok(response)
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 256 && key.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_are_bounded_and_header_safe() {
        assert!(valid_key("compact-retry_01"));
        assert!(!valid_key(""));
        assert!(!valid_key("line\nbreak"));
        assert!(!valid_key(&"x".repeat(257)));
    }
}
