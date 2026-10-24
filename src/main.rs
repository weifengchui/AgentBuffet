//! AgentBuffet：单二进制入口。配置校验失败拒绝启动（报错带文件名）。
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
            eprintln!("AgentBuffet: 读取配置失败 {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let filename = path.display().to_string();
    match agentbuffet::config::load_str(&text, &filename) {
        Ok(s) => s,
        Err(errs) => {
            eprintln!("AgentBuffet: 配置校验失败：");
            for e in errs {
                eprintln!("  - {e}");
            }
            std::process::exit(1);
        }
    }
}

fn usage(code: i32) -> ! {
    eprintln!(
        "用法: AgentBuffet [serve]                     启动网关（默认）\n\
        \x20     AgentBuffet reload                     通知运行中的网关热重载配置\n\
        \x20     AgentBuffet agents detect|status       列出全部 agent 接入状态\n\
        \x20     AgentBuffet agents attach <id> [--group g] [--dry-run]\n\
        \x20     AgentBuffet agents detach <id>         还原（备份优先，逐字节）\n\
        \x20     AgentBuffet service install|uninstall|status  开机自启（systemd 用户单元 / macOS LaunchAgent / Windows 启动目录）\n\
        \x20     AgentBuffet config export [--out PATH]   导出当前配置（默认 stdout）\n\
        \x20     AgentBuffet config import PATH           导入整份配置（校验失败不落盘，原文件备份 .bak）"
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
            eprintln!("AgentBuffet: 未知命令: {other}");
            usage(2);
        }
    }
}

async fn serve() {
    let snap = load_snapshot();

    if std::io::stderr().is_terminal() {
        for w in agentbuffet::config::startup_warnings(&snap) {
            eprintln!("AgentBuffet: [warn] {w}");
        }
    }

    let logs_dir = agentbuffet_home().join("logs");
    let state = Arc::new(AppState::with_paths(snap, default_config_path(), logs_dir));
    // env 文件（~/.agentbuffet/env）自动载入进程环境：Key 只需写文件，无需启动方 source
    let loaded = agentbuffet::load_env_file(&state.env_file);
    if loaded > 0 {
        eprintln!(
            "AgentBuffet: 已从 {} 载入 {loaded} 个环境变量",
            state.env_file.display()
        );
    }
    let listen = state.snap.read().await.gateway_addr;
    let ui = state.snap.read().await.ui_addr;

    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("AgentBuffet: 监听 {listen} 失败: {e}");
            std::process::exit(1);
        }
    };
    let ui_listener = match tokio::net::TcpListener::bind(ui).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("AgentBuffet: 监听 {ui}（_admin）失败: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "AgentBuffet 已启动: gateway http://{listen}  admin http://{ui} (UI + POST /_admin/reload)"
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

    // 漂移巡检：手动改动 Agent 本地配置等场景下自动还原，无需人为操作
    {
        let st = state.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                iv.tick().await;
                let fixed = agentbuffet::repair_drifted(&st).await;
                if !fixed.is_empty() {
                    println!("已自动修复接入器配置漂移: {}", fixed.join(", "));
                }
            }
        });
    }

    // 启动自动拉取：models 为空的服务商（如 OpenRouter）自动补齐模型目录
    {
        let st = state.clone();
        tokio::spawn(async move { agentbuffet::auto_fetch_models(st).await; });
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
        eprintln!("AgentBuffet: 服务异常退出: {e}");
        std::process::exit(1);
    }
}

async fn reload_cmd() {
    let snap = load_snapshot();
    let url = format!("http://{}/_admin/reload", snap.ui_addr);
    let r = reqwest::Client::new().post(&url).send().await;
    match r {
        Err(e) => {
            eprintln!("AgentBuffet: 调用 {url} 失败: {e}");
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
                        eprintln!("AgentBuffet: 未知参数: {other}");
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
                default_group: g.clone(),
                groups: names.into_iter().cloned().collect(),
                models: agentbuffet::routable_models(&snap, &g),
                gateway_addr: snap.gateway_addr.to_string(),
                adapter: a.adapter.clone(),
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
            let hint = snap
                .agents
                .get(id)
                .map(|x| x.adapter.clone())
                .unwrap_or_default();
            match onboard::revert(&home, id, &hint) {
                Ok(r) => print_apply_result(&r),
                Err(e) => {
                    eprintln!("还原失败: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("AgentBuffet: 未知 agents 子命令: {sub}");
            usage(2);
        }
    }
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
                        eprintln!("AgentBuffet: 未知参数: {:?}", other.0);
                        std::process::exit(2);
                    }
                }
                i += 1;
            }
            let path = default_config_path();
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("AgentBuffet: 读取 {} 失败: {e}", path.display());
                    std::process::exit(1);
                }
            };
            match out {
                Some(o) => {
                    if let Err(e) = std::fs::write(&o, &text) {
                        eprintln!("AgentBuffet: 写入 {} 失败: {e}", o.display());
                        std::process::exit(1);
                    }
                    eprintln!("已导出 {} → {}", path.display(), o.display());
                }
                None => print!("{text}"),
            }
        }
        "import" => {
            let Some(src) = args.get(1) else {
                eprintln!("AgentBuffet: 用法: AgentBuffet config import PATH");
                std::process::exit(2);
            };
            let text = match std::fs::read_to_string(src) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("AgentBuffet: 读取 {src} 失败: {e}");
                    std::process::exit(1);
                }
            };
            let dst = default_config_path();
            if let Err(errs) = agentbuffet::config::load_str(&text, src) {
                eprintln!("AgentBuffet: 配置校验失败，未写入: {errs:?}");
                std::process::exit(1);
            }
            if dst.exists() {
                let bak = PathBuf::from(format!("{}.bak", dst.display()));
                if !bak.exists() {
                    if let Err(e) = std::fs::copy(&dst, &bak) {
                        eprintln!("AgentBuffet: 备份 {} 失败: {e}", bak.display());
                        std::process::exit(1);
                    }
                }
            }
            if let Some(dir) = dst.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(&dst, &text) {
                eprintln!("AgentBuffet: 写入 {} 失败: {e}", dst.display());
                std::process::exit(1);
            }
            println!("已导入配置 → {}", dst.display());
            println!("运行中的网关执行 AgentBuffet reload 即可热重载生效。");
        }
        other => {
            eprintln!("AgentBuffet: 用法: AgentBuffet config export [--out PATH] | AgentBuffet config import PATH（未知: {other}）");
            std::process::exit(2);
        }
    }
}

fn service_cmd(args: &[String]) {
    let home = onboard::home_dir();
    match args.first().map(|s| s.as_str()).unwrap_or("") {
        "install" => match agentbuffet::autostart::enable(&home) {
            Ok(s) => {
                println!("已启用开机自启（{}）", s.method);
                println!("  单元/脚本: {}", s.unit_path);
                if !s.link_path.is_empty() {
                    println!("  启用链接: {}", s.link_path);
                }
                println!("下次登录（开机）自动启动网关；真实 Key 请确保在 ~/.agentbuffet/env。");
            }
            Err(e) => {
                eprintln!("启用失败: {e}");
                std::process::exit(1);
            }
        },
        "uninstall" => match agentbuffet::autostart::disable(&home) {
            Ok(_) => println!("已关闭开机自启（单元/脚本与启用链接已删除）"),
            Err(e) => {
                eprintln!("关闭失败: {e}");
                std::process::exit(1);
            }
        },
        "status" => {
            let s = agentbuffet::autostart::status(&home);
            println!("方式: {}", s.method);
            println!("已启用: {}", if s.enabled { "是" } else { "否" });
            println!("单元/脚本: {}", s.unit_path);
            if !s.link_path.is_empty() {
                println!("启用链接: {}", s.link_path);
            }
        }
        other => {
            eprintln!("AgentBuffet: 未知 service 子命令: {other}");
            usage(2);
        }
    }
}
