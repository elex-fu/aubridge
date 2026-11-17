//! 注册表验收(变更记录 30):四接线器经 trait 走全往返,
//! 注册表 id 唯一,gateway 侧无需 match 分支即拿到全部行为。

use std::collections::BTreeSet;

use agents::{wirings, AgentsPaths};

fn paths(tag: &str) -> AgentsPaths {
    let dir = std::env::temp_dir().join(format!("aubridge-agents-registry-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    AgentsPaths {
        claude_settings: dir.join("claude").join("settings.json"),
        codex_config: dir.join("codex").join("config.toml"),
        opencode_config: dir.join("opencode").join("opencode.json"),
        gemini_settings: dir.join("gemini").join("settings.json"),
        gemini_env: dir.join("gemini").join(".env"),
        state_dir: dir.join("state"),
    }
}

/// 用户已有内容(各 agent 一份):接线后必须原样保留。
fn seed_user_files(p: &AgentsPaths) {
    std::fs::create_dir_all(p.claude_settings.parent().unwrap()).unwrap();
    std::fs::write(&p.claude_settings, r#"{"model":"sonnet"}"#).unwrap();
    std::fs::create_dir_all(p.codex_config.parent().unwrap()).unwrap();
    std::fs::write(&p.codex_config, "model = \"gpt-5.1\"\n").unwrap();
    std::fs::create_dir_all(p.opencode_config.parent().unwrap()).unwrap();
    std::fs::write(&p.opencode_config, r#"{"theme":"dark"}"#).unwrap();
    std::fs::create_dir_all(p.gemini_settings.parent().unwrap()).unwrap();
    std::fs::write(&p.gemini_settings, r#"{"theme":"dark"}"#).unwrap();
}

#[test]
fn registry_ids_unique_and_complete() {
    let p = paths("ids");
    let ws = wirings(&p, 3427, Vec::new());
    let ids: BTreeSet<&str> = ws.iter().map(|w| w.id()).collect();
    assert_eq!(ids.len(), ws.len(), "id 必须唯一");
    assert_eq!(
        ids.into_iter().collect::<Vec<_>>(),
        vec!["claude-code", "codex", "gemini", "opencode"]
    );
    // hint 仅 codex 有(残余动作提示)。
    assert!(ws.iter().any(|w| w.hint().is_some()));
    // drift 仅 Claude Code 有。
    assert!(ws.iter().any(|w| w.drift().is_some()));
}

#[test]
fn all_wirings_roundtrip_via_trait() {
    let p = paths("roundtrip");
    seed_user_files(&p);
    let ws = wirings(
        &p,
        3427,
        vec![("glm-4.5-flash".into(), "glm-4.5-flash".into())],
    );
    for w in &ws {
        // 未接线前 detected = 配置文件存在(种子已放)。
        assert!(w.detected(), "{} 应检测到配置", w.id());
        assert!(!w.wired(), "{} 初始未接线", w.id());

        w.apply().expect("apply");
        assert!(w.wired(), "{} 接线后应在位", w.id());
        // 幂等:重复 apply 不炸也不改结果。
        w.apply().expect("apply idempotent");
        assert!(w.wired());

        w.disconnect().expect("disconnect");
        assert!(!w.wired(), "{} 断开后应还原", w.id());
    }
    // 用户内容保留。
    assert!(std::fs::read_to_string(&p.claude_settings)
        .unwrap()
        .contains("sonnet"));
    assert!(std::fs::read_to_string(&p.codex_config)
        .unwrap()
        .contains("gpt-5.1"));
    assert!(std::fs::read_to_string(&p.opencode_config)
        .unwrap()
        .contains("dark"));
    assert!(std::fs::read_to_string(&p.gemini_settings)
        .unwrap()
        .contains("dark"));
}

#[test]
fn undetected_when_config_absent() {
    let p = paths("undetected");
    // 不 seed:全部配置文件不存在。
    let ws = wirings(&p, 3427, Vec::new());
    assert!(ws.iter().all(|w| !w.detected() && !w.wired()));
}

/// 模型列表经注册表传入 OpenCode(网关聚合模型,变更记录 30 ②)。
#[test]
fn opencode_models_injected_through_registry() {
    let p = paths("models");
    std::fs::create_dir_all(p.opencode_config.parent().unwrap()).unwrap();
    let ws = wirings(
        &p,
        3427,
        vec![("glm-4.5-flash".into(), "GLM 4.5 Flash".into())],
    );
    ws.iter()
        .find(|w| w.id() == "opencode")
        .unwrap()
        .apply()
        .unwrap();
    let text = std::fs::read_to_string(&p.opencode_config).unwrap();
    assert!(text.contains("GLM 4.5 Flash"));
}

/// drift 经 trait 暴露(Claude Code 专属语义)。
#[test]
fn drift_surface_via_trait() {
    let p = paths("drift");
    std::fs::create_dir_all(p.claude_settings.parent().unwrap()).unwrap();
    let ws = wirings(&p, 3427, Vec::new());
    let cc = ws.iter().find(|w| w.id() == "claude-code").unwrap();
    assert_eq!(cc.drift(), Some(agents::Drift::Unwired));
    cc.apply().unwrap();
    assert_eq!(cc.drift(), Some(agents::Drift::Clean));
}
