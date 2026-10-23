//! buffetd：单二进制入口。配置校验失败拒绝启动（报错带文件名）。
//! M3：gateway + ui_addr(_admin) 双监听，用量记账落 ~/.agentbuffet/logs。

use agentbuffet::proxy::AppState;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;

fn agentbuffet_home() -> PathBuf {
    if let Ok(h) = std::env::var("AGENTBUFFET_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".agentbuffet")
}

fn default_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("AGENTBUFFET_CONFIG") {
        return PathBuf::from(p);
    }
    agentbuffet_home().join("config.yaml")
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

    let logs_dir = agentbuffet_home().join("logs");
    let state = Arc::new(AppState::with_paths(snap, path, logs_dir));
    let listen = state.snap.read().await.gateway_addr;
    let ui = state.snap.read().await.ui_addr;

    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("buffetd: 监听 {listen} 失败: {e}");
            std::process::exit(1);
        }
    };
    let ui_listener = match tokio::net::TcpListener::bind(ui).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("buffetd: 监听 {ui}（_admin）失败: {e}");
            std::process::exit(1);
        }
    };
    println!("buffetd 已启动: gateway http://{listen}  admin http://{ui} (POST /_admin/reload)");

    // ctrl_c 一次触发，两个监听各自优雅退出
    let shutdown = Arc::new(Notify::new());
    {
        let n = shutdown.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            n.notify_waiters();
        });
    }

    let gateway_app = agentbuffet::build_router(state.clone());
    let admin_app = agentbuffet::build_admin_router(state.clone());

    let gw = {
        let n = shutdown.clone();
        axum::serve(listener, gateway_app).with_graceful_shutdown(async move { n.notified().await })
    };
    let adm = {
        let n = shutdown.clone();
        axum::serve(
            ui_listener,
            admin_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { n.notified().await })
    };

    if let Err(e) = tokio::try_join!(gw, adm) {
        eprintln!("buffetd: 服务异常退出: {e}");
        std::process::exit(1);
    }
}
