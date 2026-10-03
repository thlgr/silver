//! AWS Signature Version 4 for a Bedrock streaming POST, checked against the published test vector.

use ring::hmac;
use sha2::{Digest, Sha256};

/// The signing algorithm identifier that heads the Authorization header.
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// Long-lived credentials for one AWS account, plus the optional session token that
/// temporary credentials (STS, IAM roles, SSO) carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsCredentials {
    /// Access key id (`AKIA…`, `ASIA…`).
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Session token for temporary credentials.
    pub session_token: Option<String>,
}

/// One header to sign, lower-cased name and trimmed value.
#[derive(Clone, Debug)]
pub struct SignedHeader {
    /// Header name, lower case.
    pub name: String,
    /// Header value.
    pub value: String,
}

/// The headers a signed request must carry, in the order AWS wants them signed.
pub struct SignedRequest {
    /// Headers to add to the outgoing request, including `authorization`.
    pub headers: Vec<SignedHeader>,
}

/// Sign a request and return the headers it must carry. `timestamp` (`YYYYMMDDTHHMMSSZ`) and its
/// `date` prefix are passed in so a test can pin them.
#[expect(
    clippy::too_many_arguments,
    reason = "AWS SigV4 signing needs all request components"
)]
pub fn sign(
    credentials: &AwsCredentials,
    region: &str,
    service: &str,
    method: &str,
    host: &str,
    uri_path: &str,
    query: &str,
    payload: &[u8],
    timestamp: &str,
    date: &str,
    extra_headers: &[(&str, &str)],
) -> SignedRequest {
    let payload_hash = hex_sha256(payload);

    // Canonical headers are sorted by lower-cased name; host and x-amz-date are always signed,
    // and a session token is signed too or AWS rejects the temporary credential.
    let mut headers: Vec<(String, String)> = vec![
        ("host".to_string(), host.to_string()),
        ("x-amz-date".to_string(), timestamp.to_string()),
    ];
    for (name, value) in extra_headers {
        headers.push((name.to_ascii_lowercase(), (*value).to_string()));
    }
    if let Some(token) = credentials.session_token.as_deref() {
        headers.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    headers.sort_by(|left, right| left.0.cmp(&right.0));

    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let signed_headers: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
    let signed_headers = signed_headers.join(";");

    let canonical_request = format!(
        "{method}\n{uri_path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "{ALGORITHM}\n{timestamp}\n{scope}\n{}",
        hex_sha256(canonical_request.as_bytes())
    );

    let signing_key = signing_key(&credentials.secret_access_key, date, region, service);
    let signature = hex(hmac::sign(&signing_key, string_to_sign.as_bytes()).as_ref());
    let authorization = format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    );

    let mut out: Vec<SignedHeader> = headers
        .into_iter()
        .filter(|(name, _)| name != "host")
        .map(|(name, value)| SignedHeader { name, value })
        .collect();
    out.push(SignedHeader {
        name: "authorization".to_string(),
        value: authorization,
    });
    SignedRequest { headers: out }
}

/// Derive the request's signing key: HMAC down the date, region, service and terminator.
fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> hmac::Key {
    let mut key = hmac::Key::new(hmac::HMAC_SHA256, format!("AWS4{secret}").as_bytes());
    for part in [date, region, service, "aws4_request"] {
        let tag = hmac::sign(&key, part.as_bytes());
        key = hmac::Key::new(hmac::HMAC_SHA256, tag.as_ref());
    }
    key
}

/// Lower-case hex of the SHA-256 digest.
pub fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

/// Lower-case hex of a byte slice.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
