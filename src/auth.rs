use crate::config::Config;
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::Deserialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Principal(pub String);
#[derive(Deserialize)]
struct Claims {
    sub: String,
    scope: Option<String>,
    #[serde(default)]
    typ: Option<String>,
}
pub struct Auth {
    pub config: Arc<Config>,
    client: reqwest::Client,
    jwks_uri: String,
    keys: Mutex<(JwkSet, Instant)>,
}
async fn bounded_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> anyhow::Result<T> {
    const LIMIT: usize = 256 * 1024;
    anyhow::ensure!(
        response.content_length().is_none_or(|n| n <= LIMIT as u64),
        "OIDC response too large"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            chunk.len() <= LIMIT - bytes.len(),
            "OIDC response too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
impl Auth {
    pub async fn new(config: Arc<Config>) -> anyhow::Result<Arc<Self>> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let discovery: serde_json::Value = bounded_json(
            client
                .get(format!(
                    "{}/.well-known/openid-configuration",
                    config.issuer.trim_end_matches('/')
                ))
                .send()
                .await?
                .error_for_status()?,
        )
        .await?;
        anyhow::ensure!(
            discovery["issuer"].as_str() == Some(&config.issuer),
            "OIDC issuer mismatch"
        );
        let jwks_uri = discovery["jwks_uri"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing JWKS URI"))?
            .to_owned();
        let issuer_url = reqwest::Url::parse(&config.issuer)?;
        let jwks_url = reqwest::Url::parse(&jwks_uri)?;
        anyhow::ensure!(
            jwks_url.scheme() == "https" && issuer_url.host_str() == jwks_url.host_str(),
            "JWKS must use the configured HTTPS issuer host"
        );
        let keys = bounded_json(client.get(&jwks_uri).send().await?.error_for_status()?).await?;
        Ok(Arc::new(Self {
            config,
            client,
            jwks_uri,
            keys: Mutex::new((keys, Instant::now())),
        }))
    }
    async fn verify(&self, token: &str) -> Option<Principal> {
        if token.len() > 16384 {
            return None;
        }
        let h = decode_header(token).ok()?;
        if h.alg != Algorithm::RS256 {
            return None;
        }
        let kid = h.kid?;
        let mut keys = self.keys.lock().await;
        if keys.1.elapsed() > Duration::from_secs(300)
            || (keys.0.find(&kid).is_none() && keys.1.elapsed() > Duration::from_secs(10))
        {
            let response = self
                .client
                .get(&self.jwks_uri)
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?;
            if response.content_length().is_some_and(|n| n > 256 * 1024) {
                return None;
            }
            let updated: JwkSet = bounded_json(response).await.ok()?;
            *keys = (updated, Instant::now());
        }
        let key = DecodingKey::from_jwk(keys.0.find(&kid)?).ok()?;
        drop(keys);
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.config.issuer]);
        validation.set_audience(&[&self.config.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = 30;
        let claims = decode::<Claims>(token, &key, &validation).ok()?.claims;
        if !self.config.subjects.contains(&claims.sub)
            || !claims
                .scope
                .as_deref()
                .unwrap_or("")
                .split_whitespace()
                .any(|s| s == self.config.scope)
        {
            return None;
        }
        if claims
            .typ
            .as_deref()
            .is_some_and(|s| s != "Bearer" && s != "at+jwt")
        {
            return None;
        }
        Some(Principal(claims.sub))
    }
}
pub async fn require_auth(State(auth): State<Arc<Auth>>, mut req: Request, next: Next) -> Response {
    let started = Instant::now();
    let request_id = uuid::Uuid::new_v4().to_string();
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    let principal = match token {
        Some(token) => auth.verify(token).await,
        None => None,
    };
    let mut response = if let Some(principal) = principal {
        req.extensions_mut().insert(principal);
        req.extensions_mut().insert(RequestId(request_id.clone()));
        next.run(req).await
    } else {
        let mut r = StatusCode::UNAUTHORIZED.into_response();
        let metadata = format!(
            "{}/.well-known/oauth-protected-resource",
            auth.config.resource.trim_end_matches("/mcp")
        );
        if let Ok(v) = format!(
            "Bearer resource_metadata=\"{metadata}\", scope=\"{}\"",
            auth.config.scope
        )
        .parse()
        {
            r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
        r
    };
    if let Ok(v) = request_id.parse() {
        response.headers_mut().insert("x-request-id", v);
    }
    tracing::info!(
        event = "http_request",
        request_id,
        status = response.status().as_u16(),
        duration_ms = started.elapsed().as_millis() as u64
    );
    response
}
#[derive(Clone)]
pub struct RequestId(pub String);

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::{
        encoding::{AsDer, Pkcs8V1Der},
        rsa::{KeyPair, KeySize},
        signature::KeyPair as _,
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;
    #[tokio::test]
    async fn jwt_trust_boundaries() {
        let key = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let public = key.public_key();
        let jwks:JwkSet=serde_json::from_value(json!({"keys":[{"kty":"RSA","kid":"test","alg":"RS256","use":"sig","n":URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),"e":URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero())}]})).unwrap();
        let der = AsDer::<Pkcs8V1Der>::as_der(&key).unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
            base64::engine::general_purpose::STANDARD.encode(der.as_ref())
        );
        let signing = EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap();
        let auth = Auth {
            config: crate::config::fixture(),
            client: reqwest::Client::new(),
            jwks_uri: "https://invalid.example/jwks".into(),
            keys: Mutex::new((jwks, Instant::now())),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = json!({"sub":"owner","iss":"https://auth.example.com/","aud":"https://mail.example.com/mcp","exp":now+300,"nbf":now-1,"scope":"openid email:mcp","typ":"Bearer"});
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test".into());
        let token = encode(&header, &claims, &signing).unwrap();
        assert!(auth.verify(&token).await.is_some());
        for (field, value) in [
            ("sub", json!("attacker")),
            ("iss", json!("https://other.example/")),
            ("aud", json!("other-api")),
            ("exp", json!(now - 300)),
            ("nbf", json!(now + 300)),
            ("scope", json!("openid")),
            ("typ", json!("ID")),
        ] {
            let mut changed = claims.clone();
            changed[field] = value;
            assert!(
                auth.verify(&encode(&header, &changed, &signing).unwrap())
                    .await
                    .is_none(),
                "{field}"
            );
        }
        assert!(auth.verify("invalid").await.is_none());
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"test"),
        )
        .unwrap();
        assert!(auth.verify(&token).await.is_none());
    }
}
