//! Headless entry point for the production local gateway.

use std::{future::Future, io};

use gateway::runtime::Runtime;

/// 加载 .env(本地开发模板见 .env.example;.env 不入 git)。
/// 已存在的环境变量优先,不覆盖。
fn load_dotenv() {
    let Ok(text) = std::fs::read_to_string(".env") else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim(), v.trim());
            if !k.is_empty() && std::env::var(k).is_err() && !v.is_empty() {
                std::env::set_var(k, v);
            }
        }
    }
}

fn main() -> io::Result<()> {
    // Load the development environment before Tokio starts worker threads.
    load_dotenv();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // 诊断子命令(变更记录 34):有参数即进 CLI 模式,中继到正在
    // 运行的网关;无参数维持 headless serve。
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        let code = runtime.block_on(gateway::cli::run(&args));
        std::process::exit(code);
    }
    runtime.block_on(run())
}

async fn run() -> io::Result<()> {
    let shutdown = shutdown_signal()?;
    let runtime = Runtime::start(false).await?;
    let session = runtime.publish_session()?;
    println!("aubridged: http://127.0.0.1:{}", runtime.state().port());
    println!("管理会话文件: {}", session.path().display());
    runtime.serve_until_shutdown(shutdown).await
}

#[cfg(unix)]
fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
    })
}

#[cfg(windows)]
fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    let mut interrupt = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        interrupt.recv().await;
    })
}
