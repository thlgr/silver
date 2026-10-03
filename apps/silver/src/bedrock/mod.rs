//! AWS Bedrock transport for Anthropic models: the Anthropic body without `model` (it is in the
//! URL) and with `anthropic_version: "bedrock-2023-05-31"`. The credential is `AKIA…:secret`
//! (SigV4), any other value a bearer API key, or empty to read the standard `AWS_*` variables.

pub mod eventstream;
pub mod sigv4;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt;
use silver_core::error::{CoreError, CoreResult};
use silver_core::model::{Model, ModelRequest, ModelStream};
use tokio_util::sync::CancellationToken;

use crate::anthropic::{anthropic_event_stream, build_anthropic_body};
use eventstream::EventStreamDecoder;
use sigv4::AwsCredentials;

/// The `anthropic_version` Bedrock requires in the body.
const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// Region used when neither the configuration nor the environment names one.
const DEFAULT_REGION: &str = "us-east-1";

/// How a request authenticates to Bedrock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BedrockAuth {
    /// SigV4 with an access key pair.
    Signed(AwsCredentials),
    /// A Bedrock API key, sent as a bearer token.
    Bearer(String),
    /// Nothing usable was configured.
    Missing,
}

/// Streaming client for Bedrock's Anthropic models.
pub struct BedrockProvider {
    region: String,
    model: String,
    auth: BedrockAuth,
    base_url: Option<String>,
    http: reqwest::Client,
}

impl BedrockProvider {
    /// Build a transport from the stored credential. An explicit `base_url` (VPC endpoint, gateway)
    /// wins over the region's public host; the region comes from it, else `AWS_REGION` /
    /// `AWS_DEFAULT_REGION`, else [DEFAULT_REGION].
    pub fn new(
        base_url: impl Into<String>,
        credential: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into();
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        let base_url = (!base_url.is_empty()).then_some(base_url);
        let region = base_url
            .as_deref()
            .and_then(region_from_host)
            .or_else(|| env_var("AWS_REGION"))
            .or_else(|| env_var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|| DEFAULT_REGION.to_string());
        Self {
            region,
            model: model.into(),
            auth: resolve_auth(&credential.into()),
            base_url,
            http: reqwest::Client::new(),
        }
    }

    /// Replace the HTTP client, e.g. to apply the daemon TLS policy.
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// How this transport authenticates, for diagnostics.
    pub fn auth(&self) -> &BedrockAuth {
        &self.auth
    }

    /// The host requests are sent to.
    fn host(&self) -> String {
        match &self.base_url {
            Some(base_url) => base_url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_string(),
            None => format!("bedrock-runtime.{}.amazonaws.com", self.region),
        }
    }
}

/// Split the stored credential into an authentication mode.
fn resolve_auth(credential: &str) -> BedrockAuth {
    let credential = credential.trim();
    if !credential.is_empty() {
        // `access-key:secret[:session-token]`: the only shape that can be signed.
        let parts: Vec<&str> = credential.split(':').collect();
        if parts.len() >= 2 && parts[0].len() >= 16 && parts[0].starts_with('A') {
            return BedrockAuth::Signed(AwsCredentials {
                access_key_id: parts[0].to_string(),
                secret_access_key: parts[1].to_string(),
                session_token: parts.get(2).map(|token| (*token).to_string()),
            });
        }
        return BedrockAuth::Bearer(credential.to_string());
    }
    if let Some(bearer) = env_var("AWS_BEARER_TOKEN_BEDROCK") {
        return BedrockAuth::Bearer(bearer);
    }
    match (
        env_var("AWS_ACCESS_KEY_ID"),
        env_var("AWS_SECRET_ACCESS_KEY"),
    ) {
        (Some(access_key_id), Some(secret_access_key)) => BedrockAuth::Signed(AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token: env_var("AWS_SESSION_TOKEN"),
        }),
        _ => BedrockAuth::Missing,
    }
}

/// A non-empty environment variable.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The region encoded in a `bedrock-runtime.<region>.amazonaws.com` host.
fn region_from_host(base_url: &str) -> Option<String> {
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let mut parts = host.split('.');
    let first = parts.next()?;
    if !first.starts_with("bedrock") {
        return None;
    }
    parts
        .next()
        .map(str::to_string)
        .filter(|region| !region.is_empty())
}

/// The Bedrock form of an Anthropic request body: no `model`, an `anthropic_version`.
pub fn bedrock_body(model: &str, request: &ModelRequest) -> serde_json::Value {
    let mut body = build_anthropic_body(model, request);
    if let Some(object) = body.as_object_mut() {
        object.remove("model");
        // Bedrock rejects `stream` in the body: the streaming operation is the URL.
        object.remove("stream");
        object.insert(
            "anthropic_version".to_string(),
            serde_json::Value::String(BEDROCK_ANTHROPIC_VERSION.to_string()),
        );
    }
    body
}

/// The path a streaming invocation posts to.
pub fn invoke_path(model: &str) -> String {
    // The model id travels in the path and may contain characters that need escaping
    // (`anthropic.claude-sonnet-4-5-20250929-v1:0`, inference profile ARNs).
    format!("/model/{}/invoke-with-response-stream", encode_path(model))
}

/// Percent-encode a path segment, leaving the unreserved set alone.
fn encode_path(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[async_trait]
impl Model for BedrockProvider {
    fn name(&self) -> &str {
        &self.model
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> CoreResult<ModelStream> {
        let model = if request.model.is_empty() {
            self.model.as_str()
        } else {
            request.model.as_str()
        };
        let body = serde_json::to_vec(&bedrock_body(model, &request))
            .map_err(|error| CoreError::Internal(format!("bedrock request body: {error}")))?;
        let path = invoke_path(model);
        let host = self.host();
        let url = format!("https://{host}{path}");

        let mut http_request = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/vnd.amazon.eventstream",
            );
        match &self.auth {
            BedrockAuth::Bearer(token) => {
                http_request = http_request.bearer_auth(token);
            }
            BedrockAuth::Signed(credentials) => {
                let now = Utc::now();
                let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
                let date = now.format("%Y%m%d").to_string();
                let signed = sigv4::sign(
                    credentials,
                    &self.region,
                    "bedrock",
                    "POST",
                    &host,
                    &path,
                    "",
                    &body,
                    &timestamp,
                    &date,
                    &[("content-type", "application/json")],
                );
                for header in signed.headers {
                    http_request = http_request.header(header.name, header.value);
                }
            }
            BedrockAuth::Missing => {
                return Err(CoreError::ProviderUnavailable(
                    "Bedrock needs credentials: store 'access-key:secret' with /login bedrock, \
                     or export AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY"
                        .to_string(),
                ));
            }
        }

        let response = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(CoreError::ProviderUnavailable(
                    "provider request cancelled".to_string(),
                ));
            }
            result = http_request.body(body).send() => result,
        };
        let response = response.map_err(|error| {
            CoreError::ProviderUnavailable(format!("bedrock request failed: {error}"))
        })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(map_bedrock_error(status.as_u16(), &body));
        }

        // Unwrap the binary framing into the SSE text the Anthropic reader consumes, so both
        // transports share one event implementation.
        let decoder = Arc::new(std::sync::Mutex::new(EventStreamDecoder::new()));
        let bytes = response.bytes_stream().map(move |chunk| {
            let chunk = chunk.map_err(|error| {
                CoreError::ProviderUnavailable(format!("bedrock stream failed: {error}"))
            })?;
            let frames = {
                let mut decoder = decoder.lock().expect("event stream decoder");
                decoder
                    .push(&chunk)
                    .map_err(CoreError::ProviderUnavailable)?
            };
            let mut out = Vec::new();
            for frame in frames {
                if let Some(exception) = frame.exception_type.as_deref() {
                    let detail = frame.event_json().unwrap_or_default();
                    return Err(map_exception(exception, &detail));
                }
                if let Some(json) = frame.event_json() {
                    out.extend_from_slice(b"data: ");
                    out.extend_from_slice(json.as_bytes());
                    out.extend_from_slice(b"\n\n");
                }
            }
            Ok(out)
        });
        Ok(Box::pin(anthropic_event_stream(bytes, cancel)))
    }
}

/// Map a Bedrock HTTP failure onto the shared provider errors.
fn map_bedrock_error(status: u16, body: &str) -> CoreError {
    let detail = silver_core::redact::redact(body.trim());
    let detail = if detail.len() > 500 {
        format!("{}…", &detail[..500])
    } else {
        detail
    };
    match status {
        400 | 403 => CoreError::ProviderUnavailable(format!(
            "bedrock rejected the request (HTTP {status}): {detail}"
        )),
        429 => CoreError::ProviderRateLimited(format!("bedrock throttled the request: {detail}")),
        500..=599 => CoreError::ProviderTransient {
            message: format!("bedrock returned HTTP {status}: {detail}"),
            retry_after_ms: None,
            rate_limited: false,
        },
        _ => CoreError::ProviderUnavailable(format!("bedrock returned HTTP {status}: {detail}")),
    }
}

/// Map an in-stream exception frame onto the shared provider errors.
fn map_exception(exception: &str, detail: &str) -> CoreError {
    let detail = silver_core::redact::redact(detail);
    if exception.to_ascii_lowercase().contains("throttl") {
        return CoreError::ProviderRateLimited(format!("bedrock throttled the stream: {detail}"));
    }
    if exception.to_ascii_lowercase().contains("modelstream") {
        return CoreError::ProviderTransient {
            message: format!("bedrock stream failed ({exception}): {detail}"),
            retry_after_ms: None,
            rate_limited: false,
        };
    }
    CoreError::ProviderUnavailable(format!("bedrock stream error ({exception}): {detail}"))
}
