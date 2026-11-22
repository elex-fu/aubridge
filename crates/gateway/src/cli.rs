//! 诊断 CLI(status / providers / provider test,变更记录 34)。
//!
//! 只读中继:读数据目录会话文件,把请求转发给**正在运行**的生产网关
//! admin API。不自建 GatewayState——那会与运行中网关争数据目录锁,
//! 也拿不到真实路由与凭据状态(变更记录 33 共用口径)。命令行只做
//! 验证诊断:配置编辑、key 管理、暂停/恢复控制归 UI(变更记录 30
//! 的"CLI/TUI 形态"非目标维持)。

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::runtime::{data_dir, GatewayAccess};

const USAGE: &str = "用法:
  aubridged status
  aubridged providers
  aubridged provider test <id> [model]";

/// 诊断请求统一超时(check_provider 上游侧 10s,留网关排队余量;
/// B-11 首段 30s 口径对齐)。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 入口:返回进程退出码(0 成功 / 1 网关或上游不可用 / 2 用法错误)。
pub async fn run(args: &[String]) -> i32 {
    run_with_dir(args, &data_dir()).await
}

/// `run` 的可注入数据目录版(集成测试把会话文件写进临时目录)。
pub async fn run_with_dir(args: &[String], data_dir: &Path) -> i32 {
    match parse(args) {
        Some(Command::Status) => status(data_dir).await,
        Some(Command::Providers) => providers(data_dir).await,
        Some(Command::ProviderTest { id, model }) => {
            provider_test(data_dir, &id, model.as_deref()).await
        }
        None => {
            eprintln!("{USAGE}");
            2
        }
    }
}

enum Command {
    Status,
    Providers,
    ProviderTest { id: String, model: Option<String> },
}

fn parse(args: &[String]) -> Option<Command> {
    match args.first().map(String::as_str) {
        Some("status") => Some(Command::Status),
        Some("providers") => Some(Command::Providers),
        Some("provider") => match args.get(1).map(String::as_str) {
            // 超出 <id> [model] 的多余参数按用法错误拒绝,不静默吞掉。
            Some("test") if args.len() <= 4 => args.get(2).map(|id| Command::ProviderTest {
                id: id.clone(),
                model: args.get(3).cloned(),
            }),
            _ => None,
        },
        _ => None,
    }
}

/// 会话文件 + 出站客户端。会话文件由网关退出时删除:读得到但连不上,
/// 即上次运行残留;读不到即网关未启动。
fn session(data_dir: &Path) -> Result<(reqwest::Client, GatewayAccess), String> {
    let path = data_dir.join("gateway-session.json");
    let text = std::fs::read_to_string(&path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => {
            "网关未运行(无会话文件)。先启动 aubridged 或桌面 App。".to_string()
        }
        _ => format!("无法读取会话文件 {}: {error}", path.display()),
    })?;
    let access: GatewayAccess =
        serde_json::from_str(&text).map_err(|error| format!("会话文件损坏,请重启网关: {error}"))?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("无法创建 HTTP 客户端: {error}"))?;
    Ok((client, access))
}

/// 出站失败统一归因:连接被拒 = 会话残留;401 = 网关已重启换 token。
fn relay_error(error: &reqwest::Error) -> String {
    if error.is_connect() {
        "网关未运行或已退出(会话文件残留自上次运行)".to_string()
    } else {
        format!("请求失败: {error}")
    }
}

async fn status(data_dir: &Path) -> i32 {
    let (client, access) = match session(data_dir) {
        Ok(value) => value,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    match client
        .get(format!("{}/healthz", access.endpoint))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            println!("网关运行中 {}", access.endpoint);
            match client
                .get(format!("{}/admin/build", access.endpoint))
                .bearer_auth(&access.admin_token)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    if let Ok(info) = response.json::<serde_json::Value>().await {
                        println!(
                            "构建 {} · {} · {} · {}",
                            info["version"].as_str().unwrap_or("未知"),
                            info["git_revision"].as_str().unwrap_or("提交未知"),
                            info["mode"].as_str().unwrap_or("模式未知"),
                            info["profile"].as_str().unwrap_or("profile 未知")
                        );
                        println!(
                            "源码门禁：{}",
                            if info["source_verified"] == true {
                                "已验证"
                            } else {
                                "未附带验收标记"
                            }
                        );
                        if let Some(capabilities) = info["capabilities"].as_array() {
                            println!(
                                "能力 {}",
                                capabilities
                                    .iter()
                                    .filter_map(|value| value.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                        }
                    }
                }
                _ => println!("构建信息不可读；旧版本可能尚未提供此接口"),
            }
            0
        }
        Ok(response) => {
            eprintln!(
                "网关应答异常(HTTP {}),会话文件可能残留自上次运行",
                response.status().as_u16()
            );
            1
        }
        Err(error) => {
            eprintln!("{}", relay_error(&error));
            1
        }
    }
}

async fn providers(data_dir: &Path) -> i32 {
    let (client, access) = match session(data_dir) {
        Ok(value) => value,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let response = match client
        .get(format!("{}/admin/providers", access.endpoint))
        .bearer_auth(&access.admin_token)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            eprintln!("{}", relay_error(&error));
            return 1;
        }
    };
    let status = response.status();
    if !status.is_success() {
        eprintln!("网关拒绝诊断请求(HTTP {})", status.as_u16());
        return 1;
    }
    let body: serde_json::Value = match response.json().await {
        Ok(value) => value,
        Err(error) => {
            eprintln!("网关应答非 JSON: {error}");
            return 1;
        }
    };
    let Some(rows) = body["providers"].as_array() else {
        eprintln!("网关应答缺 providers 字段");
        return 1;
    };
    for row in rows {
        // 从宽渲染:字段缺失/类型异常时降级显示,不让单行坏值中断全表。
        let id = row["id"].as_str().unwrap_or("?");
        let source = row["source"].as_str().unwrap_or("?");
        let models = row["models"].as_array().map_or(0, Vec::len);
        let key = if row["key_set"].as_bool().unwrap_or(false) {
            "key ✓"
        } else {
            "key 未设置"
        };
        let routing = if row["paused"].as_bool().unwrap_or(false) {
            "已暂停"
        } else if row["ready"].as_bool().unwrap_or(false) {
            "就绪"
        } else {
            "未就绪"
        };
        println!("{id:<16} {source:<7} {key:<10} {routing:<4} 模型 {models}");
    }
    0
}

async fn provider_test(data_dir: &Path, id: &str, model: Option<&str>) -> i32 {
    let (client, access) = match session(data_dir) {
        Ok(value) => value,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let mut request = client
        .post(format!("{}/admin/providers/{id}/check", access.endpoint))
        .bearer_auth(&access.admin_token);
    if let Some(model) = model {
        request = request.query(&[("model", model)]);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            eprintln!("{}", relay_error(&error));
            return 1;
        }
    };
    let status = response.status();
    if status.as_u16() == 404 {
        eprintln!("未知 provider: {id}");
        return 1;
    }
    if !status.is_success() {
        eprintln!("网关拒绝诊断请求(HTTP {})", status.as_u16());
        return 1;
    }
    let body: serde_json::Value = match response.json().await {
        Ok(value) => value,
        Err(error) => {
            eprintln!("网关应答非 JSON: {error}");
            return 1;
        }
    };
    let check_status = body["status"].as_str().unwrap_or("?");
    let check_model = body["model"].as_str().unwrap_or("-");
    let upstream = body["upstream_status"]
        .as_u64()
        .map_or_else(String::new, |code| format!(" upstream={code}"));
    if check_status == "ok" {
        println!("✓ {id} {check_model} ok");
        0
    } else {
        println!("✗ {id} {check_model} {check_status}{upstream}");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_three_commands_and_rejects_everything_else() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(parse(&a(&["status"])).is_some());
        assert!(parse(&a(&["providers"])).is_some());
        assert!(matches!(
            parse(&a(&["provider", "test", "openrouter"])),
            Some(Command::ProviderTest { model: None, .. })
        ));
        assert!(matches!(
            parse(&a(&["provider", "test", "openrouter", "glm-4.6-flash"])),
            Some(Command::ProviderTest { model: Some(_), .. })
        ));
        for bad in [
            vec!["provider"],
            vec!["provider", "test"],
            vec!["provider", "pause", "x"],
            vec!["provider", "test", "x", "y", "z"],
            vec!["statuss"],
            vec![],
        ] {
            assert!(parse(&a(&bad)).is_none(), "{bad:?} 不应可解析");
        }
    }

    #[tokio::test]
    async fn missing_session_file_exits_one() {
        let dir = std::env::temp_dir().join("aubridge-cli-missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            run_with_dir(&["status".to_string()], &dir).await,
            1,
            "无会话文件 = 网关未运行"
        );
    }

    #[tokio::test]
    async fn corrupt_session_file_exits_one() {
        let dir = std::env::temp_dir().join("aubridge-cli-corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("gateway-session.json"), "not json").unwrap();
        assert_eq!(
            run_with_dir(&["status".to_string()], &dir).await,
            1,
            "坏 JSON = 会话损坏"
        );
    }

    #[tokio::test]
    async fn unknown_arguments_exit_two() {
        let dir = std::env::temp_dir().join("aubridge-cli-usage");
        assert_eq!(
            run_with_dir(&["nonsense".to_string()], &dir).await,
            2,
            "未知子命令 = 用法错误"
        );
    }
}
