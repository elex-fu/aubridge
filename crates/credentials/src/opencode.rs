//! OpenCode OpenAI OAuth and OpenRouter API credentials, read-only.
//! Contract: docs/opencode-auth-design-2026-11-13.md.
//! The caller selects a path; this module never discovers accounts or writes login files.

use std::fmt;
use std::io;
use std::path::Path;

use serde_json::{Map, Value};

/// Verified against OpenCode's OpenAI plugin, not inferred from another OAuth client.
pub const OPENCODE_OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Clone, PartialEq, Eq)]
pub struct OpenCodeOpenAiLogin {
    pub access: String,
    pub refresh: String,
    /// OpenCode stores milliseconds, including an explicit zero for expired credentials.
    pub expires_ms: Option<u64>,
    pub account_id: String,
}

impl fmt::Debug for OpenCodeOpenAiLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenCodeOpenAiLogin")
            .field("access", &"[REDACTED]")
            .field("refresh", &"[REDACTED]")
            .field("expires_ms", &self.expires_ms)
            .field("account_id", &"[REDACTED]")
            .finish()
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn optional_string(object: &Map<String, Value>, name: &str) -> io::Result<String> {
    match object.get(name) {
        None => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(invalid_data("invalid OpenCode OAuth credential field")),
    }
}

fn read_auth_document(path: &Path) -> io::Result<Option<Value>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let document: Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid_data("invalid OpenCode auth JSON"))?;
    if !document.is_object() {
        return Err(invalid_data("OpenCode auth must be a JSON object"));
    }
    Ok(Some(document))
}

/// Missing/non-OAuth logins return None. Corrupt selected credentials fail closed;
/// unrelated provider entries are deliberately not deserialized.
pub fn read_opencode_openai_login(path: &Path) -> io::Result<Option<OpenCodeOpenAiLogin>> {
    let Some(document) = read_auth_document(path)? else {
        return Ok(None);
    };
    let Some(login) = document.get("openai").and_then(Value::as_object) else {
        return Ok(None);
    };
    if login.get("type").and_then(Value::as_str) != Some("oauth") {
        return Ok(None);
    }
    let access = login
        .get("access")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_data("invalid OpenCode OAuth access"))?;
    if access.trim().is_empty() {
        return Ok(None);
    }
    let expires_ms = match login.get("expires") {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| invalid_data("invalid OpenCode OAuth expiry"))?,
        ),
    };
    Ok(Some(OpenCodeOpenAiLogin {
        access: access.to_owned(),
        refresh: optional_string(login, "refresh")?,
        expires_ms,
        account_id: optional_string(login, "accountId")?,
    }))
}

/// Only the known OpenRouter API entry is eligible; OAuth and aliases are ignored.
pub fn read_opencode_openrouter_api_key(path: &Path) -> io::Result<Option<String>> {
    let Some(document) = read_auth_document(path)? else {
        return Ok(None);
    };
    let Some(entry) = document.get("openrouter").and_then(Value::as_object) else {
        return Ok(None);
    };
    if entry.get("type").and_then(Value::as_str) != Some("api") {
        return Ok(None);
    }
    let key = entry
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_data("invalid OpenCode OpenRouter API key"))?;
    Ok((!key.trim().is_empty()).then(|| key.to_owned()))
}

#[derive(Clone, PartialEq, Eq)]
pub enum OpenCodeRefreshOutcome {
    Refreshed {
        access: String,
        refresh: String,
        id_token: String,
        expires_in: Option<u64>,
    },
    Revoked,
}

impl fmt::Debug for OpenCodeRefreshOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refreshed { expires_in, .. } => f
                .debug_struct("Refreshed")
                .field("access", &"[REDACTED]")
                .field("refresh", &"[REDACTED]")
                .field("id_token", &"[REDACTED]")
                .field("expires_in", expires_in)
                .finish(),
            Self::Revoked => f.write_str("Revoked"),
        }
    }
}

fn classify_refresh(
    status: reqwest::StatusCode,
    document: &Value,
) -> Result<OpenCodeRefreshOutcome, String> {
    let object = document
        .as_object()
        .ok_or_else(|| "invalid OpenCode refresh response".to_string())?;
    if object.get("error").and_then(Value::as_str) == Some("invalid_grant") {
        return Ok(OpenCodeRefreshOutcome::Revoked);
    }
    if !status.is_success() {
        return Err(format!("OpenCode refresh HTTP {}", status.as_u16()));
    }
    let Some(access) = object
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|access| !access.trim().is_empty())
    else {
        return Ok(OpenCodeRefreshOutcome::Revoked);
    };
    Ok(OpenCodeRefreshOutcome::Refreshed {
        access: access.to_owned(),
        refresh: optional_string(object, "refresh_token")
            .map_err(|_| "invalid OpenCode refresh token field".to_string())?,
        id_token: optional_string(object, "id_token")
            .map_err(|_| "invalid OpenCode refresh ID token field".to_string())?,
        // An unusable lifetime does not inherit the old expired deadline.
        expires_in: object.get("expires_in").and_then(Value::as_u64),
    })
}

/// The production caller supplies the fixed official endpoint and a no-redirect client.
/// OpenCode's refresh uses form encoding; existing Codex JSON refresh remains separate.
pub async fn refresh_opencode_openai_via_http(
    client: &reqwest::Client,
    endpoint: &str,
    refresh_token: &str,
) -> Result<OpenCodeRefreshOutcome, String> {
    let response = client
        .post(endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", OPENCODE_OPENAI_CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|_| "OpenCode refresh request failed".to_string())?;
    let status = response.status();
    let document: Value = response
        .json()
        .await
        .map_err(|_| "invalid OpenCode refresh response JSON".to_string())?;
    classify_refresh(status, &document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture {
        dir: PathBuf,
        auth: PathBuf,
    }

    impl Fixture {
        fn new(content: Option<&str>) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "aubridge-opencode-auth-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            let auth = dir.join("auth.json");
            if let Some(content) = content {
                fs::write(&auth, content).unwrap();
            }
            Self { dir, auth }
        }

        fn read(&self) -> io::Result<Option<OpenCodeOpenAiLogin>> {
            read_opencode_openai_login(&self.auth)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn reads_only_openai_oauth_and_preserves_millisecond_expiry_and_file() {
        let content = json!({
            "unrelated": {"type":"unknown", "access":42, "expires":"bad"},
            "openai": {
                "type":"oauth", "access":" opaque-access ", "refresh":" refresh ",
                "expires":1_730_000_000_123u64, "accountId":" account ",
                "enterpriseUrl":"https://unused.invalid", "future":true
            }
        })
        .to_string();
        let fixture = Fixture::new(Some(&content));
        let login = fixture.read().unwrap().unwrap();
        assert_eq!(login.access, " opaque-access ");
        assert_eq!(login.refresh, " refresh ");
        assert_eq!(login.account_id, " account ");
        assert_eq!(login.expires_ms, Some(1_730_000_000_123));
        assert_eq!(fs::read_to_string(&fixture.auth).unwrap(), content);
    }

    #[test]
    fn missing_optional_fields_and_zero_expiry_remain_distinct() {
        let fixture = Fixture::new(Some(r#"{"openai":{"type":"oauth","access":"opaque"}}"#));
        let login = fixture.read().unwrap().unwrap();
        assert_eq!(login.access, "opaque");
        assert_eq!(login.refresh, "");
        assert_eq!(login.account_id, "");
        assert_eq!(login.expires_ms, None);
        fs::write(
            &fixture.auth,
            r#"{"openai":{"type":"oauth","access":"opaque","expires":0}}"#,
        )
        .unwrap();
        assert_eq!(fixture.read().unwrap().unwrap().expires_ms, Some(0));
    }

    #[test]
    fn missing_non_oauth_and_alias_entries_are_not_logins() {
        let fixture = Fixture::new(None);
        assert!(fixture.read().unwrap().is_none());
        for content in [
            r#"{}"#,
            r#"{"openai/":{"type":"oauth","access":"wrong-id"}}"#,
            r#"{"anthropic":{"type":"oauth","access":"wrong-provider"}}"#,
            r#"{"openai":null}"#,
            r#"{"openai":[]}"#,
            r#"{"openai":"string"}"#,
            r#"{"openai":{"access":"no-type"}}"#,
            r#"{"openai":{"type":"api","key":"api-key","expires":"bad"}}"#,
            r#"{"openai":{"type":"wellknown","key":"key","token":"token"}}"#,
            r#"{"openai":{"type":"unknown","access":"unknown"}}"#,
            r#"{"openai":{"type":"oauth","access":" \n\t "}}"#,
        ] {
            fs::write(&fixture.auth, content).unwrap();
            assert!(fixture.read().unwrap().is_none());
        }
    }

    #[test]
    fn invalid_json_roots_and_selected_oauth_fields_fail_closed() {
        let fixture = Fixture::new(None);
        for content in [
            "{",
            "not-json",
            "null",
            "[]",
            "42",
            r#"{"openai":{"type":"oauth"}}"#,
            r#"{"openai":{"type":"oauth","access":null}}"#,
            r#"{"openai":{"type":"oauth","access":42}}"#,
        ] {
            fs::write(&fixture.auth, content).unwrap();
            assert_eq!(
                fixture.read().unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        for (field, value) in [
            ("refresh", Value::Null),
            ("refresh", json!(42)),
            ("accountId", Value::Null),
            ("accountId", json!({"unexpected":true})),
            ("expires", Value::Null),
            ("expires", json!(-1)),
            ("expires", json!(1.5)),
            ("expires", json!("1730000000123")),
            ("expires", json!(true)),
        ] {
            let mut document = json!({"openai":{"type":"oauth","access":"fixture-access"}});
            document["openai"][field] = value;
            fs::write(&fixture.auth, document.to_string()).unwrap();
            assert_eq!(
                fixture.read().unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn io_errors_are_not_treated_as_missing_login() {
        let fixture = Fixture::new(None);
        fs::create_dir(&fixture.auth).unwrap();
        assert!(fixture.read().is_err());
    }

    #[test]
    fn openrouter_accepts_only_exact_api_entry_and_preserves_key_bytes() {
        let fixture = Fixture::new(Some(
            r#"{"openrouter":{"type":"api","key":" fixture-key ","metadata":{"future":"ignored"}},"openai":{"type":"oauth","access":42}}"#,
        ));
        let before = fs::read(&fixture.auth).unwrap();
        assert_eq!(
            read_opencode_openrouter_api_key(&fixture.auth)
                .unwrap()
                .as_deref(),
            Some(" fixture-key ")
        );
        assert_eq!(fs::read(&fixture.auth).unwrap(), before);
        for document in [
            json!({}),
            json!({"openrouter/":{"type":"api","key":"alias"}}),
            json!({"openai":{"type":"api","key":"wrong-provider"}}),
            json!({"openrouter":null}),
            json!({"openrouter":[]}),
            json!({"openrouter":{"key":"missing-type"}}),
            json!({"openrouter":{"type":"oauth","access":"wrong-kind"}}),
            json!({"openrouter":{"type":"wellknown","key":"wrong-kind","token":"token"}}),
            json!({"openrouter":{"type":"api","key":" \n\t "}}),
        ] {
            fs::write(&fixture.auth, document.to_string()).unwrap();
            assert!(read_opencode_openrouter_api_key(&fixture.auth)
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn openrouter_rejects_corruption_and_selected_api_bad_fields() {
        let fixture = Fixture::new(None);
        assert!(read_opencode_openrouter_api_key(&fixture.auth)
            .unwrap()
            .is_none());
        for content in [
            "{",
            "[]",
            "null",
            r#"{"openrouter":{"type":"api"}}"#,
            r#"{"openrouter":{"type":"api","key":null}}"#,
            r#"{"openrouter":{"type":"api","key":42}}"#,
        ] {
            fs::write(&fixture.auth, content).unwrap();
            assert_eq!(
                read_opencode_openrouter_api_key(&fixture.auth)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        fs::remove_file(&fixture.auth).unwrap();
        fs::create_dir(&fixture.auth).unwrap();
        assert!(read_opencode_openrouter_api_key(&fixture.auth).is_err());
    }

    #[test]
    fn openrouter_observes_external_rotation_and_logout_on_every_read() {
        let fixture = Fixture::new(Some(r#"{"openrouter":{"type":"api","key":"first-key"}}"#));
        assert_eq!(
            read_opencode_openrouter_api_key(&fixture.auth)
                .unwrap()
                .as_deref(),
            Some("first-key")
        );
        fs::write(
            &fixture.auth,
            r#"{"openrouter":{"type":"api","key":"second-key"}}"#,
        )
        .unwrap();
        assert_eq!(
            read_opencode_openrouter_api_key(&fixture.auth)
                .unwrap()
                .as_deref(),
            Some("second-key")
        );
        fs::write(&fixture.auth, "{}").unwrap();
        assert!(read_opencode_openrouter_api_key(&fixture.auth)
            .unwrap()
            .is_none());
    }

    #[test]
    fn each_read_observes_replacement_logout_corruption_and_recovery() {
        let first = r#"{"openai":{"type":"oauth","access":"first","expires":1000}}"#;
        let second = r#"{"openai":{"type":"oauth","access":"second","expires":2000}}"#;
        let fixture = Fixture::new(Some(first));
        assert_eq!(fixture.read().unwrap().unwrap().access, "first");
        let replacement = fixture.dir.join("replacement.json");
        fs::write(&replacement, second).unwrap();
        fs::rename(&replacement, &fixture.auth).unwrap();
        assert_eq!(fixture.read().unwrap().unwrap().access, "second");
        fs::write(&fixture.auth, "{").unwrap();
        assert!(fixture.read().is_err());
        fs::write(&fixture.auth, second).unwrap();
        assert_eq!(fixture.read().unwrap().unwrap().access, "second");
        fs::write(
            &fixture.auth,
            r#"{"openai":{"type":"api","key":"different-kind"}}"#,
        )
        .unwrap();
        assert!(fixture.read().unwrap().is_none());
        fs::remove_file(&fixture.auth).unwrap();
        assert!(fixture.read().unwrap().is_none());
    }

    #[test]
    fn debug_and_parse_errors_do_not_disclose_credentials() {
        let fixture = Fixture::new(Some(
            r#"{"openai":{"type":"oauth","access":"unique-access-secret","refresh":"unique-refresh-secret","accountId":"unique-account-secret"}}"#,
        ));
        let debug = format!("{:?}", fixture.read().unwrap().unwrap());
        for secret in [
            "unique-access-secret",
            "unique-refresh-secret",
            "unique-account-secret",
        ] {
            assert!(!debug.contains(secret));
        }
        fs::write(
            &fixture.auth,
            r#"{"openai":{"type":"oauth","access":{"unique-access-secret":true}}}"#,
        )
        .unwrap();
        assert!(!fixture
            .read()
            .unwrap_err()
            .to_string()
            .contains("unique-access-secret"));
        let refreshed = OpenCodeRefreshOutcome::Refreshed {
            access: "unique-access-secret".into(),
            refresh: "unique-refresh-secret".into(),
            id_token: "unique-id-secret".into(),
            expires_in: Some(60),
        };
        let debug = format!("{refreshed:?}");
        for secret in [
            "unique-access-secret",
            "unique-refresh-secret",
            "unique-id-secret",
        ] {
            assert!(!debug.contains(secret));
        }
    }

    #[test]
    fn refresh_classification_handles_revocation_status_and_unknown_lifetime() {
        assert_eq!(
            classify_refresh(
                reqwest::StatusCode::BAD_REQUEST,
                &json!({"error":"invalid_grant"})
            )
            .unwrap(),
            OpenCodeRefreshOutcome::Revoked
        );
        for body in [
            json!({}),
            json!({"access_token":""}),
            json!({"access_token":" \t"}),
        ] {
            assert_eq!(
                classify_refresh(reqwest::StatusCode::OK, &body).unwrap(),
                OpenCodeRefreshOutcome::Revoked
            );
        }
        assert!(classify_refresh(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"access_token":"not-success"})
        )
        .is_err());
        assert!(classify_refresh(reqwest::StatusCode::OK, &json!([])).is_err());
        for lifetime in [
            None,
            Some(Value::Null),
            Some(json!(-1)),
            Some(json!(1.5)),
            Some(json!("60")),
        ] {
            let mut body = json!({"access_token":"fresh-access"});
            if let Some(lifetime) = lifetime {
                body["expires_in"] = lifetime;
            }
            assert_eq!(
                classify_refresh(reqwest::StatusCode::OK, &body).unwrap(),
                OpenCodeRefreshOutcome::Refreshed {
                    access: "fresh-access".into(),
                    refresh: String::new(),
                    id_token: String::new(),
                    expires_in: None,
                }
            );
        }
        assert!(classify_refresh(
            reqwest::StatusCode::OK,
            &json!({"access_token":"fresh","refresh_token":null})
        )
        .is_err());
    }

    #[tokio::test]
    async fn refresh_uses_only_openai_form_fields_and_returns_lifetime() {
        use axum::{http::HeaderMap, routing::post, Json, Router};

        let app = Router::new().route("/oauth/token", post(|headers: HeaderMap, body: String| async move {
            assert_eq!(headers["content-type"], "application/x-www-form-urlencoded");
            let mut fields: Vec<_> = body.split('&').collect();
            fields.sort_unstable();
            assert_eq!(fields, vec![
                "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
                "grant_type=refresh_token",
                "refresh_token=fixture%2Brefresh%26%3D",
            ]);
            Json(json!({"access_token":"fresh-access","refresh_token":"fresh-refresh","id_token":"fresh-id","expires_in":60}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        let result =
            refresh_opencode_openai_via_http(&client, &endpoint, "fixture+refresh&=").await;
        server.abort();
        assert_eq!(
            result.unwrap(),
            OpenCodeRefreshOutcome::Refreshed {
                access: "fresh-access".into(),
                refresh: "fresh-refresh".into(),
                id_token: "fresh-id".into(),
                expires_in: Some(60),
            }
        );
    }
}
