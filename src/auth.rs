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
    refresh: Mutex<RefreshState>,
}
const KEY_TTL: Duration = Duration::from_secs(300);
const REFRESH_COOLDOWN: Duration = Duration::from_secs(10);

struct RefreshState {
    completed: Instant,
    failed: bool,
}

#[derive(Debug, PartialEq)]
enum AuthError {
    InvalidToken,
    Unavailable,
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
            refresh: Mutex::new(RefreshState {
                completed: Instant::now(),
                failed: false,
            }),
        }))
    }
    async fn key_for(&self, kid: &str) -> Result<DecodingKey, AuthError> {
        {
            let keys = self.keys.lock().await;
            if keys.1.elapsed() < KEY_TTL
                && let Some(key) = keys.0.find(kid)
            {
                return DecodingKey::from_jwk(key).map_err(|_| AuthError::InvalidToken);
            }
        }
        // Only refreshers wait on this lock. Cached-key readers never wait for I/O.
        let mut refresh = self.refresh.lock().await;
        {
            let keys = self.keys.lock().await;
            if keys.1.elapsed() < KEY_TTL
                && let Some(key) = keys.0.find(kid)
            {
                return DecodingKey::from_jwk(key).map_err(|_| AuthError::InvalidToken);
            }
        }
        if refresh.completed.elapsed() < REFRESH_COOLDOWN {
            return Err(if refresh.failed {
                AuthError::Unavailable
            } else {
                AuthError::InvalidToken
            });
        }
        // Cancellation also leaves a bounded failure cooldown, never a fresh key cache.
        refresh.completed = Instant::now();
        refresh.failed = true;
        let result = async {
            let response = self
                .client
                .get(&self.jwks_uri)
                .send()
                .await?
                .error_for_status()?;
            bounded_json::<JwkSet>(response).await
        }
        .await;
        refresh.completed = Instant::now();
        let updated = result.map_err(|_| AuthError::Unavailable)?;
        let key = updated
            .find(kid)
            .and_then(|key| DecodingKey::from_jwk(key).ok());
        *self.keys.lock().await = (updated, Instant::now());
        refresh.failed = false;
        key.ok_or(AuthError::InvalidToken)
    }
    async fn verify(&self, token: &str) -> Result<Principal, AuthError> {
        if token.len() > 16384 {
            return Err(AuthError::InvalidToken);
        }
        let h = decode_header(token).map_err(|_| AuthError::InvalidToken)?;
        if h.alg != Algorithm::RS256 {
            return Err(AuthError::InvalidToken);
        }
        let kid = h.kid.ok_or(AuthError::InvalidToken)?;
        let key = self.key_for(&kid).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.config.issuer]);
        validation.set_audience(&[&self.config.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = 30;
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|_| AuthError::InvalidToken)?
            .claims;
        if !self.config.subjects.contains(&claims.sub)
            || !claims
                .scope
                .as_deref()
                .unwrap_or("")
                .split_whitespace()
                .any(|s| s == self.config.scope)
        {
            return Err(AuthError::InvalidToken);
        }
        if claims
            .typ
            .as_deref()
            .is_some_and(|s| s != "Bearer" && s != "at+jwt")
        {
            return Err(AuthError::InvalidToken);
        }
        Ok(Principal(claims.sub))
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
        None => Err(AuthError::InvalidToken),
    };
    let mut response = if let Ok(principal) = principal {
        req.extensions_mut().insert(principal);
        req.extensions_mut().insert(RequestId(request_id.clone()));
        next.run(req).await
    } else if matches!(principal, Err(AuthError::Unavailable)) {
        let mut r = StatusCode::SERVICE_UNAVAILABLE.into_response();
        r.headers_mut()
            .insert(header::RETRY_AFTER, header::HeaderValue::from_static("10"));
        r
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_auth(jwks: JwkSet, uri: String) -> Arc<Auth> {
        Arc::new(Auth {
            config: crate::config::fixture(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(1))
                .build()
                .unwrap(),
            jwks_uri: uri,
            keys: Mutex::new((jwks, Instant::now())),
            refresh: Mutex::new(RefreshState {
                completed: Instant::now() - REFRESH_COOLDOWN,
                failed: false,
            }),
        })
    }

    fn test_keys(kid: &str) -> JwkSet {
        let key = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let public = key.public_key();
        serde_json::from_value(json!({"keys":[{"kty":"RSA","kid":kid,"alg":"RS256","use":"sig","n":URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),"e":URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero())}]})).unwrap()
    }

    #[tokio::test]
    async fn failed_refresh_is_shared_and_expired_keys_fail_closed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let app = axum::Router::new().route(
            "/jwks",
            axum::routing::get(move || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth = test_auth(
            test_keys("test"),
            format!("http://{}/jwks", listener.local_addr().unwrap()),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        auth.keys.lock().await.1 = Instant::now() - KEY_TTL;
        let (a, b, c) = tokio::join!(
            auth.key_for("test"),
            auth.key_for("test"),
            auth.key_for("unknown")
        );
        for result in [a, b, c] {
            assert!(matches!(result, Err(AuthError::Unavailable)));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            auth.key_for("test").await,
            Err(AuthError::Unavailable)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The cooldown expires and permits one further attempt, without extending trust.
        auth.refresh.lock().await.completed = Instant::now() - REFRESH_COOLDOWN;
        assert!(matches!(
            auth.key_for("test").await,
            Err(AuthError::Unavailable)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        auth.keys.lock().await.1 = Instant::now();
        assert!(auth.key_for("test").await.is_ok());
        auth.keys.lock().await.1 = Instant::now() - KEY_TTL;

        use tower::ServiceExt;
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(auth, require_auth));
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"test"}"#);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::AUTHORIZATION, format!("Bearer {header}.e30.AA"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "10");
        assert!(!response.headers().contains_key(header::WWW_AUTHENTICATE));
        server.abort();
    }

    #[tokio::test]
    async fn cached_key_does_not_wait_for_rotation_and_refresh_updates_snapshot() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = release.clone();
        let rotated = serde_json::to_string(&test_keys("rotated")).unwrap();
        let app = axum::Router::new().route(
            "/jwks",
            axum::routing::get(move || {
                let started_tx = started_tx.clone();
                let gate = gate.clone();
                let rotated = rotated.clone();
                async move {
                    if let Some(tx) = started_tx.lock().await.take() {
                        let _ = tx.send(());
                    }
                    gate.notified().await;
                    rotated
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth = test_auth(
            test_keys("test"),
            format!("http://{}/jwks", listener.local_addr().unwrap()),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let refresh_auth = auth.clone();
        let refresh = tokio::spawn(async move { refresh_auth.key_for("rotated").await });
        started_rx.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), auth.key_for("test"))
                .await
                .unwrap()
                .is_ok()
        );
        release.notify_one();
        assert!(refresh.await.unwrap().is_ok());
        assert!(auth.key_for("rotated").await.is_ok());
        assert!(matches!(
            auth.key_for("test").await,
            Err(AuthError::InvalidToken)
        ));
        assert!(matches!(
            auth.key_for("random").await,
            Err(AuthError::InvalidToken)
        ));
        server.abort();
    }
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
            refresh: Mutex::new(RefreshState {
                completed: Instant::now(),
                failed: false,
            }),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = json!({"sub":"owner","iss":"https://auth.example.com/","aud":"https://mail.example.com/mcp","exp":now+300,"nbf":now-1,"scope":"openid email:mcp","typ":"Bearer"});
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test".into());
        let token = encode(&header, &claims, &signing).unwrap();
        assert!(auth.verify(&token).await.is_ok());
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
                    .is_err(),
                "{field}"
            );
        }
        assert!(auth.verify("invalid").await.is_err());
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"test"),
        )
        .unwrap();
        assert!(auth.verify(&token).await.is_err());
    }

    #[tokio::test]
    async fn attachment_route_requires_bearer_before_handler() {
        use axum::{Router, body::Body, http::Request, middleware, routing::get};
        use tower::ServiceExt;
        let config = crate::config::fixture();
        let auth = Arc::new(Auth {
            config: config.clone(),
            client: reqwest::Client::new(),
            jwks_uri: "https://invalid.example/jwks".into(),
            keys: Mutex::new((JwkSet { keys: vec![] }, Instant::now())),
            refresh: Mutex::new(RefreshState {
                completed: Instant::now(),
                failed: false,
            }),
        });
        let mail = Arc::new(
            crate::mail::MailService::new(
                config,
                Arc::new(crate::journal::Journal::open(std::path::Path::new(":memory:")).unwrap()),
            )
            .unwrap(),
        );
        // Match the production route layering, including the nested MCP service.
        let router = Router::new()
            .route(
                "/mcp/attachments/{token}",
                get(crate::mail::attachments::download),
            )
            .with_state(mail)
            .nest_service("/mcp", Router::new().route("/", get(|| async { "mcp" })))
            .layer(middleware::from_fn_with_state(auth, require_auth));
        for bearer in [None, Some("Bearer invalid")] {
            let mut request = Request::builder().uri("/mcp/attachments/unknown");
            if let Some(bearer) = bearer {
                request = request.header(header::AUTHORIZATION, bearer);
            }
            let response = router
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
            assert!(response.headers().contains_key("x-request-id"));
        }
    }
}
