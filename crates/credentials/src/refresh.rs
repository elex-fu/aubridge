//! OAuth token 刷新(设计文档 §7,sub2api/magpie 语义)。
//!
//! - **刷新必等完**(magpie 铁律):刷新请求一旦发出,即使发起方取消
//!   也要等响应完成——OpenAI 轮换 refresh token,丢弃应答会让手里的
//!   旧 token 已被作废(refresh_token_reused);
//! - 401 → expired(刷新一次重试);刷新 invalid_grant → revoked;
//! - **只读纪律**:新 token 只返回给调用方存 aubridge 自己的 keyring,
//!   不写用户 CLI 文件(写回 opt-in 属后续迭代)。

use serde::Deserialize;

/// 刷新结果。
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshOutcome {
    /// 新 token 对(存 aubridge keyring,不动用户文件)。id_token
    /// 是 codex 通道的新鲜 JWT(exp/account_id 载体,§7),可为空。
    Refreshed {
        access: String,
        refresh: String,
        id_token: String,
    },
    /// refresh token 失效:标记 revoked,不再发送。
    Revoked,
}

/// 刷新端点响应(Claude `platform.claude.com/v1/oauth/token` /
/// Codex 同构的 token 端点)。
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// OpenAI 刷新端点可同时轮换 id_token，Codex 通道保留登录材料。
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub error: Option<String>,
}

/// 判定刷新结果(纯函数,可测;IO 由调用方注入)。
pub fn classify_refresh(resp: &TokenResponse) -> RefreshOutcome {
    if resp.error.as_deref() == Some("invalid_grant") {
        return RefreshOutcome::Revoked;
    }
    if resp.access_token.is_empty() {
        // 空响应按失效处理(宁可 revoked 不可发坏 token)。
        return RefreshOutcome::Revoked;
    }
    RefreshOutcome::Refreshed {
        access: resp.access_token.clone(),
        refresh: resp.refresh_token.clone(),
        id_token: resp.id_token.clone(),
    }
}

/// 401 处理决策(§7):expired → 刷新一次重试;刷新失败 → revoked。
#[derive(Debug, Clone, PartialEq)]
pub enum Auth401Decision {
    RefreshOnce,
    Revoke,
}

/// HTTP 刷新执行器:发出请求,**等完响应**(不可取消窗口)。
/// 端点/client 由调用方注入;此函数不做超时取消——语义即"必等完"。
pub async fn refresh_via_http(
    client: &reqwest::Client,
    endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<RefreshOutcome, String> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": client_id,
    });
    let resp = client
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let tr: TokenResponse = resp.json().await.map_err(|e| e.to_string())?;
    Ok(classify_refresh(&tr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_tokens_refreshed() {
        let r = TokenResponse {
            access_token: "new-at".into(),
            refresh_token: "new-rt".into(),
            id_token: "new-id".into(),
            error: None,
        };
        assert_eq!(
            classify_refresh(&r),
            RefreshOutcome::Refreshed {
                access: "new-at".into(),
                refresh: "new-rt".into(),
                id_token: "new-id".into(),
            }
        );
    }

    #[test]
    fn invalid_grant_revokes() {
        let r = TokenResponse {
            access_token: String::new(),
            refresh_token: String::new(),
            id_token: String::new(),
            error: Some("invalid_grant".into()),
        };
        assert_eq!(classify_refresh(&r), RefreshOutcome::Revoked);
    }

    #[test]
    fn empty_access_is_revoked_not_bad_token() {
        let r = TokenResponse {
            access_token: String::new(),
            refresh_token: "rt".into(),
            id_token: String::new(),
            error: None,
        };
        assert_eq!(classify_refresh(&r), RefreshOutcome::Revoked);
    }
}
