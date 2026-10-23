use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use p256::pkcs8::EncodePrivateKey;
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

pub fn client_assertion(
    private_jwk: &Value,
    client_id: &str,
    token_endpoint: &str,
    now: i64,
) -> Result<String, AuthError> {
    if private_jwk["kty"] != "EC" || private_jwk["crv"] != "P-256" || private_jwk["alg"] != "ES256"
    {
        return Err(AuthError::Configuration);
    }
    let d = URL_SAFE_NO_PAD
        .decode(private_jwk["d"].as_str().ok_or(AuthError::Configuration)?)
        .map_err(|_| AuthError::Configuration)?;
    let key = p256::SecretKey::from_slice(&d).map_err(|_| AuthError::Configuration)?;
    let der = key.to_pkcs8_der().map_err(|_| AuthError::Configuration)?;
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(
        private_jwk["kid"]
            .as_str()
            .ok_or(AuthError::Configuration)?
            .to_owned(),
    );
    jsonwebtoken::encode(&header, &json!({"iss":client_id,"sub":client_id,"aud":token_endpoint,"iat":now,"exp":now+120,"jti":random_token()?}), &EncodingKey::from_ec_der(der.as_bytes()))
        .map_err(|_| AuthError::Configuration)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let key = p256::SecretKey::from_slice(&[1u8; 32]).unwrap();
        let point = key.public_key().to_encoded_point(false);
        let x = URL_SAFE_NO_PAD.encode(point.x().unwrap());
        let y = URL_SAFE_NO_PAD.encode(point.y().unwrap());
        let jwk = json!({"kty":"EC","crv":"P-256","alg":"ES256","kid":"fixture","d":URL_SAFE_NO_PAD.encode([1u8;32])});
        let first = client_assertion(&jwk, "web", "https://issuer.test/token", 100).unwrap();
        let second = client_assertion(&jwk, "web", "https://issuer.test/token", 100).unwrap();
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
}
