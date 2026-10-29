//! Typed rejection for validated agent prompt submission. Each construction site states
//! whether nothing reached the pane and a later attempt may succeed, so server-owned
//! callers never classify failures by parsing encoded JSON.
use super::responses::encode_error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptSubmitError {
    /// The pane was not written; the target may become ready, unblocked, or drain its queue.
    Retryable { code: String, message: String },
    /// Retrying the identical request cannot succeed.
    Terminal { code: String, message: String },
}

impl PromptSubmitError {
    pub(crate) fn retryable(code: &str, message: impl Into<String>) -> Self {
        Self::Retryable {
            code: code.into(),
            message: message.into(),
        }
    }

    pub(crate) fn terminal(code: &str, message: impl Into<String>) -> Self {
        Self::Terminal {
            code: code.into(),
            message: message.into(),
        }
    }

    pub(crate) fn not_found(target: &str) -> Self {
        Self::retryable(
            "agent_not_found",
            format!("agent target {target} not found"),
        )
    }

    pub(crate) fn not_ready(target: &str) -> Self {
        Self::retryable(
            "agent_not_ready",
            format!("agent {target} is not an active named agent"),
        )
    }

    /// A missing target may reappear; an ambiguous one needs a different target.
    pub(crate) fn from_target_body(is_missing: bool, body: crate::api::schema::ErrorBody) -> Self {
        if is_missing {
            Self::retryable(&body.code, body.message)
        } else {
            Self::terminal(&body.code, body.message)
        }
    }

    /// PTY queue saturation and a closed actor never wrote the submission.
    pub(crate) fn from_queue_error(error: &std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::BrokenPipe => {
                Self::retryable("agent_prompt_failed", error.to_string())
            }
            _ => Self::terminal("agent_prompt_failed", error.to_string()),
        }
    }

    pub(crate) fn from_try_send<T>(error: &tokio::sync::mpsc::error::TrySendError<T>) -> Self {
        Self::retryable("agent_prompt_failed", error.to_string())
    }

    pub(crate) fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }

    pub(crate) fn code(&self) -> &str {
        match self {
            Self::Retryable { code, .. } | Self::Terminal { code, .. } => code,
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Retryable { message, .. } | Self::Terminal { message, .. } => message,
        }
    }

    /// The JSON error response used by API callers.
    pub(crate) fn to_response(&self, id: String) -> String {
        encode_error(id, self.code(), self.message())
    }
}

impl std::fmt::Display for PromptSubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_saturation_and_closed_actor_are_retryable() {
        for kind in [
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::BrokenPipe,
        ] {
            let error = std::io::Error::new(kind, "pty input queue is full");
            assert!(PromptSubmitError::from_queue_error(&error).is_retryable());
        }
        let other = std::io::Error::other("encoding failed");
        assert!(!PromptSubmitError::from_queue_error(&other).is_retryable());
    }

    #[test]
    fn response_preserves_code_and_message() {
        let error = PromptSubmitError::not_ready("reviewer");
        let value: serde_json::Value =
            serde_json::from_str(&error.to_response("id".into())).unwrap();
        assert_eq!(value["error"]["code"], "agent_not_ready");
        assert!(error.is_retryable());
        assert!(!PromptSubmitError::terminal("empty_agent_prompt", "empty").is_retryable());
    }
}
