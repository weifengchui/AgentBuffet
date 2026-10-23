//! buffetd：单二进制入口。配置校验失败拒绝启动（报错带文件名）。

use agentbuffet::proxy::AppState;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

fn default_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("AGENTBUFFET_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".agentbuffet").join("config.yaml")
}

#[tokio::main]
async fn main() {
    let path = default_config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("buffetd: 读取配置失败 {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let filename = path.display().to_string();
    let snap = match agentbuffet::config::load_str(&text, &filename) {
        Ok(s) => s,
        Err(errs) => {
            eprintln!("buffetd: 配置校验失败，拒绝启动：");
            for e in errs {
                eprintln!("  - {e}");
            }
            std::process::exit(1);
        }
    };

    if std::io::stderr().is_terminal() {
        for w in agentbuffet::config::startup_warnings(&snap) {
            eprintln!("buffetd: [warn] {w}");
        }
    }

    let state = Arc::new(AppState::new(snap));
    let listen = state.snap.read().await.gateway_addr;
    let ui = state.snap.read().await.ui_addr;

    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("buffetd: 监听 {listen} 失败: {e}");
            std::process::exit(1);
        }
    };
    println!("buffetd 已启动: gateway http://{listen} (UI 将于 M4 提供: {ui})");

    let app = agentbuffet::build_router(state);
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("buffetd: 服务异常退出: {e}");
        std::process::exit(1);
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
