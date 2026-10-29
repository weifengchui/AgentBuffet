//! 巡检让位的回归测试：文件里的 base_url 指向另一个**活着的** AgentBuffet
//! 网关时，`repair_drifted` 必须跳过改写（否则两个网关互相覆盖、永远打架）；
//! 指向死地址 / 别家 LLM 服务时仍要照常拉回本网关。

use agentbuffet::config;
use agentbuffet::onboard::{self, Target};
use agentbuffet::proxy::AppState;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_home(tag: &str) -> PathBuf {
    agentbuffet::onboard::enable_isolation_for_tests();
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!(
        "buffet-drift-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cfg_text(gw_addr: &str) -> String {
    format!(
        r#"gateway:
  addr: "{gw_addr}"
  ui_addr: "127.0.0.1:0"
providers:
  - id: p1
    vendor: mock
    base_url: "http://127.0.0.1:1/v1"
    api_key_env: K1
    allow_private: true
    models: [m-a]
routing_groups:
  - name: g
    strategy: order
    members:
      - {{ provider: p1, model: m-a, priority: 1 }}
agents:
  - {{ id: hermes, default_group: g }}
"#
    )
}

fn state(home: &Path, gw_addr: &str) -> Arc<AppState> {
    let text = cfg_text(gw_addr);
    let cfg = home.join("config.yaml");
    std::fs::write(&cfg, &text).unwrap();
    std::fs::write(home.join("env"), "K1=secret-value\n").unwrap();
    let snap = config::load_str(&text, "test.yaml").expect("config ok");
    Arc::new(AppState {
        home: home.to_path_buf(),
        env_file: home.join("env"),
        ..AppState::with_paths(snap, cfg, home.join("logs"))
    })
}

/// 保持存活但不提供服务的监听：用作"本网关地址"，保证端口不被复用。
fn reserved_addr() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap().to_string();
    (l, a)
}

/// 假 AgentBuffet 网关（`/v1/models` 回 `owned_by: agentbuffet`）。
fn fake_gateway() -> String {
    use std::io::{Read, Write};
    let lst = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in lst.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf);
            let body = r#"{"object":"list","data":[{"id":"group/g","owned_by":"agentbuffet"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        }
    });
    addr
}

/// 别家的 OpenAI 兼容服务。
fn fake_plain_llm() -> String {
    use std::io::{Read, Write};
    let lst = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in lst.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf);
            let body = r#"{"object":"list","data":[{"id":"gpt-4o","owned_by":"openai"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        }
    });
    addr
}

fn dead_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().to_string()
}

fn hermes_target(gw_addr: &str) -> Target {
    Target {
        agent_id: "hermes".to_string(),
        default_group: "g".to_string(),
        groups: vec!["g".to_string()],
        models: vec![],
        gateway_addr: gw_addr.to_string(),
        adapter: "hermes".to_string(),
    }
}

/// 用给定网关地址真实接入一次，得到指向它的 hermes 配置。
fn attach_hermes(home: &Path, gw_addr: &str) -> String {
    let cfg = home.join(".hermes").join("config.yaml");
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(&cfg, "model:\n  provider: x\n  base_url: \"http://127.0.0.1:1/v1\"\n").unwrap();
    let target = hermes_target(gw_addr);
    onboard::apply(home, &target).unwrap();
    std::fs::read_to_string(&cfg).unwrap()
}

/// 核心场景：被另一个活着的网关占着 → 巡检让位，文件逐字节保留。
#[tokio::test]
async fn repair_skips_agent_held_by_live_other_gateway() {
    let other_gw = fake_gateway();
    let home = temp_home("yield");
    let hermes = home.join(".hermes").join("config.yaml");
    let before = attach_hermes(&home, &other_gw);
    assert!(before.contains(&other_gw), "前置：配置应指向另一个网关");

    let (_hold, ours) = reserved_addr();
    let st = state(&home, &ours);
    let fixed = agentbuffet::repair_drifted(&st).await;

    assert!(fixed.is_empty(), "被占用的实例不应被改写: {fixed:?}");
    assert_eq!(
        std::fs::read_to_string(&hermes).unwrap(),
        before,
        "文件必须原样保留"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 对照场景：地址只是不可达（网关已停 / 写错地址）→ 仍照常修复回本网关。
#[tokio::test]
async fn repair_still_fixes_drift_to_dead_address() {
    let dead = dead_addr();
    let home = temp_home("fix");
    let hermes = home.join(".hermes").join("config.yaml");
    attach_hermes(&home, &dead);

    let (_hold, ours) = reserved_addr();
    let st = state(&home, &ours);
    let fixed = agentbuffet::repair_drifted(&st).await;

    assert_eq!(fixed, vec!["hermes".to_string()]);
    let after = std::fs::read_to_string(&hermes).unwrap();
    assert!(
        after.contains(&format!("http://{ours}/hermes/v1")),
        "应改回本网关: {after}"
    );
    assert!(!after.contains(&dead), "不应再指向死地址: {after}");
    let _ = std::fs::remove_dir_all(&home);
}

/// 对照场景：文件指向别家 LLM 服务（活着，但不是本网关家族）→ 照常修复。
#[tokio::test]
async fn repair_fixes_drift_to_foreign_llm_service() {
    let llm = fake_plain_llm();
    let home = temp_home("llm");
    let hermes = home.join(".hermes").join("config.yaml");
    attach_hermes(&home, &llm);

    let (_hold, ours) = reserved_addr();
    let st = state(&home, &ours);
    let fixed = agentbuffet::repair_drifted(&st).await;

    assert_eq!(fixed, vec!["hermes".to_string()], "别家服务不算被占用");
    let after = std::fs::read_to_string(&hermes).unwrap();
    assert!(after.contains(&format!("http://{ours}/hermes/v1")), "{after}");
    let _ = std::fs::remove_dir_all(&home);
}

/// 已一致时不做任何写入（幂等），返回空列表。
#[tokio::test]
async fn repair_noop_when_already_consistent() {
    let home = temp_home("noop");
    let hermes = home.join(".hermes").join("config.yaml");
    let (_hold, ours) = reserved_addr();
    let before = attach_hermes(&home, &ours);

    let st = state(&home, &ours);
    let fixed = agentbuffet::repair_drifted(&st).await;

    assert!(fixed.is_empty(), "{fixed:?}");
    assert_eq!(std::fs::read_to_string(&hermes).unwrap(), before);
    let _ = std::fs::remove_dir_all(&home);
}
