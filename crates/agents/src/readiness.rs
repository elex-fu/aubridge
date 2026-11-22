//! Executable discovery is independent of configuration and credentials.
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct Discovery {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<u64>,
}
impl Default for Discovery {
    fn default() -> Self {
        Self {
            status: "not_checked".into(),
            version: None,
            origin: None,
            observed_at: None,
        }
    }
}

pub fn program_name(id: &str) -> Option<&'static str> {
    match id {
        "claude-code" => Some("claude"),
        "codex" => Some("codex"),
        "opencode" => Some("opencode"),
        "gemini" => Some("gemini"),
        _ => None,
    }
}
pub fn supported_version(id: &str) -> Option<&'static str> {
    match id {
        "claude-code" => Some("2.1.325"),
        "codex" => Some("0.169.0-alpha.11.1"),
        "opencode" => Some("2.2.5"),
        "gemini" => Some("0.68.0"),
        _ => None,
    }
}
pub fn version_supported(id: &str, version: &str) -> bool {
    supported_version(id) == Some(version)
        || (id == "codex" && version == "0.169.1")
        || (id == "gemini" && version == "0.70.0")
}
pub fn version_from_output(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|s| s.trim_start_matches('v'))
        .find(|s| {
            let base = s.split('-').next().unwrap_or("");
            let parts: Vec<_> = base.split('.').collect();
            parts.len() >= 3
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_owned)
}

pub fn resolve_program(id: &str, name: &str) -> Result<PathBuf, String> {
    let path = find_program(id, name)?;
    // The child runs in a different cwd; resolve relative overrides before spawning.
    std::path::absolute(path).map_err(|_| "CLI 路径无法转换为绝对路径".into())
}

fn find_program(id: &str, name: &str) -> Result<PathBuf, String> {
    let env_key = match id {
        "claude-code" => "AUBRIDGE_CLAUDE_BIN",
        "codex" => "AUBRIDGE_CODEX_BIN",
        "opencode" => "AUBRIDGE_OPENCODE_BIN",
        "gemini" => "AUBRIDGE_GEMINI_BIN",
        _ => return Err(format!("Agent {id} 暂不支持测试")),
    };
    if let Some(value) = std::env::var_os(env_key) {
        let path = PathBuf::from(value);
        if executable(&path) {
            return Ok(path);
        }
        return Err(format!(
            "{env_key} 指向的 {name} 不可执行: {}",
            path.display()
        ));
    }
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for sub in [
            ".local/bin",
            ".npm-global/bin",
            ".local/node/bin",
            ".bun/bin",
            ".cargo/bin",
            ".volta/bin",
            "n/bin",
            ".claude/local",
        ] {
            dirs.push(home.join(sub));
        }
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"] {
        dirs.push(PathBuf::from(dir));
    }
    let mut not_executable = None;
    for dir in dirs {
        for candidate in candidates(&dir, name) {
            if executable(&candidate) {
                return Ok(candidate);
            }
            if candidate.is_file() {
                not_executable = Some(candidate);
            }
        }
    }
    if id == "opencode" {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            if let Some(program) = desktop_opencode(&home) {
                return Ok(program);
            }
        }
    }
    if let Some(path) = not_executable {
        return Err(format!("{name} 文件不可执行: {}", path.display()));
    }
    if id == "gemini" {
        return Err("未找到 gemini 命令。请先安装官方 Gemini CLI：npm install -g @google/gemini-cli，并确保命令在 PATH 中（支持 ~/.local/node/bin）；也可设置 AUBRIDGE_GEMINI_BIN 指向官方可执行文件。安装说明：https://geminicli.com/docs/get-started/installation/".into());
    }
    Err(format!(
        "未找到 {name} 命令。请先安装并确保它在 PATH 中，或设置 {env_key} 指向可执行文件。"
    ))
}

fn desktop_opencode(home: &Path) -> Option<PathBuf> {
    let root = home.join("Library/Application Support/ai.opencode.desktop/cli");
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let mut installed: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .take(64)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let (version, suffix) = name.split_once('-')?;
            if suffix != arch {
                return None;
            }
            let numbers: Option<Vec<u32>> =
                version.split('.').map(|part| part.parse().ok()).collect();
            let program = entry.path().join("opencode-cli");
            executable(&program).then_some((numbers?, program))
        })
        .collect();
    installed.sort_by(|a, b| b.0.cmp(&a.0));
    installed.into_iter().next().map(|(_, path)| path)
}

pub fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut paths = vec![dir.join(name)];
    if cfg!(windows) {
        paths.push(dir.join(format!("{name}.exe")));
    }
    paths
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions_preserve_prerelease_identity_and_unknown_is_not_supported() {
        assert_eq!(
            version_from_output("codex-cli 0.169.0-alpha.11.1\n"),
            Some("0.169.0-alpha.11.1".into())
        );
        assert_eq!(version_from_output("unknown development build"), None);
        assert_ne!(supported_version("codex"), Some("0.169.0"));
    }
    #[test]
    fn gemini_support_keeps_068_and_adds_only_exact_070() {
        assert!(version_supported("gemini", "0.68.0"));
        assert!(version_supported("gemini", "0.70.0"));
        for version in ["0.70.1", "0.70.0-rc.1", "0.71.0"] {
            assert!(!version_supported("gemini", version));
        }
        assert!(!version_supported("claude-code", "0.70.0"));
    }
}
