//! Google Vertex AI transport. `claude-*` models use `:streamRawPredict` with the Anthropic body
//! (`model` in the URL, `anthropic_version: "vertex-2023-10-16"`); everything else uses the
//! OpenAI-compatible `endpoints/openapi/chat/completions`. The credential is a Google access token.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use serde_json::{json, Value};
use silver_core::error::{CoreError, CoreResult};
use silver_core::model::{Model, ModelRequest, ModelStream};
use tokio_util::sync::CancellationToken;

use crate::anthropic::{anthropic_event_stream, build_anthropic_body};
use crate::provider::OpenAiCompatibleProvider;

/// The `anthropic_version` Vertex requires in the body.
const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// Scope requested for a service-account token.
const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Google's OAuth2 token endpoint.
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// JWT bearer grant type for service accounts.
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// Metadata server token endpoint, available inside Google Cloud.
const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// Refresh an access token this long before it expires.
const REFRESH_MARGIN: Duration = Duration::from_secs(120);

/// Region used when nothing names one. Anthropic models are served here.
const DEFAULT_REGION: &str = "us-east5";

/// Where the access token comes from.
#[derive(Clone, Debug)]
enum Credential {
    /// A bearer token supplied directly (`ya29.…`, or `gcloud auth print-access-token`).
    AccessToken(String),
    /// A service-account key: the client email and its PKCS#8 private key.
    ServiceAccount {
        client_email: String,
        private_key_pem: String,
        project_id: Option<String>,
    },
    /// Ask the metadata server, which is how a workload on Google Cloud authenticates.
    Metadata,
}

/// A token and when it stops being valid.
#[derive(Clone)]
struct CachedToken {
    token: String,
    expires_at: u64,
}

impl CachedToken {
    fn fresh(&self, now: u64) -> bool {
        self.expires_at > now.saturating_add(REFRESH_MARGIN.as_secs())
    }
}

/// Streaming client for Vertex AI.
pub struct VertexProvider {
    credential: Credential,
    project: Option<String>,
    region: String,
    model: String,
    http: reqwest::Client,
    token: Arc<Mutex<Option<CachedToken>>>,
}

impl VertexProvider {
    /// Build a transport from a raw access token, a service-account JSON key, or empty to try
    /// `GOOGLE_APPLICATION_CREDENTIALS`, `GOOGLE_ACCESS_TOKEN`, then the metadata server. Project and
    /// region come from the base URL, else the environment, else the service-account key.
    pub fn new(
        base_url: impl Into<String>,
        credential: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into();
        let (url_project, url_region) = parse_vertex_base_url(&base_url);
        let credential = resolve_credential(&credential.into());
        let project = url_project
            .or_else(|| env_var("VERTEX_PROJECT"))
            .or_else(|| env_var("GOOGLE_CLOUD_PROJECT"))
            .or_else(|| env_var("GCLOUD_PROJECT"))
            .or_else(|| match &credential {
                Credential::ServiceAccount { project_id, .. } => Option::clone(project_id),
                _ => None,
            });
        let region = url_region
            .or_else(|| env_var("VERTEX_REGION"))
            .or_else(|| env_var("GOOGLE_CLOUD_REGION"))
            .or_else(|| env_var("CLOUD_ML_REGION"))
            .unwrap_or_else(|| DEFAULT_REGION.to_string());
        Self {
            credential,
            project,
            region,
            model: model.into(),
            http: reqwest::Client::new(),
            token: Arc::new(Mutex::new(None)),
        }
    }

    /// Replace the HTTP client, e.g. to apply the daemon TLS policy.
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// The project requests are scoped to, when one is known.
    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    /// The API host for this region.
    fn host(&self) -> String {
        format!("https://{}-aiplatform.googleapis.com", self.region)
    }

    /// A usable access token, minted or refreshed as needed.
    async fn access_token(&self) -> Result<String, CoreError> {
        if let Some(cached) = self
            .token
            .lock()
            .ok()
            .and_then(|slot| Option::clone(&slot))
            .filter(|token| token.fresh(now_unix()))
        {
            return Ok(cached.token);
        }
        let minted = match &self.credential {
            Credential::AccessToken(token) => CachedToken {
                token: String::clone(token),
                // A token handed to us carries no expiry; assume the Google default hour and
                // let a 401 surface if it was already stale.
                expires_at: now_unix() + 3600,
            },
            Credential::ServiceAccount {
                client_email,
                private_key_pem,
                ..
            } => self.exchange_jwt(client_email, private_key_pem).await?,
            Credential::Metadata => self.metadata_token().await?,
        };
        let token = String::clone(&minted.token);
        if let Ok(mut slot) = self.token.lock() {
            *slot = Some(minted);
        }
        Ok(token)
    }

    /// Sign a service-account JWT and exchange it for an access token.
    async fn exchange_jwt(
        &self,
        client_email: &str,
        private_key_pem: &str,
    ) -> Result<CachedToken, CoreError> {
        let now = now_unix();
        let assertion = sign_service_account_jwt(client_email, private_key_pem, now)?;
        let response = self
            .http
            .post(TOKEN_URL)
            .form(&[("grant_type", JWT_BEARER_GRANT), ("assertion", &assertion)])
            .send()
            .await
            .map_err(|error| {
                CoreError::ProviderUnavailable(format!("vertex token request failed: {error}"))
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(CoreError::ProviderUnavailable(format!(
                "vertex rejected the service account (HTTP {}): {}",
                status.as_u16(),
                silver_core::redact::redact(&body)
            )));
        }
        parse_token_response(&body, now)
    }

    /// Ask the metadata server for the workload's own token.
    async fn metadata_token(&self) -> Result<CachedToken, CoreError> {
        let response = self
            .http
            .get(METADATA_TOKEN_URL)
            .header("Metadata-Flavor", "Google")
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|error| {
                CoreError::ProviderUnavailable(format!(
                    "no Google credentials: the metadata server is unreachable ({error}); \
                     store a service-account key or an access token with /login vertex"
                ))
            })?;
        let body = response.text().await.unwrap_or_default();
        parse_token_response(&body, now_unix())
    }
}

/// Decide where the access token comes from.
fn resolve_credential(credential: &str) -> Credential {
    let credential = credential.trim();
    if !credential.is_empty() {
        if let Some(account) = service_account_from_json(credential) {
            return account;
        }
        return Credential::AccessToken(credential.to_string());
    }
    if let Some(path) = env_var("GOOGLE_APPLICATION_CREDENTIALS") {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(account) = service_account_from_json(&contents) {
                return account;
            }
            tracing::warn!(
                path,
                "GOOGLE_APPLICATION_CREDENTIALS is not a service-account key"
            );
        }
    }
    if let Some(token) = env_var("GOOGLE_ACCESS_TOKEN").or_else(|| env_var("GCLOUD_ACCESS_TOKEN")) {
        return Credential::AccessToken(token);
    }
    Credential::Metadata
}

/// Read a service-account key out of JSON text.
fn service_account_from_json(raw: &str) -> Option<Credential> {
    let value: Value = serde_json::from_str(raw).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("service_account") {
        return None;
    }
    Some(Credential::ServiceAccount {
        client_email: value.get("client_email")?.as_str()?.to_string(),
        private_key_pem: value.get("private_key")?.as_str()?.to_string(),
        project_id: value
            .get("project_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Sign the assertion Google exchanges for an access token.
pub fn sign_service_account_jwt(
    client_email: &str,
    private_key_pem: &str,
    now: u64,
) -> Result<String, CoreError> {
    let header = base64url(
        &serde_json::to_vec(&json!({ "alg": "RS256", "typ": "JWT" }))
            .map_err(|error| CoreError::Internal(format!("vertex jwt header: {error}")))?,
    );
    let claims = base64url(
        &serde_json::to_vec(&json!({
            "iss": client_email,
            "scope": CLOUD_PLATFORM_SCOPE,
            "aud": TOKEN_URL,
            "iat": now,
            "exp": now + 3600,
        }))
        .map_err(|error| CoreError::Internal(format!("vertex jwt claims: {error}")))?,
    );
    let signing_input = format!("{header}.{claims}");

    let der = pem_to_der(private_key_pem)
        .ok_or_else(|| CoreError::InvalidRequest("service-account key is not PEM".to_string()))?;
    let key = RsaKeyPair::from_pkcs8(&der).map_err(|error| {
        CoreError::InvalidRequest(format!(
            "service-account key is not a PKCS#8 RSA key: {error}"
        ))
    })?;
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_unspecified| {
        CoreError::Internal("could not sign the service-account JWT".to_string())
    })?;
    Ok(format!("{signing_input}.{}", base64url(&signature)))
}

/// Strip the PEM armour and decode the body.
fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<Vec<&str>>()
        .join("");
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .ok()
}

/// Base64url without padding, as JWT requires.
fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Read `{access_token, expires_in}` out of a token response.
fn parse_token_response(body: &str, now: u64) -> Result<CachedToken, CoreError> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        CoreError::ProviderUnavailable(format!("vertex token response was not JSON: {error}"))
    })?;
    let token = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            CoreError::ProviderUnavailable("vertex token response carried no token".to_string())
        })?;
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    Ok(CachedToken {
        token: token.to_string(),
        expires_at: now + expires_in,
    })
}

/// Pull the project and region out of a full Vertex endpoint URL.
pub fn parse_vertex_base_url(base_url: &str) -> (Option<String>, Option<String>) {
    let base_url = base_url.trim();
    if base_url.is_empty() {
        return (None, None);
    }
    let mut project = None;
    let mut region = None;
    let segments: Vec<&str> = base_url.split('/').collect();
    for (index, segment) in segments.iter().enumerate() {
        match *segment {
            "projects" => project = segments.get(index + 1).map(|value| value.to_string()),
            "locations" => region = segments.get(index + 1).map(|value| value.to_string()),
            _ => {}
        }
    }
    if region.is_none() {
        // `https://us-east5-aiplatform.googleapis.com` names the region in the host.
        if let Some(host) = base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
        {
            if let Some(prefix) = host.strip_suffix("-aiplatform.googleapis.com") {
                region = Some(prefix.to_string());
            }
        }
    }
    (project, region)
}

/// Whether a model id is served by the Anthropic surface on Vertex.
pub fn is_anthropic_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("claude") || model.contains("anthropic")
}

/// The Vertex form of an Anthropic body: no `model`, a Vertex `anthropic_version`.
pub fn vertex_anthropic_body(model: &str, request: &ModelRequest) -> Value {
    let mut body = build_anthropic_body(model, request);
    if let Some(object) = body.as_object_mut() {
        object.remove("model");
        object.insert(
            "anthropic_version".to_string(),
            Value::String(VERTEX_ANTHROPIC_VERSION.to_string()),
        );
    }
    body
}

/// The `:streamRawPredict` URL for an Anthropic model.
pub fn raw_predict_url(host: &str, project: &str, region: &str, model: &str) -> String {
    format!(
        "{host}/v1/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:streamRawPredict"
    )
}

/// The OpenAI-compatible base URL for everything else.
pub fn openapi_base_url(host: &str, project: &str, region: &str) -> String {
    format!("{host}/v1/projects/{project}/locations/{region}/endpoints/openapi")
}

/// A non-empty environment variable.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Unix seconds now.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

#[async_trait]
impl Model for VertexProvider {
    fn name(&self) -> &str {
        &self.model
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> CoreResult<ModelStream> {
        let model = if request.model.is_empty() {
            &self.model
        } else {
            &request.model
        };
        let Some(project) = self.project.as_deref() else {
            return Err(CoreError::InvalidRequest(
                "Vertex needs a project: set VERTEX_PROJECT or GOOGLE_CLOUD_PROJECT, or store a \
                 service-account key with /login vertex"
                    .to_string(),
            ));
        };
        let token = self.access_token().await?;
        let host = self.host();

        if !is_anthropic_model(model) {
            // Gemini and the open models are served by the OpenAI-compatible surface, which
            // the shared transport already speaks.
            return OpenAiCompatibleProvider::new(
                openapi_base_url(&host, project, &self.region),
                token,
                model,
            )
            .with_http_client(reqwest::Client::clone(&self.http))
            .stream(request, cancel)
            .await;
        }

        let body = vertex_anthropic_body(model, &request);
        let url = raw_predict_url(&host, project, &self.region, model);
        let http_request = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);

        let response = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(CoreError::ProviderUnavailable(
                    "provider request cancelled".to_string(),
                ));
            }
            result = http_request.send() => result,
        };
        let response = response.map_err(|error| {
            CoreError::ProviderUnavailable(format!("vertex request failed: {error}"))
        })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let detail = silver_core::redact::redact(body.trim());
            return Err(match status.as_u16() {
                429 => CoreError::ProviderRateLimited(format!("vertex throttled: {detail}")),
                500..=599 => CoreError::ProviderTransient {
                    message: format!("vertex returned HTTP {}: {detail}", status.as_u16()),
                    retry_after_ms: None,
                    rate_limited: false,
                },
                other => CoreError::ProviderUnavailable(format!(
                    "vertex returned HTTP {other}: {detail}"
                )),
            });
        }

        let bytes = futures::StreamExt::map(response.bytes_stream(), |chunk| {
            chunk.map(|chunk| chunk.to_vec()).map_err(|error| {
                CoreError::ProviderUnavailable(format!("vertex stream failed: {error}"))
            })
        });
        Ok(Box::pin(anthropic_event_stream(bytes, cancel)))
    }
}
