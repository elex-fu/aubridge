//! 插件装载与开关状态(设计文档 §9,M3 核心逻辑)。
//!
//! - 插件目录(`~/.aubridge/plugins/`)扫描:每个 `.json` 一份 manifest,
//!   校验失败逐条报告,**默认全部关闭**;
//! - enabled 状态持久化(`~/.aubridge/plugins/enabled.json`),
//!   只记录用户显式开启的 id——新装插件自动处于关(§9);
//! - 装载的插件进 GatewayState 时与内置 preset 同路径
//!   (crate-layout §1 插件化 = 数据合同)。

use std::collections::BTreeMap;
use std::path::Path;

use manifest::schema::{validate, Manifest};

/// 插件目录扫描结果。
#[derive(Debug, Default)]
pub struct PluginScan {
    /// 校验通过的 manifest(未开启)。
    pub plugins: Vec<Manifest>,
    /// 校验失败:文件名 → 可读错误清单。
    pub invalid: BTreeMap<String, Vec<String>>,
}

/// 扫描插件目录。缺目录 = 空结果(不是错误)。
pub fn scan_dir(dir: &Path) -> PluginScan {
    let mut out = PluginScan::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "enabled.json" {
            continue; // 状态文件不是插件
        }
        match std::fs::read_to_string(&path)
            .map_err(|e| vec![format!("读取失败: {e}")])
            .and_then(|text| {
                serde_json::from_str::<Manifest>(&text)
                    .map_err(|e| vec![format!("JSON 解析失败: {e}")])
            }) {
            Ok(m) => {
                let errs = validate(&m);
                if errs.is_empty() {
                    out.plugins.push(m);
                } else {
                    out.invalid.insert(name, errs);
                }
            }
            Err(errs) => {
                out.invalid.insert(name, errs);
            }
        }
    }
    out.plugins.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// enabled 状态存储(§9)。
///
/// 持久化格式 `enabled.json`:
/// ```json
/// { "auto": false, "on": { "kenari-free": true, "old-plugin": false } }
/// ```
/// `auto` = 自动开启新插件(独立设置,默认关);`on` 记录显式开关:
/// true = 开,false = 显式关(auto 模式下重启也不复活),缺省 = 默认关。
/// 兼容旧版扁平 map(视为 on、auto=false)。
pub struct EnabledState {
    path: std::path::PathBuf,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct EnabledFile {
    #[serde(default)]
    auto: bool,
    #[serde(default)]
    on: BTreeMap<String, bool>,
}

impl EnabledState {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    fn load(&self) -> EnabledFile {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return EnabledFile::default();
        };
        // 新格式优先;旧格式(扁平 map)降级为 on。
        serde_json::from_str::<EnabledFile>(&text)
            .or_else(|_| {
                serde_json::from_str::<BTreeMap<String, bool>>(&text)
                    .map(|on| EnabledFile { auto: false, on })
            })
            .unwrap_or_default()
    }

    /// 插件是否开启(缺记录 = false,默认全关)。
    pub fn is_enabled(&self, id: &str) -> bool {
        self.load().on.get(id).copied().unwrap_or(false)
    }

    /// 显式关闭记录(on=false)。auto 模式下重启不复活被关的插件。
    pub fn is_disabled(&self, id: &str) -> bool {
        self.load().on.get(id) == Some(&false)
    }

    /// 自动开启新插件(独立设置,默认关,§9)。
    pub fn auto_enable(&self) -> bool {
        self.load().auto
    }

    pub fn set_auto_enable(&self, on: bool) -> std::io::Result<()> {
        let mut f = self.load();
        f.auto = on;
        self.save(&f)
    }

    /// 设置开关并持久化(原子写)。关 = 记 false(显式关闭,
    /// auto 模式下重启也不复活);开 = 记 true。
    pub fn set(&self, id: &str, on: bool) -> std::io::Result<()> {
        let mut f = self.load();
        f.on.insert(id.into(), on);
        self.save(&f)
    }

    fn save(&self, f: &EnabledFile) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(f).expect("serialize enabled"),
        )?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aubridge-plugins-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const VALID: &str = r#"{
        "id": "kenari-free", "version": "0.3.1",
        "provider": {"base": "https://api.kenari.dev/v1", "auth": "api-key"},
        "free_models": ["deepseek-v4-flash:free"],
        "permissions": {"net": ["api.kenari.dev"], "store_key": true}
    }"#;

    #[test]
    fn scan_loads_valid_reports_invalid() {
        let dir = tmp("scan");
        std::fs::write(dir.join("good.json"), VALID).unwrap();
        std::fs::write(dir.join("bad.json"), r#"{"id": "BAD", "version": ""}"#).unwrap();
        std::fs::write(dir.join("enabled.json"), r#"{"other": true}"#).unwrap();

        let scan = scan_dir(&dir);
        assert_eq!(scan.plugins.len(), 1);
        assert_eq!(scan.plugins[0].id, "kenari-free");
        assert!(scan.invalid.contains_key("bad.json"));
        // 状态文件不是插件。
        assert!(!scan.invalid.contains_key("enabled.json"));
    }

    #[test]
    fn auto_enable_flag_persists_independent() {
        let dir = tmp("auto");
        let st = EnabledState::new(dir.join("enabled.json"));
        assert!(!st.auto_enable(), "自动开启默认关(§9)");
        st.set_auto_enable(true).unwrap();
        assert!(st.auto_enable());
        assert!(!st.is_enabled("anything"), "auto 不等于逐个开启");
        // 新实例重读 + 格式兼容(新格式 {auto, on})。
        let st2 = EnabledState::new(dir.join("enabled.json"));
        assert!(st2.auto_enable());
        // 旧版扁平 map 降级:auto=false。
        std::fs::write(dir.join("enabled.json"), r#"{"old": true}"#).unwrap();
        let st3 = EnabledState::new(dir.join("enabled.json"));
        assert!(!st3.auto_enable());
        assert!(st3.is_enabled("old"));
    }
}
