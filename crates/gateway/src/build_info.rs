//! Identity of this executable, never inferred from the caller's working tree.

#[derive(serde::Serialize)]
pub struct BuildInfo {
    pub version: &'static str,
    pub git_revision: Option<&'static str>,
    pub git_tree: Option<&'static str>,
    pub mode: &'static str,
    pub profile: &'static str,
    pub source_verified: bool,
    pub pid: u32,
    pub capabilities: &'static [&'static str],
}

pub fn current() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        git_revision: nonempty(env!("AUBRIDGE_GIT_REVISION")),
        git_tree: nonempty(env!("AUBRIDGE_GIT_TREE")),
        mode: env!("AUBRIDGE_BINARY_MODE"),
        profile: env!("AUBRIDGE_BINARY_PROFILE"),
        source_verified: env!("AUBRIDGE_SOURCE_CHECKED") == "true",
        pid: std::process::id(),
        capabilities: &[
            "messages",
            "chat",
            "responses-http",
            "quota-windows",
            "model-directory",
            "request-limits",
            "cache-usage",
            "price-snapshots",
            "request-id",
            "opencode-cli-test",
            "codex-compaction",
            "claude-cli-transport",
            "session-diagnostics",
            "media-preflight",
            "benchmark-snapshots",
            "gemini-native-ingress",
        ],
    }
}

fn nonempty(value: &'static str) -> Option<&'static str> {
    (!value.is_empty()).then_some(value)
}
