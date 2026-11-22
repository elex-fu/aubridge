//! Strong diagnostic identity. Routing affinity deliberately has a different contract.
use axum::http::HeaderMap;
use proto::protocol::MessagesRequest;

#[derive(Default)]
pub(crate) struct Identity {
    pub session: Option<String>,
    pub parent: Option<String>,
    pub purpose: Option<String>,
}

fn id(value: &str) -> Option<String> {
    let value = value.trim();
    // Reject rather than truncate: truncation can merge unrelated conversations.
    (!value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:|".contains(c)))
    .then(|| value.to_owned())
}

pub(crate) fn extract(headers: &HeaderMap, req: Option<&MessagesRequest>) -> Identity {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let mut session = [
        "thread-id",
        "x-codex-thread-id",
        "x-opencode-session",
        "x-gemini-cli-session-id",
        "x-session-affinity",
        "x-session-id",
        "session_id",
        "session-id",
        "x-claude-code-session-id",
        "x-conversation-id",
        "x-deepseek-harness-session-id",
    ]
    .iter()
    .find_map(|name| header(name).and_then(id));
    if session.is_none() {
        session = req
            .and_then(|r| r.metadata.as_ref())
            .and_then(|m| m.user_id.as_deref())
            .and_then(|user| {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(user) {
                    return value
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .and_then(id);
                }
                // Claude's legacy user_<account>_account_<account>_session_<id>:
                // persist only the explicit session suffix, never account labels.
                user.rsplit_once("_session_")
                    .and_then(|(_, session)| id(session))
            });
    }
    let mut parent = header("x-codex-parent-thread-id")
        .or_else(|| header("x-parent-session-id"))
        .or_else(|| header("x-parent-conversation-id"))
        .and_then(id);
    if let (Some(root), Some(child)) = (
        session.as_ref(),
        header("x-claude-code-agent-id").and_then(id),
    ) {
        parent = Some(root.clone());
        session = Some(format!("{root}|agent:{child}"));
    }
    let purpose = header("x-aubridge-request-purpose")
        .filter(|v| {
            matches!(
                *v,
                "normal" | "compact" | "title" | "subagent" | "count_tokens"
            )
        })
        .map(str::to_owned)
        .or_else(|| {
            header("x-deepseek-harness-compact")
                .filter(|v| {
                    !matches!(
                        v.to_ascii_lowercase().as_str(),
                        "" | "0" | "false" | "no" | "off"
                    )
                })
                .map(|_| "compact".to_owned())
        });
    Identity {
        session,
        parent,
        purpose,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_identity_only_and_child_is_separate() {
        let mut headers = HeaderMap::new();
        assert!(extract(&headers, None).session.is_none());
        headers.insert("x-session-id", "thread-1".parse().unwrap());
        headers.insert("x-claude-code-agent-id", "child-2".parse().unwrap());
        let identity = extract(&headers, None);
        assert_eq!(identity.session.as_deref(), Some("thread-1|agent:child-2"));
        assert_eq!(identity.parent.as_deref(), Some("thread-1"));
        assert!(identity.purpose.is_none());
    }
    #[test]
    fn codex_threads_share_session_but_never_diagnostic_identity() {
        let mut headers = HeaderMap::new();
        headers.insert("session-id", "shared-session".parse().unwrap());
        headers.insert("thread-id", "parent-thread".parse().unwrap());
        assert_eq!(
            extract(&headers, None).session.as_deref(),
            Some("parent-thread")
        );
        headers.insert("thread-id", "child-thread".parse().unwrap());
        headers.insert("x-codex-parent-thread-id", "parent-thread".parse().unwrap());
        let identity = extract(&headers, None);
        assert_eq!(identity.session.as_deref(), Some("child-thread"));
        assert_eq!(identity.parent.as_deref(), Some("parent-thread"));
    }
    #[test]
    fn metadata_never_retains_account_and_no_prompt_fallback() {
        let mut req: MessagesRequest = serde_json::from_value(serde_json::json!({"model":"test","messages":[],"max_tokens":1,"metadata":{"user_id":"user_secret_account_secret_session_thread-1"}})).unwrap();
        assert_eq!(
            extract(&HeaderMap::new(), Some(&req)).session.as_deref(),
            Some("thread-1")
        );
        req.metadata.as_mut().unwrap().user_id = Some("secret-account".into());
        assert!(extract(&HeaderMap::new(), Some(&req)).session.is_none());
        assert!(id(&"a".repeat(257)).is_none());
    }
}
