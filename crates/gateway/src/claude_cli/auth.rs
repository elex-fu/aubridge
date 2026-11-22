//! Official CLI alone owns authentication and native token refresh.
use serde_json::Value;
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Clone)]
pub(super) struct Login {
    pub subject: String,
    pub env: BTreeMap<OsString, OsString>,
}

pub(super) fn environment() -> BTreeMap<OsString, OsString> {
    const KEEP: &[&str] = &[
        "HOME",
        "PATH",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "all_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "NODE_EXTRA_CA_CERTS",
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ];
    KEEP.iter()
        .filter_map(|key| std::env::var_os(key).map(|value| (OsString::from(key), value)))
        .collect()
}

pub(super) fn command(program: &PathBuf, env: &BTreeMap<OsString, OsString>) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .envs(env)
        .env("DISABLE_AUTOUPDATER", "1")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

pub(super) async fn probe(program: &PathBuf) -> Option<Login> {
    let mut env = environment();
    let mut cmd = command(program, &env);
    cmd.args(["--setting-sources", "", "auth", "status", "--json"]);
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let mut bytes = Vec::new();
        (&mut stdout)
            .take(65537)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        if bytes.len() > 65536 || !child.wait().await.ok()?.success() {
            return None;
        }
        let status: Value = serde_json::from_slice(&bytes).ok()?;
        if status["loggedIn"] != true
            || !matches!(
                status["authMethod"].as_str(),
                Some("oauth_token" | "claude.ai")
            )
            || status["apiProvider"] != "firstParty"
        {
            return None;
        }
        if status["authMethod"] != "oauth_token" {
            env.remove(&OsString::from("CLAUDE_CODE_OAUTH_TOKEN"));
        }
        let identity = if let Some(token) = env
            .get(&OsString::from("CLAUDE_CODE_OAUTH_TOKEN"))
            .filter(|token| !token.is_empty())
        {
            token.to_string_lossy().into_owned()
        } else {
            let identity = ["accountUuid", "accountId", "email"]
                .iter()
                .filter_map(|k| status[*k].as_str())
                .collect::<Vec<_>>()
                .join("|");
            if identity.is_empty() {
                return None;
            }
            identity
        };
        Some(Login {
            subject: credentials::quota::subject_tag("claude-cli", &identity),
            env,
        })
    })
    .await
    .ok()
    .flatten();
    super::run::kill_group(&mut child).await;
    result
}
