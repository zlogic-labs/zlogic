use zlogic_protocol::error::{ApiError, ErrorCategory, LocalizedMessage, RetryPolicy};
use zlogic_protocol::llm::{LlmError, LlmErrorKind};

pub const MESSAGE_MAX_CHARS: usize = 8000;

pub fn err(kind: LlmErrorKind, message: impl Into<String>) -> LlmError {
    LlmError {
        kind,
        retryable: false,
        message: message.into(),
        status: None,
        request_id: None,
    }
}

pub fn retryable(kind: LlmErrorKind, message: impl Into<String>) -> LlmError {
    LlmError {
        retryable: true,
        ..err(kind, message)
    }
}

pub fn network(message: impl Into<String>) -> LlmError {
    retryable(LlmErrorKind::Network, message)
}

pub fn aborted() -> LlmError {
    err(LlmErrorKind::Aborted, "request aborted")
}

pub fn protocol(message: impl Into<String>) -> LlmError {
    err(LlmErrorKind::Server, message)
}

pub fn provider_text(message: &str) -> &str {
    message
}

pub fn attach_endpoint(mut error: LlmError, endpoint: &crate::Endpoint, path: &str) -> LlmError {
    let url = readable_url(endpoint, path);
    if !error.message.contains("url:") {
        error.message = format!("{} (url: {url})", error.message);
    }
    error
}

fn readable_url(endpoint: &crate::Endpoint, path: &str) -> String {
    let base = endpoint.base_url.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    let path = path.split('?').next().unwrap_or(path);
    format!("{base}/{path}")
}

pub fn to_api_error(error: &LlmError, diagnostic: impl Into<String>) -> ApiError {
    let (code, category, fallback) = match error.kind {
        LlmErrorKind::Auth => (
            "llm_auth_failed",
            ErrorCategory::PermissionDenied,
            "Model provider authentication failed",
        ),
        LlmErrorKind::RateLimit => (
            "llm_rate_limited",
            ErrorCategory::Unavailable,
            "The model provider is rate limiting requests",
        ),
        LlmErrorKind::Quota => (
            "llm_quota_exhausted",
            ErrorCategory::Unavailable,
            "The model provider quota is exhausted",
        ),
        LlmErrorKind::Server => (
            "llm_server_unavailable",
            ErrorCategory::Unavailable,
            "The model provider is temporarily unavailable",
        ),
        LlmErrorKind::Network => (
            "llm_network_failed",
            ErrorCategory::Unavailable,
            "Could not reach the model provider",
        ),
        LlmErrorKind::BadRequest => (
            "llm_request_invalid",
            ErrorCategory::InvalidArgument,
            "The model provider rejected the request",
        ),
        LlmErrorKind::ContentPolicy => (
            "llm_content_blocked",
            ErrorCategory::PermissionDenied,
            "The model provider blocked this content",
        ),
        LlmErrorKind::Aborted => (
            "llm_aborted",
            ErrorCategory::Unavailable,
            "The model request was interrupted",
        ),
        LlmErrorKind::ContextLengthExceeded => (
            "llm_context_too_long",
            ErrorCategory::InvalidArgument,
            "The conversation exceeds the model context window",
        ),
    };
    let retry = if error.retryable {
        RetryPolicy::Immediate
    } else {
        RetryPolicy::Never
    };
    let diagnostic = diagnostic.into();
    let mut api = ApiError::new(
        code,
        category,
        LocalizedMessage::new(format!("error.{code}"), fallback),
    )
    .with_retry(retry)
    .with_diagnostic(diagnostic.clone())
    .with_detail("diagnostic", diagnostic);
    if let Some(status) = error.status {
        api = api.with_detail("status", u64::from(status));
    }
    if let Some(request_id) = &error.request_id {
        api = api.with_detail("request_id", request_id.clone());
    }
    api
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseMeta {
    pub request_id: Option<String>,
}

const REQUEST_ID_HEADERS: &[&str] = &[
    "x-request-id",
    "request-id",
    "anthropic-request-id",
    "x-amzn-requestid",
    "x-amz-request-id",
    "x-goog-request-id",
    "cf-ray",
];

impl ResponseMeta {
    pub fn from_headers<'a, I>(headers: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut request_id: Option<(usize, String)> = None;

        for (name, value) in headers {
            let lower = name.to_ascii_lowercase();
            if let Some(rank) = REQUEST_ID_HEADERS.iter().position(|h| *h == lower)
                && !value.is_empty()
                && request_id.as_ref().is_none_or(|(r, _)| rank < *r)
            {
                request_id = Some((rank, value.to_string()));
            }
        }

        Self {
            request_id: request_id.map(|(_, v)| v),
        }
    }
}

pub fn from_status(status: u16, body: &str, meta: &ResponseMeta) -> LlmError {
    let kind = match status {
        401 | 403 => LlmErrorKind::Auth,
        408 | 429 => LlmErrorKind::RateLimit,
        400 | 404 | 405 | 409 | 413 | 422 => LlmErrorKind::BadRequest,
        s if s >= 500 => LlmErrorKind::Server,
        _ => LlmErrorKind::Server,
    };
    let retryable = matches!(kind, LlmErrorKind::RateLimit | LlmErrorKind::Server);
    let e = LlmError {
        kind,
        retryable,
        message: format!(
            "HTTP {status}: {}",
            truncate(&redact(body), MESSAGE_MAX_CHARS)
        ),
        status: Some(status),
        request_id: meta.request_id.clone(),
    };
    classify_body(e, body)
}

const OVERFLOW_MARKERS: &[&str] = &[
    "context_length_exceeded",
    "context length exceeded",
    "maximum context length",
    "reduce the length of the messages",
    "input is too long",
    "prompt is too long",
    "too many tokens",
    "exceeds the maximum number of tokens",
    "request_too_large",
    "exceeds the context window",
    "tokens in request more than max tokens allowed",
    "maximum prompt length is",
    "exceeds the available context size",
    "greater than the context length",
    "exceeded model token limit",
    "request entity too large",
    "context length is only",
    "model_context_window_exceeded",
    "context window exceeds limit",
    "exceeds the limit of",
];

const QUOTA_MARKERS: &[&str] = &[
    "insufficient_quota",
    "insufficient quota",
    "quota_exceeded",
    "quota exceeded",
    "exceeded your current quota",
    "billing_hard_limit_reached",
    "credit balance is too low",
    "no remaining credit",
    "usage_limit_reached",
    "gousagelimiterror",
];

const POLICY_MARKERS: &[&str] = &[
    "content_policy",
    "content policy",
    "content_filter",
    "responsibleaipolicyviolation",
];

pub fn classify_body(mut e: LlmError, body: &str) -> LlmError {
    let lower = body.to_ascii_lowercase();

    if OVERFLOW_MARKERS.iter().any(|m| lower.contains(m)) {
        e.kind = LlmErrorKind::ContextLengthExceeded;
        e.retryable = false;
        return e;
    }
    if e.kind == LlmErrorKind::RateLimit && QUOTA_MARKERS.iter().any(|m| lower.contains(m)) {
        e.kind = LlmErrorKind::Quota;
        e.retryable = false;
        return e;
    }
    if POLICY_MARKERS.iter().any(|m| lower.contains(m)) {
        e.kind = LlmErrorKind::ContentPolicy;
        e.retryable = false;
    }
    e
}

const SENSITIVE_NAMES: &[&str] = &[
    "authorization",
    "api_key",
    "api-key",
    "apikey",
    "access_token",
    "refresh_token",
    "id_token",
    "token",
    "secret",
    "credential",
    "signature",
    "password",
];

fn redact(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    if !SENSITIVE_NAMES.iter().any(|n| lower.contains(n)) {
        return body.to_string();
    }

    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut i = 0usize;
    'outer: while i < bytes.len() {
        for name in SENSITIVE_NAMES {
            if lower[i..].starts_with(name)
                && let Some((vstart, vend)) = value_span(bytes, i + name.len())
            {
                out.push_str(&body[i..vstart]);
                out.push_str("<redacted>");
                i = vend;
                continue 'outer;
            }
        }
        let ch_len = body[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&body[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn value_span(bytes: &[u8], pos: usize) -> Option<(usize, usize)> {
    let mut i = pos;
    if bytes.get(i) == Some(&b'"') {
        i += 1;
    }
    let mut saw_sep = false;
    while let Some(&b) = bytes.get(i) {
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b':' | b'=' => {
                saw_sep = true;
                i += 1;
            }
            _ => break,
        }
    }
    if !saw_sep {
        return None;
    }
    let quoted = bytes.get(i) == Some(&b'"');
    if quoted {
        i += 1;
    }
    let start = i;
    while let Some(&b) = bytes.get(i) {
        if quoted {
            if b == b'"' {
                break;
            }
        } else if matches!(b, b',' | b'&' | b'}' | b' ' | b'\n' | b'\r' | b'\t') {
            break;
        }
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((start, i))
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16, body: &str) -> LlmError {
        from_status(code, body, &ResponseMeta::default())
    }

    #[test]
    fn rate_limit_is_retryable_auth_is_not() {
        assert!(status(429, "").retryable);
        assert!(status(503, "").retryable);
        assert!(!status(401, "").retryable);
        assert!(!status(400, "").retryable);
    }

    #[test]
    fn context_overflow_gets_its_own_kind_and_is_not_retryable() {
        let e = status(
            400,
            r#"{"error":{"message":"This model's maximum context length is 128000 tokens"}}"#,
        );
        assert_eq!(e.kind, LlmErrorKind::ContextLengthExceeded);
        assert!(
            !e.retryable,
            "resending the same request would overflow again"
        );
    }

    #[test]
    fn anthropic_phrasing_also_detected() {
        let e = status(
            400,
            r#"{"error":{"message":"prompt is too long: 250000 tokens"}}"#,
        );
        assert_eq!(e.kind, LlmErrorKind::ContextLengthExceeded);
    }

    #[test]
    fn newly_covered_overflow_phrasings() {
        for body in [
            "Input is too long for requested model",
            "request entity too large",
            "This endpoint's context length is only 8192 tokens",
            "model_context_window_exceeded",
            "tokens in request more than max tokens allowed",
            "prompt exceeds the available context size",
        ] {
            assert_eq!(
                status(400, body).kind,
                LlmErrorKind::ContextLengthExceeded,
                "{body:?} should be classified as overflow"
            );
        }
    }

    #[test]
    fn quota_exhaustion_is_a_429_that_must_not_retry() {
        let e = status(
            429,
            r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#,
        );
        assert_eq!(e.kind, LlmErrorKind::Quota);
        assert!(!e.retryable);

        let e = status(
            429,
            r#"{"error":{"message":"Rate limit reached for gpt-5"}}"#,
        );
        assert_eq!(e.kind, LlmErrorKind::RateLimit);
        assert!(e.retryable);
    }

    #[test]
    fn usage_limit_errors_are_not_retried() {
        let e = status(
            429,
            r#"{"error":{"code":"usage_limit_reached","message":"Usage limit reached"}}"#,
        );
        assert_eq!(e.kind, LlmErrorKind::Quota, "{e:?}");
        assert!(!e.retryable);

        let e = status(
            429,
            r#"{"type":"error","error":{"type":"GoUsageLimitError","message":"Insufficient Balance"}}"#,
        );
        assert_eq!(e.kind, LlmErrorKind::Quota, "{e:?}");
        assert!(!e.retryable);
    }

    #[test]
    fn quota_wording_on_a_400_is_not_upgraded() {
        let e = status(400, "the user asked about quota exceeded semantics");
        assert_eq!(e.kind, LlmErrorKind::BadRequest);
    }

    #[test]
    fn content_policy_is_classified_and_not_retryable() {
        let e = status(
            400,
            r#"{"error":{"code":"content_filter","message":"blocked"}}"#,
        );
        assert_eq!(e.kind, LlmErrorKind::ContentPolicy);
        assert!(!e.retryable);
    }

    #[test]
    fn retry_after_headers_are_ignored() {
        let m = ResponseMeta::from_headers([("Retry-After", "30"), ("retry-after-ms", "1500")]);
        assert_eq!(m.request_id, None);
        let m = ResponseMeta::from_headers([("Retry-After", "soon")]);
        assert_eq!(m.request_id, None);
    }

    #[test]
    fn request_id_prefers_the_more_specific_header() {
        let m = ResponseMeta::from_headers([("cf-ray", "abc"), ("x-request-id", "req_1")]);
        assert_eq!(m.request_id.as_deref(), Some("req_1"));
        let m = ResponseMeta::from_headers([("CF-Ray", "abc")]);
        assert_eq!(m.request_id.as_deref(), Some("abc"));
    }

    #[test]
    fn meta_rides_into_the_error() {
        let meta = ResponseMeta {
            request_id: Some("req_9".into()),
        };
        let e = from_status(429, "Rate limit reached", &meta);
        assert_eq!(e.request_id.as_deref(), Some("req_9"));
    }

    #[test]
    fn credentials_zlogiced_in_the_body_are_redacted() {
        let e = status(
            400,
            r#"{"error":"bad request","api_key":"sk-live-1234567890","note":"keep me"}"#,
        );
        assert!(!e.message.contains("sk-live-1234567890"), "{}", e.message);
        assert!(e.message.contains("<redacted>"));
        assert!(
            e.message.contains("keep me"),
            "non-sensitive content must be preserved: {}",
            e.message
        );
    }

    #[test]
    fn query_style_secrets_are_redacted_too() {
        let e = status(400, "upstream rejected: token=abcdef123&model=gpt-5");
        assert!(!e.message.contains("abcdef123"), "{}", e.message);
        assert!(e.message.contains("model=gpt-5"));
    }

    #[test]
    fn bodies_without_secrets_pass_through_untouched() {
        let body = r#"{"error":{"message":"invalid tool schema at properties.foo"}}"#;
        assert!(
            status(400, body)
                .message
                .contains("invalid tool schema at properties.foo")
        );
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "错误信息".repeat(1000);
        let e = status(500, &s);
        assert!(e.message.len() <= 8100);
        let e = status(500, &"e".repeat(3000));
        assert!(e.message.contains(&"e".repeat(3000)), "{}", e.message.len());
    }

    #[test]
    fn to_api_error_classifies_kind_into_code_category_and_detail() {
        let e = status(401, "bad api key");
        let api = to_api_error(&e, "test:model connection validation failed: bad api key");
        assert_eq!(api.code, "llm_auth_failed");
        assert_eq!(api.category, ErrorCategory::PermissionDenied);
        assert_eq!(
            api.details
                .get("diagnostic")
                .and_then(serde_json::Value::as_str),
            Some("test:model connection validation failed: bad api key")
        );
        assert_eq!(api.details.get("status"), Some(&serde_json::json!(401)));
        assert!(matches!(api.retry, RetryPolicy::Never));

        let e = status(400, "invalid max_tokens");
        let api = to_api_error(
            &e,
            "test:model connection validation failed: invalid max_tokens",
        );
        assert_eq!(api.code, "llm_request_invalid");
        assert_eq!(api.category, ErrorCategory::InvalidArgument);

        let e = status(503, "unavailable");
        let api = to_api_error(&e, "test:model connection validation failed: unavailable");
        assert_eq!(api.code, "llm_server_unavailable");
        assert_eq!(api.category, ErrorCategory::Unavailable);
        assert!(matches!(api.retry, RetryPolicy::Immediate));
    }

    #[test]
    fn to_api_error_carries_the_request_id() {
        let mut meta = ResponseMeta::default();
        meta.request_id = Some("req_42".into());
        let e = from_status(429, "slow down", &meta);
        let api = to_api_error(&e, "detail");
        assert_eq!(
            api.details
                .get("request_id")
                .and_then(serde_json::Value::as_str),
            Some("req_42")
        );
        assert_eq!(api.code, "llm_rate_limited");
    }
}
