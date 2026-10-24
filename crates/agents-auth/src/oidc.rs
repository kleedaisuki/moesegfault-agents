use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::{AuthError, constant_eq, hash_secret, random_token};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    pub end_session_endpoint: String,
    pub revocation_endpoint: String,
    pub code_challenge_methods_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
}

impl Discovery {
    pub fn validate(&self, issuer: &str) -> Result<(), AuthError> {
        if self.issuer != issuer
            || !self
                .code_challenge_methods_supported
                .iter()
                .any(|s| s == "S256")
        {
            return Err(AuthError::Configuration);
        }
        let expected = Url::parse(issuer).map_err(|_| AuthError::Configuration)?;
        if expected.scheme() != "https"
            || expected.username() != ""
            || expected.password().is_some()
        {
            return Err(AuthError::Configuration);
        }
        for endpoint in [
            &self.authorization_endpoint,
            &self.token_endpoint,
            &self.jwks_uri,
            &self.end_session_endpoint,
            &self.revocation_endpoint,
        ] {
            let url = Url::parse(endpoint).map_err(|_| AuthError::Configuration)?;
            if url.origin() != expected.origin()
                || url.fragment().is_some()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(AuthError::Configuration);
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub struct LoginTransaction {
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    pub issuer: String,
    pub return_to: String,
    pub expires_at: i64,
    pub browser_binding: String,
}

impl LoginTransaction {
    pub fn new(issuer: &str, return_to: &str, now: i64) -> Result<Self, AuthError> {
        if !safe_return_path(return_to) {
            return Err(AuthError::Transaction);
        }
        Ok(Self {
            state: random_token()?,
            nonce: random_token()?,
            verifier: random_token()?,
            issuer: issuer.into(),
            return_to: return_to.into(),
            expires_at: now + 300,
            browser_binding: random_token()?,
        })
    }
    pub fn authorization_url(
        &self,
        discovery: &Discovery,
        client_id: &str,
        redirect_uri: &str,
    ) -> Result<Url, AuthError> {
        discovery.validate(&self.issuer)?;
        let mut url =
            Url::parse(&discovery.authorization_endpoint).map_err(|_| AuthError::Configuration)?;
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("scope", "openid profile offline_access"),
            ("state", &self.state),
            ("nonce", &self.nonce),
            ("code_challenge", &hash_secret(&self.verifier)),
            ("code_challenge_method", "S256"),
        ]);
        Ok(url)
    }
    pub fn validate_callback(
        &self,
        state: &str,
        issuer: &str,
        browser_binding: &str,
        now: i64,
    ) -> Result<(), AuthError> {
        if self.expires_at <= now
            || !constant_eq(&self.state, state)
            || issuer != self.issuer
            || !constant_eq(&self.browser_binding, browser_binding)
        {
            return Err(AuthError::Transaction);
        }
        Ok(())
    }
}

pub fn safe_return_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path.to_ascii_lowercase().contains("%2f")
        && !path.to_ascii_lowercase().contains("%5c")
        && path.len() <= 2048
}

#[derive(Clone, Deserialize, Serialize)]
pub struct IdentityClaims {
    pub iss: String,
    pub sub: String,
    pub aud: Value,
    pub exp: i64,
    pub iat: i64,
    pub token_use: String,
    pub nonce: Option<String>,
    pub scope: Option<String>,
    #[serde(default)]
    pub nbf: Option<i64>,
}

/// Signature verification is always RS256; token data cannot select a different algorithm.
pub fn verify_identity_token(
    wire: &str,
    jwks: &Value,
    issuer: &str,
    audience: &str,
    nonce: Option<&str>,
    now: i64,
) -> Result<IdentityClaims, AuthError> {
    let header = jsonwebtoken::decode_header(wire).map_err(|_| AuthError::Token)?;
    if header.alg != Algorithm::RS256
        || header.jku.is_some()
        || header.jwk.is_some()
        || header.x5u.is_some()
    {
        return Err(AuthError::Token);
    }
    let kid = header.kid.as_deref().ok_or(AuthError::Token)?;
    let keys = jwks
        .get("keys")
        .and_then(Value::as_array)
        .ok_or(AuthError::Token)?;
    let key = keys
        .iter()
        .find(|key| key["kid"].as_str() == Some(kid))
        .ok_or(AuthError::Token)?;
    if key["kty"] != "RSA"
        || key["alg"] != "RS256"
        || key.get("use").is_some_and(|v| v != "sig")
        || key.get("key_ops").is_some_and(|v| {
            !v.as_array()
                .is_some_and(|ops| ops.iter().any(|op| op == "verify"))
        })
    {
        return Err(AuthError::Token);
    }
    let decoding_key = DecodingKey::from_rsa_components(
        key["n"].as_str().ok_or(AuthError::Token)?,
        key["e"].as_str().ok_or(AuthError::Token)?,
    )
    .map_err(|_| AuthError::Token)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    // The transport supplies its clock, making policy tests deterministic on native and WASM.
    validation.validate_exp = false;
    let claims = jsonwebtoken::decode::<IdentityClaims>(wire, &decoding_key, &validation)
        .map_err(|_| AuthError::Token)?
        .claims;
    validate_claims(&claims, issuer, audience, nonce, now)?;
    Ok(claims)
}

pub fn validate_claims(
    claims: &IdentityClaims,
    issuer: &str,
    audience: &str,
    nonce: Option<&str>,
    now: i64,
) -> Result<(), AuthError> {
    let audience_matches = match &claims.aud {
        Value::String(aud) => aud == audience,
        Value::Array(aud) => aud.len() == 1 && aud[0].as_str() == Some(audience),
        _ => false,
    };
    let expected_use = if nonce.is_some() { "id" } else { "access" };
    if claims.iss != issuer
        || !audience_matches
        || claims.sub.is_empty()
        || claims.exp <= now - 30
        || claims.iat > now + 30
        || claims.nbf.is_some_and(|nbf| nbf > now + 30)
        || claims.exp <= claims.iat
        || claims.token_use != expected_use
    {
        return Err(AuthError::Token);
    }
    if let Some(nonce) = nonce {
        if !claims
            .nonce
            .as_deref()
            .is_some_and(|actual| constant_eq(actual, nonce))
        {
            return Err(AuthError::Token);
        }
    } else if !claims
        .scope
        .as_deref()
        .unwrap_or_default()
        .split_ascii_whitespace()
        .any(|s| s == "openid")
    {
        return Err(AuthError::Token);
    }
    Ok(())
}

/// Unsigned public JWT framing. Private-key arithmetic belongs to native
/// WebCrypto, not the software verifier's cryptographic dependencies.
pub struct ClientAssertionInput {
    signing_input: String,
}

impl ClientAssertionInput {
    pub fn signing_input(&self) -> &str {
        &self.signing_input
    }

    /// ES256 JOSE and WebCrypto both use the 64-byte R || S representation,
    /// not ASN.1 DER: <https://www.rfc-editor.org/rfc/rfc7518#section-3.4>.
    pub fn finish(self, signature: &[u8]) -> Result<String, AuthError> {
        if signature.len() != 64 {
            return Err(AuthError::Configuration);
        }
        Ok(format!(
            "{}.{}",
            self.signing_input,
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }
}

pub fn prepare_client_assertion(
    private_jwk: &Value,
    client_id: &str,
    token_endpoint: &str,
    now: i64,
) -> Result<ClientAssertionInput, AuthError> {
    if private_jwk["kty"] != "EC" || private_jwk["crv"] != "P-256" || private_jwk["alg"] != "ES256"
    {
        return Err(AuthError::Configuration);
    }
    for field in ["d", "x", "y"] {
        validate_jwk_coordinate(&private_jwk[field])?;
    }
    let kid = private_jwk["kid"]
        .as_str()
        .ok_or(AuthError::Configuration)?;
    if !bounded_identifier(kid, 128)
        || !bounded_identifier(client_id, 256)
        || private_jwk.get("use").is_some_and(|value| value != "sig")
        || private_jwk
            .get("ext")
            .is_some_and(|value| !value.is_boolean())
        || private_jwk
            .get("key_ops")
            .is_some_and(|value| !signing_operations(value))
        || now < 0
    {
        return Err(AuthError::Configuration);
    }
    let endpoint = Url::parse(token_endpoint).map_err(|_| AuthError::Configuration)?;
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(AuthError::Configuration);
    }
    let expires_at = now.checked_add(120).ok_or(AuthError::Configuration)?;
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(kid.to_owned());
    let claims = json!({"iss":client_id,"sub":client_id,"aud":token_endpoint,"iat":now,"exp":expires_at,"jti":random_token()?});
    let header = serde_json::to_vec(&header).map_err(|_| AuthError::Configuration)?;
    let claims = serde_json::to_vec(&claims).map_err(|_| AuthError::Configuration)?;
    Ok(ClientAssertionInput {
        signing_input: format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header),
            URL_SAFE_NO_PAD.encode(claims)
        ),
    })
}

fn bounded_identifier(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

fn validate_jwk_coordinate(value: &Value) -> Result<(), AuthError> {
    let encoded = value.as_str().ok_or(AuthError::Configuration)?;
    if encoded.len() != 43 {
        return Err(AuthError::Configuration);
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| AuthError::Configuration)?;
    if decoded.len() != 32 || URL_SAFE_NO_PAD.encode(&decoded) != encoded {
        return Err(AuthError::Configuration);
    }
    Ok(())
}

fn signing_operations(value: &Value) -> bool {
    let Some(operations) = value.as_array() else {
        return false;
    };
    operations.iter().any(|operation| operation == "sign")
        && operations.iter().enumerate().all(|(index, operation)| {
            matches!(operation.as_str(), Some("sign" | "verify"))
                && !operations[..index].contains(operation)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::EncodingKey;
    #[test]
    fn return_paths_do_not_escape_origin() {
        for path in [
            "https://evil.test",
            "//evil.test",
            "/\\evil.test",
            "/%2Fevil",
            "/%5Cevil",
            "/\r\nLocation:evil",
        ] {
            assert!(!safe_return_path(path));
        }
        assert!(safe_return_path("/agents?tab=active"));
    }
    #[test]
    fn transaction_rejects_mixup_and_stolen_callback() {
        let tx = LoginTransaction::new("https://issuer.test", "/", 100).unwrap();
        assert!(
            tx.validate_callback(&tx.state, &tx.issuer, &tx.browser_binding, 101)
                .is_ok()
        );
        assert!(
            tx.validate_callback(&tx.state, &tx.issuer, "attacker", 101)
                .is_err()
        );
        assert!(
            tx.validate_callback(&tx.state, "https://other.test", &tx.browser_binding, 101)
                .is_err()
        );
        assert!(
            tx.validate_callback(&tx.state, &tx.issuer, &tx.browser_binding, 400)
                .is_err()
        );
    }
    #[test]
    fn token_purpose_audience_and_scopes_are_not_interchangeable() {
        let mut claims = IdentityClaims {
            iss: "https://issuer.test".into(),
            sub: "opaque".into(),
            aud: json!("web"),
            exp: 400,
            iat: 100,
            token_use: "id".into(),
            nonce: Some("nonce".into()),
            scope: Some("openid".into()),
            nbf: None,
        };
        assert!(validate_claims(&claims, &claims.iss, "web", Some("nonce"), 200).is_ok());
        assert!(validate_claims(&claims, &claims.iss, "native", Some("nonce"), 200).is_err());
        assert!(validate_claims(&claims, &claims.iss, "web", None, 200).is_err());
        claims.token_use = "access".into();
        assert!(validate_claims(&claims, &claims.iss, "web", None, 200).is_ok());
        claims.scope = Some("profile".into());
        assert!(validate_claims(&claims, &claims.iss, "web", None, 200).is_err());
    }
    #[test]
    fn real_rs256_signature_and_jwk_metadata_are_enforced() {
        use rsa::{RsaPrivateKey, pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts};
        let key = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("fixture".into());
        let claims = json!({"iss":"https://issuer.test","sub":"opaque","aud":"native","iat":100,"exp":400,"scope":"openid","token_use":"access"});
        let wire = jsonwebtoken::encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_der(key.to_pkcs1_der().unwrap().as_bytes()),
        )
        .unwrap();
        let mut jwks = json!({"keys":[{"kid":"fixture","kty":"RSA","alg":"RS256","use":"sig","n":URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(key.e().to_bytes_be())}]});
        assert!(
            verify_identity_token(&wire, &jwks, "https://issuer.test", "native", None, 200).is_ok()
        );
        assert!(
            verify_identity_token(&wire, &jwks, "https://other.test", "native", None, 200).is_err()
        );
        assert!(
            verify_identity_token(&wire, &jwks, "https://issuer.test", "web", None, 200).is_err()
        );
        assert!(
            verify_identity_token(&wire, &jwks, "https://issuer.test", "native", None, 500)
                .is_err()
        );
        let mut tampered = wire.clone().into_bytes();
        let last = tampered.len() - 3;
        tampered[last] = if tampered[last] == b'a' { b'b' } else { b'a' };
        assert!(
            verify_identity_token(
                std::str::from_utf8(&tampered).unwrap(),
                &jwks,
                "https://issuer.test",
                "native",
                None,
                200
            )
            .is_err()
        );
        jwks["keys"][0]["use"] = json!("enc");
        assert!(
            verify_identity_token(&wire, &jwks, "https://issuer.test", "native", None, 200)
                .is_err()
        );
    }

    #[test]
    fn client_assertions_use_registered_es256_key_and_unique_jti() {
        use p256::ecdsa::{Signature, SigningKey, signature::Signer};
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let key = p256::SecretKey::from_slice(&[1u8; 32]).unwrap();
        let point = key.public_key().to_encoded_point(false);
        let x = URL_SAFE_NO_PAD.encode(point.x().unwrap());
        let y = URL_SAFE_NO_PAD.encode(point.y().unwrap());
        let jwk = json!({"kty":"EC","crv":"P-256","alg":"ES256","kid":"fixture","d":URL_SAFE_NO_PAD.encode([1u8;32]),"x":x,"y":y});
        let sign = |input: ClientAssertionInput| {
            let key = SigningKey::from_slice(&[1u8; 32]).unwrap();
            let signature: Signature = key.sign(input.signing_input().as_bytes());
            input.finish(&signature.to_bytes()).unwrap()
        };
        let first =
            sign(prepare_client_assertion(&jwk, "web", "https://issuer.test/token", 100).unwrap());
        let second =
            sign(prepare_client_assertion(&jwk, "web", "https://issuer.test/token", 100).unwrap());
        let mut validation = Validation::new(Algorithm::ES256);
        validation.validate_exp = false;
        validation.set_audience(&["https://issuer.test/token"]);
        validation.set_issuer(&["web"]);
        let public = DecodingKey::from_ec_components(&x, &y).unwrap();
        let a = jsonwebtoken::decode::<Value>(&first, &public, &validation)
            .unwrap()
            .claims;
        let b = jsonwebtoken::decode::<Value>(&second, &public, &validation)
            .unwrap()
            .claims;
        assert_eq!(a["sub"], "web");
        assert_eq!(a["exp"], 220);
        assert_ne!(a["jti"], b["jti"]);
    }

    fn framing_jwk() -> Value {
        json!({"kty":"EC","crv":"P-256","alg":"ES256","kid":"fixture",
            "d":URL_SAFE_NO_PAD.encode([1u8;32]),
            "x":URL_SAFE_NO_PAD.encode([2u8;32]),
            "y":URL_SAFE_NO_PAD.encode([3u8;32])})
    }

    #[test]
    fn unsigned_assertion_contains_only_public_header_and_exact_claims() {
        let jwk = framing_jwk();
        let input =
            prepare_client_assertion(&jwk, "client", "https://issuer.test/token", 100).unwrap();
        let segments: Vec<_> = input.signing_input().split('.').collect();
        assert_eq!(segments.len(), 2);
        let decode = |segment: &str| {
            serde_json::from_slice::<Value>(&URL_SAFE_NO_PAD.decode(segment).unwrap()).unwrap()
        };
        assert_eq!(
            decode(segments[0]),
            json!({"alg":"ES256","typ":"JWT","kid":"fixture"})
        );
        let claims = decode(segments[1]);
        assert_eq!(claims.as_object().unwrap().len(), 6);
        assert_eq!(claims["iss"], "client");
        assert_eq!(claims["sub"], "client");
        assert_eq!(claims["aud"], "https://issuer.test/token");
        assert_eq!(claims["iat"], 100);
        assert_eq!(claims["exp"], 220);
        assert!(!claims["jti"].as_str().unwrap().is_empty());
        assert!(claims.get("d").is_none());
        assert!(claims.get("jwk").is_none());
    }

    #[test]
    fn assertion_metadata_coordinates_and_time_are_strict() {
        let jwk = framing_jwk();
        for (field, value) in [
            ("kty", json!("RSA")),
            ("crv", json!("P-384")),
            ("alg", json!("HS256")),
            ("kid", json!("")),
            ("kid", json!("x".repeat(129))),
            ("kid", json!("key\n")),
            ("use", json!("enc")),
            ("key_ops", json!(["verify"])),
            ("key_ops", json!(["sign", 1])),
            ("key_ops", json!(["sign", "sign"])),
            ("ext", json!("false")),
        ] {
            let mut invalid = jwk.clone();
            invalid[field] = value;
            assert!(
                prepare_client_assertion(&invalid, "client", "https://issuer.test/token", 100)
                    .is_err(),
                "{field}"
            );
        }
        for field in ["d", "x", "y"] {
            for value in [
                Value::Null,
                json!(URL_SAFE_NO_PAD.encode([0u8; 31])),
                json!(format!("{}=", jwk[field].as_str().unwrap())),
                json!("!".repeat(43)),
                json!(format!("{}B", "A".repeat(42))),
            ] {
                let mut invalid = jwk.clone();
                invalid[field] = value;
                assert!(
                    prepare_client_assertion(&invalid, "client", "https://issuer.test/token", 100)
                        .is_err(),
                    "{field}"
                );
            }
        }
        for now in [-1, i64::MAX] {
            assert!(
                prepare_client_assertion(&jwk, "client", "https://issuer.test/token", now).is_err()
            );
        }
        for endpoint in [
            "http://issuer.test/token",
            "https://user:pass@issuer.test/token",
            "https://issuer.test/token#fragment",
        ] {
            assert!(prepare_client_assertion(&jwk, "client", endpoint, 100).is_err());
        }
        assert!(prepare_client_assertion(&jwk, "", "https://issuer.test/token", 100).is_err());
        let mut valid = jwk.clone();
        valid["use"] = json!("sig");
        valid["key_ops"] = json!(["sign"]);
        valid["ext"] = json!(false);
        assert!(
            prepare_client_assertion(&valid, "client", "https://issuer.test/token", 100).is_ok()
        );
    }

    #[test]
    fn assertion_assembly_requires_p1363_signature_not_der() {
        let jwk = framing_jwk();
        for length in [0, 32, 63, 65, 70, 72] {
            let input =
                prepare_client_assertion(&jwk, "client", "https://issuer.test/token", 100).unwrap();
            assert!(input.finish(&vec![0; length]).is_err());
        }
        let input =
            prepare_client_assertion(&jwk, "client", "https://issuer.test/token", 100).unwrap();
        let wire = input.finish(&[7; 64]).unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(wire.split('.').nth(2).unwrap())
                .unwrap(),
            vec![7; 64]
        );
    }
}
