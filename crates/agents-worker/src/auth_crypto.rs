//! Confidential-client private operations stay in the platform's Web Crypto.
//! No software signing fallback: import/sign failures stop the OAuth exchange.

use agents_auth::prepare_client_assertion;
use js_sys::{Array, ArrayBuffer, Object, Uint8Array};
use serde_json::Value;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use worker::{Error, Result};

pub(super) async fn client_assertion(
    private_jwk: &Value,
    client_id: &str,
    token_endpoint: &str,
    now: i64,
) -> Result<String> {
    let unsigned = prepare_client_assertion(private_jwk, client_id, token_endpoint, now)
        .map_err(|_| Error::RustError("authentication_failed".into()))?;
    // JSON.parse produces a native JWK dictionary, not serde's JS Map encoding.
    let jwk = js_sys::JSON::parse(&private_jwk.to_string())
        .map_err(|_| crypto_failure())?
        .dyn_into::<Object>()
        .map_err(|_| crypto_failure())?;
    let global: web_sys::WorkerGlobalScope = js_sys::global().unchecked_into();
    let subtle = global.crypto().map_err(|_| crypto_failure())?.subtle();
    let import = web_sys::EcKeyImportParams::new("ECDSA");
    import.set_named_curve("P-256");
    let usages = Array::of1(&"sign".into());
    let key = JsFuture::from(
        subtle
            .import_key_with_object("jwk", &jwk, import.as_ref(), false, &usages)
            .map_err(|_| crypto_failure())?,
    )
    .await
    .map_err(|_| crypto_failure())?
    .dyn_into::<web_sys::CryptoKey>()
    .map_err(|_| crypto_failure())?;
    let algorithm = web_sys::EcdsaParams::new_with_str("ECDSA", "SHA-256");
    let signature = JsFuture::from(
        subtle
            .sign_with_object_and_u8_array(
                algorithm.as_ref(),
                &key,
                unsigned.signing_input().as_bytes(),
            )
            .map_err(|_| crypto_failure())?,
    )
    .await
    .map_err(|_| crypto_failure())?
    .dyn_into::<ArrayBuffer>()
    .map_err(|_| crypto_failure())?;
    // Web Crypto ECDSA returns IEEE-P1363 r||s, exactly the JOSE ES256 format.
    unsigned
        .finish(&Uint8Array::new(&signature).to_vec())
        .map_err(|_| crypto_failure())
}

fn crypto_failure() -> Error {
    Error::RustError("identity_crypto_failed".into())
}
