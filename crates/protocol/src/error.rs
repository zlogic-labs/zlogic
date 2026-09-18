//! Structured failures shared by query responses and the live turn stream.
//! The protocol deliberately transports a message descriptor rather than a rendered sentence.
//! Hosts choose the locale; the engine may be shared by several hosts with different preferences.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Text that a host can render in its own locale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct LocalizedMessage {
    /// Catalogue key. Desktop prefixes engine keys with `error.` in its existing i18next bundle.
    pub key: String,
    /// Scalar interpolation values. Values are JSON so numbers retain pluralisation semantics.
    #[cfg_attr(feature = "ts", ts(type = "Record<string, string | number | boolean>"))]
    pub args: BTreeMap<String, Value>,
    /// Safe text for a missing catalogue entry or a newer engine talking to an older host.
    pub fallback: String,
}

impl LocalizedMessage {
    pub fn new(key: impl Into<String>, fallback: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            args: BTreeMap::new(),
            fallback: fallback.into(),
        }
    }

    pub fn arg(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.args.insert(name.into(), value.into());
        self
    }
}

impl std::fmt::Display for LocalizedMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fallback.fmt(f)
    }
}

/// Broad semantics used for presentation and transport status.
/// This is intentionally not the business error code. A UI may react to `Conflict` uniformly,
/// while the more precise `ApiError::code` is what logs carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ErrorCategory {
    NotWired,
    NotFound,
    Conflict,
    InvalidArgument,
    PermissionDenied,
    Unavailable,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum RetryPolicy {
    Never,
    Immediate,
    After { after_ms: u64 },
}

/// Engine failure returned to a host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ApiError {
    /// Stable business identifier, for example `workspace_path_outside_root`.
    pub code: String,
    pub category: ErrorCategory,
    pub message: LocalizedMessage,
    pub retry: RetryPolicy,
    /// Safe structured context for UI actions and diagnostics.
    #[cfg_attr(feature = "ts", ts(type = "Record<string, unknown>"))]
    pub details: BTreeMap<String, Value>,
    /// Correlates a safe UI failure with full server-side diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incident_id: Option<String>,
    /// Never crosses a serialization boundary. It keeps the source text available to host logs.
    #[serde(skip)]
    #[cfg_attr(feature = "ts", ts(skip))]
    diagnostic: Option<String>,
}

impl ApiError {
    pub fn new(
        code: impl Into<String>,
        category: ErrorCategory,
        message: LocalizedMessage,
    ) -> Self {
        Self {
            code: code.into(),
            category,
            message,
            retry: RetryPolicy::Never,
            details: BTreeMap::new(),
            incident_id: None,
            diagnostic: None,
        }
    }

    pub fn with_detail(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.details.insert(name.into(), value.into());
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_diagnostic(mut self, diagnostic: impl Into<String>) -> Self {
        self.diagnostic = Some(diagnostic.into());
        self
    }

    pub fn not_wired(op: impl Into<String>) -> Self {
        let op = op.into();
        Self::new(
            "engine_not_wired",
            ErrorCategory::NotWired,
            LocalizedMessage::new(
                "error.engineNotWired",
                "This engine operation is not available",
            )
            .arg("op", op.clone()),
        )
        .with_detail("op", op)
    }

    pub fn not_found(kind: impl Into<String>, id: impl std::fmt::Display) -> Self {
        let kind = kind.into();
        let id = id.to_string();
        Self::new(
            format!("{kind}_not_found").replace([' ', '-'], "_"),
            ErrorCategory::NotFound,
            LocalizedMessage::new(
                "error.resourceNotFound",
                "The requested resource was not found",
            )
            .arg("kind", kind.clone())
            .arg("id", id.clone()),
        )
        .with_detail("kind", kind)
        .with_detail("id", id)
    }

    pub fn invalid_code(code: impl Into<String>, fallback: impl Into<String>) -> Self {
        let code = code.into();
        Self::new(
            code.clone(),
            ErrorCategory::InvalidArgument,
            LocalizedMessage::new(format!("error.{code}"), fallback),
        )
    }

    pub fn conflict_code(code: impl Into<String>, fallback: impl Into<String>) -> Self {
        let code = code.into();
        Self::new(
            code.clone(),
            ErrorCategory::Conflict,
            LocalizedMessage::new(format!("error.{code}"), fallback),
        )
    }

    pub fn denied(code: impl Into<String>, fallback: impl Into<String>) -> Self {
        let code = code.into();
        Self::new(
            code.clone(),
            ErrorCategory::PermissionDenied,
            LocalizedMessage::new(format!("error.{code}"), fallback),
        )
    }

    pub fn unavailable(code: impl Into<String>, fallback: impl Into<String>) -> Self {
        let code = code.into();
        Self::new(
            code.clone(),
            ErrorCategory::Unavailable,
            LocalizedMessage::new(format!("error.{code}"), fallback),
        )
    }

    pub fn internal(diagnostic: impl std::fmt::Display) -> Self {
        let incident_id = uuid::Uuid::now_v7().to_string();
        let mut error = Self::new(
            "internal_unexpected",
            ErrorCategory::Internal,
            LocalizedMessage::new(
                "error.internalUnexpected",
                "An unexpected internal error occurred",
            ),
        );
        error.incident_id = Some(incident_id);
        error.diagnostic = Some(diagnostic.to_string());
        error
    }

    pub fn diagnostic(&self) -> Option<&str> {
        self.diagnostic.as_deref()
    }

    pub fn is_retryable(&self) -> bool {
        !matches!(self.retry, RetryPolicy::Never)
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_diagnostics_never_cross_the_wire() {
        let error = ApiError::internal("database path and provider response");
        let value = serde_json::to_value(&error).unwrap();

        assert_eq!(value["code"], "internal_unexpected");
        assert_eq!(value["category"], "internal");
        assert_eq!(
            value["message"]["fallback"],
            "An unexpected internal error occurred"
        );
        assert!(value.get("diagnostic").is_none());
        assert!(value["incident_id"].as_str().is_some());
        assert_eq!(
            error.diagnostic(),
            Some("database path and provider response")
        );
    }

    #[test]
    fn business_code_and_category_are_independent() {
        let error = ApiError::conflict_code("workspace_file_changed", "changed")
            .with_detail("path", "src/main.rs");
        let value = serde_json::to_value(error).unwrap();

        assert_eq!(value["code"], "workspace_file_changed");
        assert_eq!(value["category"], "conflict");
        assert_eq!(value["message"]["key"], "error.workspace_file_changed");
        assert_eq!(value["details"]["path"], "src/main.rs");
    }
}
