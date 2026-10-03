//! Secret redaction for everything a model or log can see: vendor token prefixes, auth headers,
//! PEM keys, connection strings and URL userinfo, config keys and KEY=value lines. Over-redaction
//! is preferred: a false positive costs one value, a false negative leaks a live credential.

/// Replacement written in place of a detected secret.
pub const REDACTED: &str = "[redacted]";

/// Placeholder for a redacted PEM private-key block.
const REDACTED_PRIVATE_KEY: &str = "[redacted private key]";

/// Token prefixes with the shortest believable body: any length would redact identifiers like
/// `am_i` or `fc-x`, yet fixtures such as `sk-abc123` must still match.
const SECRET_PREFIXES: &[(&str, usize)] = &[
    // OpenAI / Anthropic / OpenRouter / Stripe / generic
    ("sk-ant-", 4),
    ("sk-or-", 4),
    ("sk_live_", 4),
    ("sk_test_", 4),
    ("rk_live_", 4),
    ("sk-", 4),
    // GitHub
    ("github_pat_", 4),
    ("ghp_", 4),
    ("gho_", 4),
    ("ghu_", 4),
    ("ghs_", 4),
    ("ghr_", 4),
    // xAI
    ("xai-", 4),
    // Slack
    ("xoxb-", 8),
    ("xoxa-", 8),
    ("xoxp-", 8),
    ("xoxr-", 8),
    ("xoxs-", 8),
    ("xapp-", 8),
    // Google
    ("AIza", 10),
    ("ya29.", 8),
    // AWS
    ("AKIA", 16),
    // JWTs
    ("eyJ", 8),
    // GitLab token families
    ("glpat-", 8),
    ("gloas-", 8),
    ("gldt-", 8),
    ("glrt-", 8),
    ("glrtr-", 8),
    ("glcbt-", 8),
    ("glptt-", 8),
    ("glft-", 8),
    ("glimt-", 8),
    ("glagent-", 8),
    ("glsoat-", 8),
    ("glffct-", 8),
    ("glwt-", 8),
    ("GR1348941", 8),
    // Package registries, model hubs and vendor services
    ("npm_", 8),
    ("pypi-", 8),
    ("hf_", 8),
    ("r8_", 8),
    ("pplx-", 8),
    ("fal_", 8),
    ("fc-", 8),
    ("bb_live_", 8),
    ("gAAAA", 8),
    ("SG.", 8),
    ("dop_v1_", 8),
    ("doo_v1_", 8),
    ("am_", 8),
    ("tvly-", 8),
    ("exa_", 8),
    ("gsk_", 8),
    ("syt_", 8),
    ("retaindb_", 8),
    ("hsk-", 8),
    ("mem0_", 8),
    ("brv_", 8),
    ("ntn_", 8),
    ("fw-", 8),
    ("fw_", 8),
    ("fpk_", 8),
    ("pk-lf-", 8),
];

/// Key names that always denote a secret (case-insensitive).
const SECRET_KEY_NAMES: &[&str] = &[
    "api_key",
    "api-key",
    "apikey",
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
    "access_key",
    "private_key",
    "access_token",
    "auth_token",
    "bearer_token",
    "client_secret",
    "secret_key",
    "api_token",
    "auth_key",
    "refresh_token",
    "session_token",
    "credential",
    "credentials",
    "aws_access_key_id",
    "aws_secret_access_key",
];

/// Suffixes that force redaction even for short, human-readable values
/// (for example OPENAI_API_KEY, client_secret, db_password).
const SECRET_KEY_SUFFIXES: &[&str] = &[
    "_api_key",
    "_access_key",
    "_secret_key",
    "_private_key",
    "_client_secret",
    "_token",
    "_secret",
    "_password",
    "_passwd",
    "_credential",
];

/// The one weak suffix: a bare _key (sort_key) is only redacted when its value itself
/// looks credential-shaped.
const WEAK_KEY_SUFFIX: &str = "_key";

/// Characters that may wrap a key or a value inside structured text.
const QUOTES: [char; 2] = ['"', '\''];

/// Redact secret-shaped tokens, assignments and blocks from a string.
pub fn redact(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let text = redact_pem_blocks(text);
    let text = redact_auth_headers(&text);
    let text = redact_assignments(&text);
    let text = redact_quoted_pairs(&text);
    let text = redact_url_credentials(&text);
    let text = redact_telegram_tokens(&text);
    redact_tokens(&text)
}

/// Redaction for file reads: the unambiguous shapes only. The assignment, YAML and quoted-field
/// passes are skipped, since they would rewrite source such as `api_key: String`.
pub fn redact_source(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let text = redact_pem_blocks(text);
    let text = redact_auth_headers(&text);
    let text = redact_url_credentials(&text);
    let text = redact_telegram_tokens(&text);
    redact_tokens(&text)
}

// ---------------------------------------------------------------------------
// PEM private keys
// ---------------------------------------------------------------------------

/// Replace every -----BEGIN ... PRIVATE KEY----- block with a fixed placeholder.
fn redact_pem_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(begin) = rest.find("-----BEGIN") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..begin]);
        let candidate = &rest[begin..];
        let header_end = candidate.find('\n').unwrap_or(candidate.len());
        if !candidate[..header_end].contains("PRIVATE KEY") {
            // A certificate or some other PEM block, not a private key.
            out.push_str("-----BEGIN");
            rest = &candidate["-----BEGIN".len()..];
            continue;
        }
        let Some(end) = candidate.find("-----END") else {
            out.push_str(candidate);
            break;
        };
        let end_line_end = candidate[end..]
            .find('\n')
            .map(|offset| end + offset + 1)
            .unwrap_or(candidate.len());
        out.push_str(REDACTED_PRIVATE_KEY);
        rest = &candidate[end_line_end..];
    }
    out
}

// ---------------------------------------------------------------------------
// KEY=value / YAML assignments
// ---------------------------------------------------------------------------

/// Redact KEY=value and key: value lines whose key names a secret.
fn redact_assignments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        match redact_assignment_line(line) {
            Some(replaced) => out.push_str(&replaced),
            None => out.push_str(line),
        }
    }
    out
}

fn redact_assignment_line(line: &str) -> Option<String> {
    if line.trim_start().starts_with('#') {
        return None;
    }
    redact_equals_assignment(line).or_else(|| redact_yaml_assignment(line))
}

fn redact_equals_assignment(line: &str) -> Option<String> {
    let (key_part, value_part) = line.split_once('=')?;
    let raw_key = key_part.trim();
    let key = raw_key.strip_prefix("export ").unwrap_or(raw_key).trim();
    if key.is_empty() || key.chars().any(|ch| ch.is_whitespace() || ch == ':') {
        return None;
    }
    if !is_secret_key(key) || !should_redact_value(key, value_part.trim()) {
        return None;
    }
    Some(format!("{key_part}={REDACTED}"))
}

fn redact_yaml_assignment(line: &str) -> Option<String> {
    let colon = line.find(':')?;
    let after = &line[colon + 1..];
    if after.starts_with("//") {
        // A URL scheme, not a YAML key.
        return None;
    }
    let key = line[..colon].trim();
    if key.is_empty() || key.chars().any(char::is_whitespace) {
        return None;
    }
    if !is_secret_key(key) {
        return None;
    }
    let value = after.trim_start();
    if value.is_empty() || !should_redact_value(key, value) {
        return None;
    }
    let value_start = line.len() - value.len();
    Some(format!("{}{REDACTED}", &line[..value_start]))
}

/// Whether a value should be masked for a secret-named key.
fn should_redact_value(key: &str, value: &str) -> bool {
    let value = value.trim().trim_matches(|ch| QUOTES.contains(&ch));
    if value.is_empty() || value.contains(REDACTED) {
        return false;
    }
    // Authorization: Bearer ... is handled by the dedicated header pass so the scheme word
    // survives; do not swallow it here.
    let lower = value.to_ascii_lowercase();
    if lower == "bearer"
        || lower.starts_with("bearer ")
        || lower == "basic"
        || lower.starts_with("basic ")
    {
        return false;
    }
    if value.starts_with('$') || value.starts_with('{') || value.starts_with('%') {
        // A variable or template reference, not a literal credential.
        return false;
    }
    if is_strong_key(key) {
        return true;
    }
    if looks_like_secret(value) || looks_opaque(value) {
        return true;
    }
    // A value that is not a plain alphabetic identifier is credential-shaped.
    !value.chars().all(|ch| ch.is_ascii_alphabetic())
}

// ---------------------------------------------------------------------------
// JSON / Python repr quoted fields
// ---------------------------------------------------------------------------

/// Redact the value of a quoted secret key.
fn redact_quoted_pairs(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < bytes.len() {
        let quote = bytes[index];
        if quote == b'"' || quote == b'\'' {
            if let Some((end, replacement)) = quoted_pair_at(text, index, quote) {
                out.push_str(&replacement);
                index = end;
                continue;
            }
            let key_end = quoted_end(text, index, quote).unwrap_or(index + 1);
            out.push_str(&text[index..key_end]);
            index = key_end;
            continue;
        }
        let ch = text[index..]
            .chars()
            .next()
            .expect("index sits on a char boundary");
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// If a quoted string starts at start and is a secret key, return the end index (one past
/// the value) and the fully substituted pair.
fn quoted_pair_at(text: &str, start: usize, quote: u8) -> Option<(usize, String)> {
    let key_end = quoted_end(text, start, quote)?;
    let key = &text[start + 1..key_end - 1];
    if !is_secret_key(key) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut cursor = skip_whitespace(bytes, key_end);
    if cursor >= bytes.len() || !matches!(bytes[cursor], b':' | b'=') {
        return None;
    }
    cursor = skip_whitespace(bytes, cursor + 1);
    let separator = &text[key_end..cursor];
    let quoted_key = &text[start..key_end];
    if cursor < bytes.len() && matches!(bytes[cursor], b'"' | b'\'') {
        let value_quote = bytes[cursor];
        let value_end = quoted_end(text, cursor, value_quote)?;
        if text[cursor..value_end].contains(REDACTED) {
            return None;
        }
        return Some((
            value_end,
            format!(
                "{quoted_key}{separator}{}{REDACTED}{}",
                value_quote as char, value_quote as char
            ),
        ));
    }
    let value_start = cursor;
    while cursor < bytes.len()
        && !bytes[cursor].is_ascii_whitespace()
        && !matches!(bytes[cursor], b',' | b'}' | b']' | b';' | b')')
    {
        cursor += 1;
    }
    if cursor == value_start || text[value_start..cursor].contains(REDACTED) {
        return None;
    }
    Some((cursor, format!("{quoted_key}{separator}{REDACTED}")))
}

fn skip_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

/// Index one past the closing quote, honouring backslash escapes.
fn quoted_end(text: &str, start: usize, quote: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            byte if byte == quote => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Authorization / API-key headers
// ---------------------------------------------------------------------------

/// Redact the value after Bearer, x-api-key: and x-goog-api-key:, keeping the scheme and
/// header name visible.
fn redact_auth_headers(text: &str) -> String {
    const SCHEMES: [&str; 3] = ["bearer", "x-api-key", "x-goog-api-key"];
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let mut best: Option<(usize, usize, usize)> = None;
        for scheme in SCHEMES {
            let mut search = cursor;
            while let Some(offset) = lower[search..].find(scheme) {
                let start = search + offset;
                let boundary = start == 0 || !is_token_byte(bytes[start - 1]);
                if boundary {
                    if let Some((value_start, value_end)) = auth_header_value(text, start, scheme) {
                        if best.is_none_or(|(best_start, _, _)| start < best_start) {
                            best = Some((start, value_start, value_end));
                        }
                        break;
                    }
                }
                search = start + 1;
            }
        }
        match best {
            Some((_, value_start, value_end)) => {
                out.push_str(&text[cursor..value_start]);
                out.push_str(REDACTED);
                cursor = value_end;
            }
            None => {
                out.push_str(&text[cursor..]);
                break;
            }
        }
    }
    out
}

/// The value span of a header scheme starting at start; returns (value_start, value_end).
fn auth_header_value(text: &str, start: usize, scheme: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut cursor = start + scheme.len();
    if scheme == "bearer" {
        if cursor >= bytes.len() || !bytes[cursor].is_ascii_whitespace() {
            return None;
        }
    } else {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b':' {
            return None;
        }
        cursor += 1;
    }
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    let value_start = cursor;
    while cursor < bytes.len() && !is_header_value_delimiter(bytes[cursor]) {
        cursor += 1;
    }
    (cursor > value_start).then_some((value_start, cursor))
}

fn is_header_value_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'"' | b'\'' | b',' | b'}' | b')' | b']' | b';' | b'<' | b'>'
        )
}

// ---------------------------------------------------------------------------
// URL userinfo and database connection strings
// ---------------------------------------------------------------------------

/// Redact the password (or a bare userinfo token) in scheme://user:pass@host.
fn redact_url_credentials(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b':' && bytes[index..].starts_with(b"://") {
            let authority_start = index + 3;
            let mut authority_end = authority_start;
            while authority_end < bytes.len() && !is_authority_end(bytes[authority_end]) {
                authority_end += 1;
            }
            if let Some(at_offset) = text[authority_start..authority_end].rfind('@') {
                let at = authority_start + at_offset;
                let userinfo = &text[authority_start..at];
                if !userinfo.is_empty() && !userinfo.contains('{') && !userinfo.contains('$') {
                    out.push_str("://");
                    match userinfo.find(':') {
                        Some(colon) => {
                            out.push_str(&userinfo[..=colon]);
                            out.push_str(REDACTED);
                        }
                        None if userinfo.len() >= 8 => out.push_str(REDACTED),
                        None => out.push_str(userinfo),
                    }
                    index = at;
                    continue;
                }
            }
        }
        let ch = text[index..]
            .chars()
            .next()
            .expect("index sits on a char boundary");
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn is_authority_end(byte: u8) -> bool {
    matches!(
        byte,
        b'/' | b'?'
            | b'#'
            | b' '
            | b'\t'
            | b'\n'
            | b'\r'
            | b'"'
            | b'\''
            | b')'
            | b']'
            | b'<'
            | b'>'
    )
}

// ---------------------------------------------------------------------------
// Telegram bot tokens
// ---------------------------------------------------------------------------

/// Redact [bot]digits:token Telegram bot tokens.
fn redact_telegram_tokens(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b':' {
            let mut digit_start = index;
            while digit_start > 0 && bytes[digit_start - 1].is_ascii_digit() {
                digit_start -= 1;
            }
            let digits = index - digit_start;
            let mut body_end = index + 1;
            while body_end < bytes.len() && is_telegram_body_byte(bytes[body_end]) {
                body_end += 1;
            }
            let body = body_end - (index + 1);
            if digits >= 8 && body >= 30 {
                let mut start = digit_start;
                if text[..digit_start].ends_with("bot") {
                    start = digit_start - 3;
                }
                if out.len() >= index - start {
                    out.truncate(out.len() - (index - start));
                }
                out.push_str(REDACTED);
                index = body_end;
                continue;
            }
        }
        let ch = text[index..]
            .chars()
            .next()
            .expect("index sits on a char boundary");
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn is_telegram_body_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

// ---------------------------------------------------------------------------
// Vendor token prefixes
// ---------------------------------------------------------------------------

/// Replace every token that starts with a known vendor prefix.
fn redact_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    for ch in text.chars() {
        if is_token_char(ch) {
            token.push(ch);
        } else {
            flush_token(&mut token, &mut out);
            out.push(ch);
        }
    }
    flush_token(&mut token, &mut out);
    out
}

fn flush_token(token: &mut String, out: &mut String) {
    if token.is_empty() {
        return;
    }
    if looks_like_secret(token) {
        out.push_str(REDACTED);
    } else {
        out.push_str(token);
    }
    token.clear();
}

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

fn looks_like_secret(token: &str) -> bool {
    SECRET_PREFIXES.iter().any(|(prefix, min_body)| {
        token
            .strip_prefix(prefix)
            .is_some_and(|body| body.len() >= *min_body)
    })
}

// ---------------------------------------------------------------------------
// Key names and value shapes
// ---------------------------------------------------------------------------

/// Whether a key name denotes a secret.
fn is_secret_key(key: &str) -> bool {
    let key = key.trim().trim_matches(|ch| QUOTES.contains(&ch)).trim();
    if key.is_empty()
        || key
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')))
    {
        return false;
    }
    let lower = key.to_ascii_lowercase();
    // Dotted config keys: any segment may name the secret (spring.datasource.password).
    if lower
        .split('.')
        .any(|segment| SECRET_KEY_NAMES.contains(&segment))
    {
        return true;
    }
    let compact = lower.replace(['-', '.'], "_");
    if SECRET_KEY_NAMES.contains(&compact.as_str()) {
        return true;
    }
    if SECRET_KEY_SUFFIXES
        .iter()
        .any(|suffix| compact.ends_with(suffix))
    {
        return true;
    }
    compact.ends_with(WEAK_KEY_SUFFIX)
}

/// Whether a secret-named key forces redaction regardless of how its value looks.
fn is_strong_key(key: &str) -> bool {
    let compact = key
        .trim()
        .trim_matches(|ch| QUOTES.contains(&ch))
        .trim()
        .to_ascii_lowercase()
        .replace(['-', '.'], "_");
    if compact.is_empty() {
        return false;
    }
    SECRET_KEY_NAMES.contains(&compact.as_str())
        || SECRET_KEY_SUFFIXES
            .iter()
            .any(|suffix| compact.ends_with(suffix))
}

/// Whether a value looks like an opaque credential.
fn looks_opaque(value: &str) -> bool {
    if value == REDACTED || value == "***" {
        return true;
    }
    let len = value.len();
    if len >= 16 && value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return true;
    }
    if len >= 20
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '/' | '+' | '=' | '-'))
    {
        return true;
    }
    if len < 12 {
        return false;
    }
    let classes = [
        value.chars().any(|ch| ch.is_ascii_lowercase()),
        value.chars().any(|ch| ch.is_ascii_uppercase()),
        value.chars().any(|ch| ch.is_ascii_digit()),
    ];
    classes.iter().filter(|present| **present).count() >= 2
}
