//! buffetd：单二进制入口。配置校验失败拒绝启动（报错带文件名）。
//! M3：gateway + ui_addr(_admin) 双监听，用量记账落 ~/.agentbuffet/logs。
//! M4：CLI 子命令（reload / agents / service）+ systemd 用户级自启单元（只写文件，不 spawn 进程）。

use agentbuffet::onboard;
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

fn load_snapshot() -> agentbuffet::config::Snapshot {
    let path = default_config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("buffetd: 读取配置失败 {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let filename = path.display().to_string();
    match agentbuffet::config::load_str(&text, &filename) {
        Ok(s) => s,
        Err(errs) => {
            eprintln!("buffetd: 配置校验失败：");
            for e in errs {
                eprintln!("  - {e}");
            }
            std::process::exit(1);
        }
    }
}

fn usage(code: i32) -> ! {
    eprintln!(
        "用法: buffetd [serve]                     启动网关（默认）\n\
        \x20     buffetd reload                     通知运行中的网关热重载配置\n\
        \x20     buffetd agents detect|status       列出全部 agent 接入状态\n\
        \x20     buffetd agents attach <id> [--group g] [--dry-run]\n\
        \x20     buffetd agents detach <id>         还原（备份优先，逐字节）\n\
        \x20     buffetd service install|uninstall  写/删 systemd 用户级自启单元\n\
        \x20     buffetd config export [--out PATH]   导出当前配置（默认 stdout）\n\
        \x20     buffetd config import PATH           导入整份配置（校验失败不落盘，原文件备份 .bak）"
    );
    std::process::exit(code);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("serve");
    match cmd {
        "serve" => serve().await,
        "reload" => reload_cmd().await,
        "agents" => agents_cmd(&args[1..]),
        "service" => service_cmd(&args[1..]),
        "config" => config_cmd(&args[1..]),
        "help" | "-h" | "--help" => usage(0),
        other => {
            eprintln!("buffetd: 未知命令: {other}");
            usage(2);
        }
    }
}

async fn serve() {
    let snap = load_snapshot();

    if std::io::stderr().is_terminal() {
        for w in agentbuffet::config::startup_warnings(&snap) {
            eprintln!("buffetd: [warn] {w}");
        }
    }

    let logs_dir = agentbuffet_home().join("logs");
    let state = Arc::new(AppState::with_paths(snap, default_config_path(), logs_dir));
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
    println!(
        "buffetd 已启动: gateway http://{listen}  admin http://{ui} (UI + POST /_admin/reload)"
    );

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

async fn reload_cmd() {
    let snap = load_snapshot();
    let url = format!("http://{}/_admin/reload", snap.ui_addr);
    let r = reqwest::Client::new().post(&url).send().await;
    match r {
        Err(e) => {
            eprintln!("buffetd: 调用 {url} 失败: {e}");
            std::process::exit(1);
        }
        Ok(resp) => {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            println!("{code} {body}");
            if !code.is_success() {
                std::process::exit(1);
            }
        }
    }
}

fn print_apply_result(r: &onboard::ApplyResult) {
    for f in &r.files {
        println!("文件: {}", f.path);
        println!("{}", f.preview);
    }
    if let Some(t) = &r.env_template {
        println!("环境变量模板（不写文件）:\n{t}");
    }
    println!("说明: {}", r.note);
}

fn agents_cmd(args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let snap = load_snapshot();
    let home = onboard::home_dir();
    match sub {
        "detect" | "status" => {
            let targets = agentbuffet::agent_targets(&snap);
            let list = onboard::status_all(&home, &targets);
            if list.is_empty() {
                println!("配置中无 agent");
                return;
            }
            for a in list {
                let st = if a.attached {
                    if a.drifted {
                        "已接入(漂移)"
                    } else {
                        "已接入"
                    }
                } else {
                    "未接入"
                };
                println!("{:<14} {:<12} {:<10} {}", a.agent_id, a.adapter, st, a.note);
            }
        }
        "attach" => {
            let Some(id) = args.get(1) else {
                eprintln!("agents attach 需要 <agent_id>");
                usage(2);
            };
            let mut group: Option<String> = None;
            let mut dry = false;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--group" => {
                        group = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--dry-run" => {
                        dry = true;
                        i += 1;
                    }
                    other => {
                        eprintln!("buffetd: 未知参数: {other}");
                        usage(2);
                    }
                }
            }
            let Some(a) = snap.agents.get(id) else {
                eprintln!("配置中不存在 agent: {id}");
                std::process::exit(1);
            };
            let g = group.unwrap_or_else(|| a.default_group.clone());
            if !snap.groups.contains_key(&g) {
                eprintln!("路由组不存在: {g}");
                std::process::exit(1);
            }
            let mut names: Vec<&String> = snap.groups.keys().collect();
            names.sort();
            let target = onboard::Target {
                agent_id: id.clone(),
                default_group: g,
                groups: names.into_iter().cloned().collect(),
                gateway_addr: snap.gateway_addr.to_string(),
            };
            let res = if dry {
                onboard::plan(&home, &target)
            } else {
                onboard::apply(&home, &target)
            };
            match res {
                Ok(r) => print_apply_result(&r),
                Err(e) => {
                    eprintln!("接入失败: {e}");
                    std::process::exit(1);
                }
            }
        }
        "detach" => {
            let Some(id) = args.get(1) else {
                eprintln!("agents detach 需要 <agent_id>");
                usage(2);
            };
            match onboard::revert(&home, id) {
                Ok(r) => print_apply_result(&r),
                Err(e) => {
                    eprintln!("还原失败: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("buffetd: 未知 agents 子命令: {sub}");
            usage(2);
        }
    }
}

fn unit_path() -> PathBuf {
    onboard::home_dir()
        .join(".config")
        .join("systemd")
        .join("user")
        .join("agentbuffet.service")
}

fn config_cmd(args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "export" => {
            let mut out: Option<PathBuf> = None;
            let mut i = 1;
            while i < args.len() {
                match (args[i].as_str(), args.get(i + 1)) {
                    ("--out", Some(v)) => {
                        out = Some(PathBuf::from(v));
                        i += 1;
                    }
                    other => {
                        eprintln!("buffetd: 未知参数: {:?}", other.0);
                        std::process::exit(2);
                    }
                }
                i += 1;
            }
            let path = default_config_path();
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("buffetd: 读取 {} 失败: {e}", path.display());
                    std::process::exit(1);
                }
            };
            match out {
                Some(o) => {
                    if let Err(e) = std::fs::write(&o, &text) {
                        eprintln!("buffetd: 写入 {} 失败: {e}", o.display());
                        std::process::exit(1);
                    }
                    eprintln!("已导出 {} → {}", path.display(), o.display());
                }
                None => print!("{text}"),
            }
        }
        "import" => {
            let Some(src) = args.get(1) else {
                eprintln!("buffetd: 用法: buffetd config import PATH");
                std::process::exit(2);
            };
            let text = match std::fs::read_to_string(src) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("buffetd: 读取 {src} 失败: {e}");
                    std::process::exit(1);
                }
            };
            let dst = default_config_path();
            if let Err(errs) = agentbuffet::config::load_str(&text, src) {
                eprintln!("buffetd: 配置校验失败，未写入: {errs:?}");
                std::process::exit(1);
            }
            if dst.exists() {
                let bak = PathBuf::from(format!("{}.bak", dst.display()));
                if !bak.exists() {
                    if let Err(e) = std::fs::copy(&dst, &bak) {
                        eprintln!("buffetd: 备份 {} 失败: {e}", bak.display());
                        std::process::exit(1);
                    }
                }
            }
            if let Some(dir) = dst.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(&dst, &text) {
                eprintln!("buffetd: 写入 {} 失败: {e}", dst.display());
                std::process::exit(1);
            }
            println!("已导入配置 → {}", dst.display());
            println!("运行中的网关执行 buffetd reload 即可热重载生效。");
        }
        other => {
            eprintln!("buffetd: 用法: buffetd config export [--out PATH] | buffetd config import PATH（未知: {other}）");
            std::process::exit(2);
        }
    }
}

fn service_cmd(args: &[String]) {
    match args.first().map(|s| s.as_str()).unwrap_or("") {
        "install" => {
            let p = unit_path();
            if let Some(dir) = p.parent() {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    eprintln!("创建 {} 失败: {e}", dir.display());
                    std::process::exit(1);
                }
            }
            let exe = std::env::current_exe()
                .map(|x| x.display().to_string())
                .unwrap_or_else(|_| "buffetd".into());
            let env_file = agentbuffet_home().join("env");
            let unit = format!(
                "[Unit]\nDescription=AgentBuffet AI gateway\n\
                 [Service]\nExecStart={exe} serve\nRestart=on-failure\n\
                 EnvironmentFile=-{env}\n\
                 [Install]\nWantedBy=default.target\n",
                env = env_file.display()
            );
            if let Err(e) = std::fs::write(&p, unit) {
                eprintln!("写入 {} 失败: {e}", p.display());
                std::process::exit(1);
            }
            println!("已写入自启单元: {}", p.display());
            println!(
                "真实 Key 请放入 {}（600 权限），然后手动执行：",
                env_file.display()
            );
            println!("  systemctl --user daemon-reload && systemctl --user enable --now agentbuffet.service");
        }
        "uninstall" => {
            let p = unit_path();
            match std::fs::remove_file(&p) {
                Ok(_) => println!("已删除 {}", p.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("{} 不存在，无需删除", p.display())
                }
                Err(e) => {
                    eprintln!("删除 {} 失败: {e}", p.display());
                    std::process::exit(1);
                }
            }
            println!("如已启用，请手动执行：systemctl --user disable --now agentbuffet.service");
        }
        other => {
            eprintln!("buffetd: 未知 service 子命令: {other}");
            usage(2);
        }
    }
}
