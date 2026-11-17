//! Gemini CLI 接线(magpie `internal/agent/gemini.go` 字段核实):
//!
//! - `~/.gemini/settings.json`:`security.auth.selectedType` →
//!   `"gemini-api-key"`(API key 模式,走我们注入的 key);
//! - `~/.gemini/.env`:`GOOGLE_GEMINI_BASE_URL` + `GEMINI_API_KEY`
//!   (KEY=VALUE 行级编辑);
//! - stash:原值备份,断开原样还原(env 行删除 + selectedType 还原)。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

pub struct GeminiCli {
    pub settings: PathBuf,
    pub env_file: PathBuf,
    pub state_dir: PathBuf,
    pub port: u16,
}

impl GeminiCli {
    pub fn new(settings: PathBuf, env_file: PathBuf, state_dir: PathBuf, port: u16) -> Self {
        Self {
            settings,
            env_file,
            state_dir,
            port,
        }
    }

    /// 接线:改 selectedType + 注入 env 两行(幂等)。
    pub fn apply(&self) -> std::io::Result<()> {
        // 幂等守卫:在位时不动 stash(否则接线态被记成原值,断开
        // 无法还原——与 claude-code/codex 同口径)。
        if self.wired() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.state_dir)?;
        // stash:原 selectedType + 原 env 两行(缺失记 null)。
        let original_selected = get_json(
            &read_json_or_default(&self.settings),
            &["security", "auth", "selectedType"],
        );
        let original_env = read_env(&self.env_file);
        let stash = json!({
            "selectedType": original_selected,
            "GOOGLE_GEMINI_BASE_URL": original_env.get("GOOGLE_GEMINI_BASE_URL"),
            "GEMINI_API_KEY": original_env.get("GEMINI_API_KEY"),
        });
        atomic_write_json(&self.stash_path(), &stash)?;

        // settings.json:selectedType → gemini-api-key。
        let mut v = read_json_or_default(&self.settings);
        set_path(
            &mut v,
            &["security", "auth", "selectedType"],
            json!("gemini-api-key"),
        );
        atomic_write_json(&self.settings, &v)?;

        // .env:设置两行。
        let mut env = original_env;
        env.insert(
            "GOOGLE_GEMINI_BASE_URL".into(),
            format!("http://127.0.0.1:{}", self.port),
        );
        env.insert("GEMINI_API_KEY".into(), "aubridge-gemini".into());
        write_env(&self.env_file, &env)
    }

    /// 断开:按 stash 还原(原缺失则删除)。
    pub fn disconnect(&self) -> std::io::Result<()> {
        if !self.stash_path().exists() {
            return Ok(());
        }
        let stash: Value = read_json(&self.stash_path())?;
        let mut v = read_json_or_default(&self.settings);
        match stash.get("selectedType").cloned() {
            Some(prev) if !prev.is_null() => {
                set_path(&mut v, &["security", "auth", "selectedType"], prev)
            }
            _ => del_path(&mut v, &["security", "auth", "selectedType"]),
        }
        atomic_write_json(&self.settings, &v)?;

        let mut env = read_env(&self.env_file);
        env.remove("GOOGLE_GEMINI_BASE_URL");
        env.remove("GEMINI_API_KEY");
        for key in ["GOOGLE_GEMINI_BASE_URL", "GEMINI_API_KEY"] {
            if let Some(Value::String(s)) = stash.get(key) {
                if stash.get(key).is_some_and(|v| !v.is_null()) {
                    env.insert(key.into(), s.clone());
                }
            }
        }
        write_env(&self.env_file, &env)?;
        let _ = std::fs::remove_file(self.stash_path());
        Ok(())
    }

    pub fn wired(&self) -> bool {
        read_env(&self.env_file)
            .get("GOOGLE_GEMINI_BASE_URL")
            .is_some_and(|v| v.ends_with(&self.port.to_string()))
    }

    fn stash_path(&self) -> PathBuf {
        self.state_dir.join("gemini.stash.json")
    }
}

impl crate::AgentWiring for GeminiCli {
    fn id(&self) -> &'static str {
        "gemini"
    }
    fn name(&self) -> &'static str {
        "Gemini CLI"
    }
    fn apply(&self) -> std::io::Result<()> {
        GeminiCli::apply(self)
    }
    fn disconnect(&self) -> std::io::Result<()> {
        GeminiCli::disconnect(self)
    }
    fn wired(&self) -> bool {
        GeminiCli::wired(self)
    }
    fn detected(&self) -> bool {
        self.settings.exists() || self.env_file.exists()
    }
}

fn read_json_or_default(path: &Path) -> Value {
    read_json(path).unwrap_or_else(|_| json!({}))
}

fn read_json(path: &Path) -> std::io::Result<Value> {
    let t = std::fs::read_to_string(path)?;
    serde_json::from_str(&t).map_err(std::io::Error::other)
}

fn get_json(v: &Value, path: &[&str]) -> Value {
    let mut cur = v;
    for seg in path {
        cur = cur.get(seg).unwrap_or(&Value::Null);
    }
    cur.clone()
}

fn set_path(v: &mut Value, path: &[&str], val: Value) {
    let mut cur = v;
    for (i, seg) in path.iter().enumerate() {
        if i == path.len() - 1 {
            cur.as_object_mut()
                .expect("object")
                .insert((*seg).into(), val);
            return;
        }
        cur = cur
            .as_object_mut()
            .expect("object")
            .entry((*seg).to_string())
            .or_insert_with(|| json!({}));
    }
}

fn del_path(v: &mut Value, path: &[&str]) {
    if path.len() < 2 {
        return;
    }
    let (parent, last) = path.split_at(path.len() - 1);
    let mut cur = v;
    for seg in parent {
        match cur.get_mut(*seg) {
            Some(c) => cur = c,
            None => return,
        }
    }
    if let Some(o) = cur.as_object_mut() {
        o.remove(last[0]);
    }
}

/// 读 .env 为有序行对(KEY=VALUE;注释/空行丢弃,重写时纯键值)。
fn read_env(path: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if !k.trim().is_empty() && !k.trim_start().starts_with('#') {
                    out.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
    }
    out
}

fn write_env(path: &Path, env: &std::collections::BTreeMap<String, String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

fn atomic_write_json(path: &Path, v: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v).expect("serialize"))?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("aubridge-gemini-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (
            dir.join("settings.json"),
            dir.join(".env"),
            dir.join("state"),
        )
    }

    #[test]
    fn apply_sets_env_and_selected_type() {
        let (settings, env_file, state) = tmp("apply");
        std::fs::write(&settings, r#"{"theme":"dark"}"#).unwrap();
        std::fs::write(&env_file, "CUSTOM=1\n").unwrap();

        GeminiCli::new(settings.clone(), env_file.clone(), state, 3427)
            .apply()
            .unwrap();
        let env = read_env(&env_file);
        assert_eq!(
            env.get("GOOGLE_GEMINI_BASE_URL").unwrap(),
            "http://127.0.0.1:3427"
        );
        assert_eq!(env.get("GEMINI_API_KEY").unwrap(), "aubridge-gemini");
        assert_eq!(env.get("CUSTOM").unwrap(), "1");
        let v: Value = read_json(&settings).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["security"]["auth"]["selectedType"], "gemini-api-key");
    }

    #[test]
    fn disconnect_restores_original() {
        let (settings, env_file, state) = tmp("disconnect");
        std::fs::write(
            &settings,
            r#"{"security":{"auth":{"selectedType":"oauth"}}}"#,
        )
        .unwrap();
        std::fs::write(&env_file, "GOOGLE_GEMINI_BASE_URL=https://orig\n").unwrap();

        let g = GeminiCli::new(settings.clone(), env_file.clone(), state, 3427);
        g.apply().unwrap();
        assert!(g.wired());
        g.disconnect().unwrap();
        assert!(!g.wired());

        let v: Value = read_json(&settings).unwrap();
        assert_eq!(v["security"]["auth"]["selectedType"], "oauth");
        assert_eq!(
            read_env(&env_file).get("GOOGLE_GEMINI_BASE_URL").unwrap(),
            "https://orig"
        );
    }
}
