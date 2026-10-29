//! 运行环境（Scope）：本机 + 其它环境（WSL 发行版 / Windows 侧用户）。
//!
//! 参照 Magpie（yetone/magpie，`internal/agent/wsl.go`）：装在 WSL 发行版里的 Agent
//! 与 Windows 本机的同名 Agent 是**两个独立实例**——配置各写各的，网关侧的用量记录
//! 也分开统计。Magpie 用 `<id>@wsl:<distro>`（如 `codex@wsl:Ubuntu`）区分，
//! 本模块沿用同一套编码：
//!
//! - 本机：`hermes`
//! - WSL Ubuntu：`hermes@wsl:Ubuntu`
//! - Windows 侧用户（AgentBuffet 跑在 WSL 里时可见）：`hermes@win:<user>`
//!
//! 枚举是**双向对称**的：网关跑在哪一侧，就枚举另一侧。
//! - 跑在 Windows：读注册表枚举 WSL 发行版（`\\wsl.localhost\<distro>`）。
//! - 跑在 Linux/WSL：读 `/mnt/<盘>/Users/<用户>` 枚举 Windows 侧用户。
//!
//! 两侧的 Agent 各自独立，互不覆盖。
//!
//! Agent id 里带 `@` 后缀即表示"某个环境里的实例"，`split_agent_id` 可还原出
//! 基础类型与 scope id；`scope_of` 用于反查该实例所在的运行环境。
//!
//! 硬约束（沿用接入器的安全护栏）：
//! - 只读文件系统 + 注册表，**不启动任何外部进程**（不跑 wsl.exe，避免把停着的
//!   发行版拉起来；也不跑 ip/host 命令）。
//! - WSL 发行版名取注册表 `Lxss\DistributionName` 的原样大小写：`\\wsl.localhost\`
//!   根目录无法枚举，且发行版名大小写敏感。
//! - Linux 侧枚举 Windows 用户时，只认真实存在的用户目录（有 `.hermes`/`.codex`
//!   等 Agent 配置之一），不把 `Public`/`Default` 之类算进来。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 本机（原生环境）的 scope id。
pub const NATIVE: &str = "native";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 网关自己所在的环境。
    Native,
    /// WSL 发行版（网关跑在 Windows 侧时可见）。
    Wsl,
    /// Windows 侧的用户目录（网关跑在 WSL/Linux 侧时可见）。
    Windows,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Native => "native",
            Kind::Wsl => "wsl",
            Kind::Windows => "windows",
        }
    }
}

/// 一个运行环境：Agent 配置所在的主目录 + 网关在该环境下的可达地址。
#[derive(Debug, Clone)]
pub struct Scope {
    /// 稳定 id：`native` / `wsl:Ubuntu` / `wsl:Ubuntu~root` / `win:yxf`
    pub id: String,
    pub kind: Kind,
    /// Agent 配置所在主目录（Windows 侧路径）
    pub home: PathBuf,
    /// WSL 发行版名（本机为 None）
    pub distro: Option<String>,
    /// Windows 侧用户名（Linux 侧枚举到 Windows 用户目录时为 Some）
    pub user: Option<String>,
    /// WSL 根目录（`\\wsl.localhost\Ubuntu`），用于读 etc/resolv.conf
    pub root: Option<PathBuf>,
    /// 该环境下网关的主机名修正：WSL 里 127.0.0.1 不可达时改用 Windows 主机 IP
    pub gw_host: Option<String>,
    /// 人类可读的补充说明（如"WSL 未开启 localhost 转发"）
    pub note: String,
}

impl Scope {
    /// 展示名（本机 → "本机 (Windows)"；WSL → "WSL · Ubuntu"；Windows 侧 → "Windows · yxf"）。
    pub fn label(&self) -> String {
        match self.kind {
            Kind::Native => native_label(),
            Kind::Wsl => format!("WSL · {}", self.distro.clone().unwrap_or_default()),
            Kind::Windows => format!("Windows · {}", self.user.clone().unwrap_or_default()),
        }
    }

    pub fn is_native(&self) -> bool {
        self.kind == Kind::Native
    }
}

fn native_label() -> String {
    match std::env::consts::OS {
        "windows" => "本机 (Windows)".to_string(),
        "macos" => "本机 (macOS)".to_string(),
        "linux" => "本机 (Linux)".to_string(),
        other => format!("本机 ({other})"),
    }
}

// ---------------------------------------------------------------- id 编解码

/// 基础类型 + scope id → 实例 id。
/// `("hermes", "native")` → `hermes`；`("hermes", "wsl:Ubuntu")` → `hermes@wsl:Ubuntu`。
pub fn agent_id_for(kind: &str, scope_id: &str) -> String {
    if scope_id.is_empty() || scope_id == NATIVE {
        kind.to_string()
    } else {
        format!("{kind}@{scope_id}")
    }
}

/// 实例 id → (基础类型, scope id)。无 `@` 后缀即本机。
pub fn split_agent_id(agent_id: &str) -> (&str, Option<&str>) {
    match agent_id.split_once('@') {
        Some((kind, scope)) if !kind.is_empty() && !scope.is_empty() => (kind, Some(scope)),
        _ => (agent_id, None),
    }
}

/// 实例 id 所属的 scope id（无后缀 → 本机）。
pub fn scope_of(agent_id: &str) -> &str {
    split_agent_id(agent_id).1.unwrap_or(NATIVE)
}

/// 实例 id 的基础类型（去掉 `@scope` 后缀）。
pub fn kind_of(agent_id: &str) -> &str {
    split_agent_id(agent_id).0
}

/// 把任意发行版名压成可用于 id 的 slug（保留大小写与 `-` `.` `_`）。
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "wsl".to_string()
    } else {
        out
    }
}

// ---------------------------------------------------------------- 枚举环境

/// 本机环境。
pub fn native_scope(home: &Path) -> Scope {
    Scope {
        id: NATIVE.to_string(),
        kind: Kind::Native,
        home: home.to_path_buf(),
        distro: None,
        user: None,
        root: None,
        gw_host: None,
        note: String::new(),
    }
}

/// 全部环境：本机 + 其它可见环境。
///
/// - 网关在 Windows 上：额外是各个 WSL 发行版。
/// - 网关在 Linux/WSL 上：额外是 Windows 侧的用户目录。
pub fn enumerate(home: &Path) -> Vec<Scope> {
    let mut v = vec![native_scope(home)];
    // 隔离实例只认自己的主目录：不枚举 WSL / Windows 侧的真实用户目录，
    // 否则测试进程会把用户真实的 WSL 配置也当成目标。
    if !crate::onboard::agent_home_isolated() {
        v.extend(other_scopes());
    }
    v
}

/// 枚举缓存：(主目录, 缓存时刻, 枚举结果)。
type ScopeCache = Mutex<Option<(String, Instant, Vec<Scope>)>>;

/// 带缓存的枚举：扫描要读注册表与 UNC 目录，短时间内重复调用直接复用结果。
///
/// 缓存按主目录分桶，TTL 20s——UI 反复刷新与 30s 一次的漂移巡检都不会反复打盘。
pub fn enumerate_cached(home: &Path) -> Vec<Scope> {
    static CACHE: OnceLock<ScopeCache> = OnceLock::new();
    const TTL: Duration = Duration::from_secs(20);
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let key = home.display().to_string();
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some((k, at, scopes)) = guard.as_ref() {
        if k == &key && at.elapsed() < TTL {
            return scopes.clone();
        }
    }
    let scopes = enumerate(home);
    *guard = Some((key, Instant::now(), scopes.clone()));
    scopes
}

/// 按 id 取 scope。
pub fn scope_by_id<'a>(scopes: &'a [Scope], id: &str) -> Option<&'a Scope> {
    scopes.iter().find(|s| s.id == id)
}

/// scope id → 该环境的主目录。找不到返回 None（如发行版已卸载）。
pub fn home_for_scope(scope_id: &str, native_home: &Path) -> Option<PathBuf> {
    if scope_id == NATIVE {
        return Some(native_home.to_path_buf());
    }
    enumerate_cached(native_home)
        .into_iter()
        .find(|s| s.id == scope_id)
        .map(|s| s.home)
}

// ---------------------------------------------------------------- WSL

/// 网关自己所在环境之外的可见环境。
#[cfg(windows)]
fn other_scopes() -> Vec<Scope> {
    wsl_scopes()
}

/// 网关跑在 Linux/WSL 里时，另一侧是 Windows：读 `/mnt/<盘>/Users/<用户>`。
#[cfg(not(windows))]
fn other_scopes() -> Vec<Scope> {
    windows_scopes()
}

#[cfg(windows)]
fn wsl_scopes() -> Vec<Scope> {
    let mut out = Vec::new();
    for name in distro_names() {
        let Some(root) = wsl_root(&name) else {
            continue;
        };
        let homes = wsl_homes(&root);
        for (i, (user, home)) in homes.iter().enumerate() {
            // 同一发行版内多个用户：首个用 `wsl:<distro>`，其余用 `wsl:<distro>~<user>`
            let id = if i == 0 {
                format!("wsl:{}", slug(&name))
            } else {
                format!(
                    "wsl:{}~{}",
                    slug(&name),
                    slug(user.as_deref().unwrap_or("user"))
                )
            };
            let (gw_host, note) = wsl_gateway_host(&root);
            out.push(Scope {
                id,
                kind: Kind::Wsl,
                home: home.clone(),
                distro: Some(name.clone()),
                user: None,
                root: Some(root.clone()),
                gw_host,
                note,
            });
        }
    }
    out
}

/// Windows 侧的用户目录（网关跑在 WSL/Linux 里时可见）。
///
/// 扫 `/mnt/<盘>/Users/<用户>`：只认**装了 Agent 的用户目录**（存在 `.hermes`/
/// `.codex`/`.config/opencode` 之一），避免把 `Public`、`Default`、`All Users`
/// 之类也算进来——它们不是可接入的运行环境。
#[cfg(not(windows))]
fn windows_scopes() -> Vec<Scope> {
    let mut out = Vec::new();
    for drive in mounted_windows_drives() {
        let users_dir = drive.join("Users");
        for (user, home) in scan_agent_users(&users_dir) {
            // 网关在 WSL 里，Windows 侧要用 WSL 的地址访问它
            let (gw_host, note) = wsl_host_ip_for_windows();
            out.push(Scope {
                id: format!("win:{}", slug(&user)),
                kind: Kind::Windows,
                home,
                distro: None,
                user: Some(user),
                root: None,
                gw_host,
                note,
            });
        }
    }
    out
}

/// 扫 `Users` 目录，挑出**装了 Agent 的用户**（按用户名排序）。
///
/// 平台无关（Windows 侧也能测）：`Public`/`Default` 这类没装 Agent 的目录
/// 不算运行环境，会被过滤掉。
#[allow(dead_code)] // Windows 构建下只在 Linux 侧的枚举里用到
fn scan_agent_users(users_dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(users_dir) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = rd.flatten().filter(|e| e.path().is_dir()).collect();
    entries.sort_by_key(|e| e.file_name());
    entries
        .into_iter()
        .map(|e| {
            let user = e.file_name().to_string_lossy().into_owned();
            (user, e.path())
        })
        .filter(|(_, home)| has_agent_config(home))
        .collect()
}

/// Linux/WSL 里挂载的 Windows 盘（`/mnt/c`、`/mnt/d` …）。
///
/// 挂载根目录可由 `AGENTBUFFET_WSL_MNT` 覆盖（默认 `/mnt`）——自建挂载点或
/// 测试环境用得上。
#[cfg(not(windows))]
fn mounted_windows_drives() -> Vec<PathBuf> {
    let root = std::env::var("AGENTBUFFET_WSL_MNT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/mnt"));
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&root) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        // 形如 /mnt/c 的单字母目录才是 Windows 盘（/mnt/wsl、/mnt/wslg 不是）
        let name = e.file_name().to_string_lossy().into_owned();
        if name.len() == 1 && name.chars().all(|c| c.is_ascii_alphabetic()) && p.is_dir() {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// 该目录下是否装了任何受支持的 Agent（用于过滤 Windows 侧无效用户目录）。
#[allow(dead_code)] // Windows 构建下只在 Linux 侧的枚举里用到
fn has_agent_config(home: &Path) -> bool {
    home.join(".hermes").is_dir()
        || home.join(".codex").is_dir()
        || home.join(".config").join("opencode").is_dir()
        || home.join(".claude.json").is_file()
}

/// 注册表 `HKCU\...\Lxss` 下各发行版的 DistributionName（原样大小写）。
#[cfg(windows)]
fn distro_names() -> Vec<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let mut out = Vec::new();
    let Ok(lxss) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Lxss")
    else {
        return out;
    };
    for key in lxss.enum_keys().flatten() {
        if let Ok(sub) = lxss.open_subkey(&key) {
            if let Ok(name) = sub.get_value::<String, _>("DistributionName") {
                if !name.is_empty() {
                    out.push(name);
                }
            }
        }
    }
    out
}

/// 发行版的 Windows 侧根目录：`\\wsl.localhost\<name>` 优先，其次 `\\wsl$\<name>`。
#[cfg(windows)]
fn wsl_root(name: &str) -> Option<PathBuf> {
    for base in [
        format!("\\\\wsl.localhost\\{name}"),
        format!("\\\\wsl$\\{name}"),
    ] {
        let root = PathBuf::from(&base);
        if root.is_dir() {
            return Some(root);
        }
    }
    None
}

/// 发行版内的用户主目录：`/home/<user>` 优先，其次 `/root`。
#[cfg(windows)]
fn wsl_homes(root: &Path) -> Vec<(Option<String>, PathBuf)> {
    let mut out: Vec<(Option<String>, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root.join("home")) {
        let mut users: Vec<(String, PathBuf)> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect();
        users.sort_by(|a, b| a.0.cmp(&b.0));
        for (u, p) in users {
            out.push((Some(u), p));
        }
    }
    let root_home = root.join("root");
    if root_home.is_dir() {
        out.push((None, root_home));
    }
    out
}

/// WSL 里访问网关的主机修正 + 提示。
///
/// - 镜像网络（`networkingMode=mirrored`）：WSL 的 127.0.0.1 就是 Windows 的，
///   直接用回环地址。
/// - 默认 NAT：`localhostForwarding=true`（默认）时 WSL 里的 127.0.0.1 会转发到
///   Windows 主机，同样可用回环地址。
/// - 显式关闭了 localhost 转发：回环不可达，改用发行版看到的 Windows 主机 IP
///   （取自 etc/resolv.conf 的 nameserver），此时网关需监听回环之外。
#[cfg(windows)]
fn wsl_gateway_host(root: &Path) -> (Option<String>, String) {
    let cfg = WslConfig::read();
    if cfg.mirrored() || cfg.localhost_forwarding() {
        return (None, String::new());
    }
    match wsl_host_ip(root) {
        Some(ip) => (
            Some(ip.clone()),
            format!("该发行版未开启 localhost 转发，已改用 Windows 主机 IP {ip}；请把网关监听地址改为 0.0.0.0:3425（并设 allow_loopback_only=false）"),
        ),
        None => (
            None,
            "该发行版未开启 localhost 转发，WSL 内可能无法访问 127.0.0.1 上的网关；请检查 .wslconfig".to_string(),
        ),
    }
}

#[cfg(windows)]
fn wsl_host_ip(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("etc").join("resolv.conf")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("nameserver") {
            let ip = rest.trim();
            if !ip.is_empty() && ip.parse::<std::net::IpAddr>().is_ok() {
                return Some(ip.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------- .wslconfig

/// `%USERPROFILE%\.wslconfig` 中与可达性相关的两项。
#[derive(Debug, Clone, Default)]
pub struct WslConfig {
    pub networking_mode: String,
    pub localhost_forwarding: Option<bool>,
}

impl WslConfig {
    /// 读取本机 .wslconfig（不存在或读失败时返回默认值）。
    pub fn read() -> WslConfig {
        let mut c = WslConfig::default();
        let Some(home) = crate::onboard::home_dir_opt() else {
            return c;
        };
        let Ok(text) = std::fs::read_to_string(home.join(".wslconfig")) else {
            return c;
        };
        let mut in_wsl2 = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                in_wsl2 = line.trim_start_matches('[').trim_end_matches(']').trim() == "wsl2";
                continue;
            }
            if !in_wsl2 {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim().to_ascii_lowercase().as_str() {
                "networkingmode" => c.networking_mode = v.trim().to_ascii_lowercase(),
                "localhostforwarding" => {
                    c.localhost_forwarding = match v.trim().to_ascii_lowercase().as_str() {
                        "true" | "1" | "yes" => Some(true),
                        "false" | "0" | "no" => Some(false),
                        _ => None,
                    }
                }
                _ => {}
            }
        }
        c
    }

    /// 是否镜像网络（WSL 的 127.0.0.1 即 Windows 的）。
    pub fn mirrored(&self) -> bool {
        self.networking_mode == "mirrored"
    }

    /// localhost 转发是否开启（默认开启）。
    pub fn localhost_forwarding(&self) -> bool {
        self.localhost_forwarding.unwrap_or(true)
    }
}

// ---------------------------------------------------------------- 网关地址

/// 该实例所在环境访问网关应使用的 `host:port`。
///
/// - 本机：直接用配置的监听地址。
/// - WSL（网关在 Windows 侧）：镜像网络或开启了 localhost 转发时同为回环；
///   否则换成发行版看到的 Windows 主机 IP（网关需监听回环之外）。
/// - Windows 侧（网关在 WSL 里）：WSL 的 127.0.0.1 与 Windows 的不是同一个，
///   必须换成 Windows 看到的 WSL 地址（WSL2 的 eth0 IP，或镜像网络下的回环）。
pub fn gateway_addr_for(agent_id: &str, gateway_addr: &str, native_home: &Path) -> String {
    let scope_id = scope_of(agent_id);
    if scope_id == NATIVE {
        return gateway_addr.to_string();
    }
    let scopes = enumerate_cached(native_home);
    let Some(sc) = scope_by_id(&scopes, scope_id) else {
        return gateway_addr.to_string();
    };
    let Some(host) = sc.gw_host.as_deref() else {
        return gateway_addr.to_string();
    };
    let port = gateway_addr
        .rsplit_once(':')
        .map(|(_, p)| p.to_string())
        .unwrap_or_else(|| "3425".to_string());
    format!("{host}:{port}")
}

/// 网关跑在 WSL 里时，Windows 侧该用哪个地址访问它。
///
/// WSL2 默认 NAT 网络下，Windows 访问 WSL 里的服务要用 WSL 的 eth0 IP；
/// 镜像网络（`networkingMode=mirrored`）下则两边回环互通，用 127.0.0.1 即可。
#[cfg(not(windows))]
fn wsl_host_ip_for_windows() -> (Option<String>, String) {
    // 读 /proc/net/route 找默认网关所在网卡，再取该网卡的 IPv4——纯文件读取，
    // 不启动任何外部进程（与 scope 枚举的硬约束一致）。
    let Some(ip) = wsl_eth0_ip() else {
        return (
            None,
            "未能读到 WSL 的 eth0 地址，Windows 侧可能无法访问本网关；请改用 localhost 转发或手动指定监听地址".to_string(),
        );
    };
    (
        Some(ip.clone()),
        format!("网关运行在 WSL 内，Windows 侧请通过 {ip} 访问（WSL2 NAT 网络）"),
    )
}

/// WSL 的 eth0 IPv4 地址（从 /proc/net/fib_trie 或 `ip addr` 的替代来源读取）。
#[cfg(not(windows))]
fn wsl_eth0_ip() -> Option<String> {
    // /proc/net/fib_trie 里含 LOCAL 段的本机地址，取默认路由网卡对应的那个。
    let text = std::fs::read_to_string("/proc/net/fib_trie").ok()?;
    let mut best: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(ip) = t.strip_prefix("|-- ") {
            if let Some(oct) = ip.split_whitespace().next() {
                // 只取 IPv4（4 段十进制）
                if oct.split('.').count() == 4 && !ip.starts_with("127.") {
                    best = Some(ip.to_string());
                }
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_roundtrip() {
        assert_eq!(agent_id_for("hermes", NATIVE), "hermes");
        assert_eq!(agent_id_for("hermes", "wsl:Ubuntu"), "hermes@wsl:Ubuntu");
        assert_eq!(
            split_agent_id("hermes@wsl:Ubuntu"),
            ("hermes", Some("wsl:Ubuntu"))
        );
        assert_eq!(split_agent_id("hermes"), ("hermes", None));
        assert_eq!(scope_of("hermes@wsl:Ubuntu"), "wsl:Ubuntu");
        assert_eq!(scope_of("hermes"), NATIVE);
        assert_eq!(kind_of("hermes@wsl:Ubuntu"), "hermes");
    }

    #[test]
    fn split_ignores_empty_parts() {
        assert_eq!(split_agent_id("@wsl:Ubuntu").0, "@wsl:Ubuntu");
        assert_eq!(split_agent_id("hermes@").1, None);
    }

    #[test]
    fn slug_keeps_distro_shape() {
        assert_eq!(slug("Ubuntu-24.04"), "Ubuntu-24.04");
        assert_eq!(slug("openSUSE Leap"), "openSUSE-Leap");
        assert_eq!(slug(""), "wsl");
    }

    #[test]
    fn native_scope_basics() {
        let s = native_scope(Path::new("/tmp/x"));
        assert_eq!(s.id, NATIVE);
        assert!(s.is_native());
        assert!(s.label().contains("本机"));
    }

    #[test]
    fn gateway_addr_native_unchanged() {
        let a = gateway_addr_for("hermes", "127.0.0.1:3425", Path::new("/tmp/x"));
        assert_eq!(a, "127.0.0.1:3425");
    }

    #[test]
    fn windows_scope_id_is_independent_of_native() {
        // WSL 里的实例与 Windows 侧同名实例必须是两个不同的 id
        assert_eq!(agent_id_for("hermes", "win:yxf"), "hermes@win:yxf");
        assert_ne!(
            agent_id_for("hermes", "win:yxf"),
            agent_id_for("hermes", NATIVE)
        );
        assert_eq!(kind_of("hermes@win:yxf"), "hermes");
        assert_eq!(scope_of("hermes@win:yxf"), "win:yxf");
    }

    #[test]
    fn scope_of_two_environments_differ() {
        assert_ne!(scope_of("hermes@wsl:Ubuntu"), scope_of("hermes@win:yxf"));
        assert_ne!(scope_of("hermes@wsl:Ubuntu"), scope_of("hermes"));
    }

    #[test]
    fn kind_labels_cover_all_envs() {
        assert_eq!(Kind::Native.as_str(), "native");
        assert_eq!(Kind::Wsl.as_str(), "wsl");
        assert_eq!(Kind::Windows.as_str(), "windows");
    }

    #[test]
    fn native_scope_has_no_distro_or_user() {
        let s = native_scope(Path::new("/tmp/x"));
        assert!(s.distro.is_none());
        assert!(s.user.is_none());
    }

    /// Linux/WSL 侧：扫挂载的 Windows 盘，找出装了 Agent 的用户目录，
    /// 生成 `win:<user>` scope——与本机 Linux home 独立开来。
    ///
    /// 扫描逻辑（`scan_agent_users`）是平台无关的，Windows 上也能测；
    /// 只有"盘符枚举"这一步是 Linux 专属。
    #[cfg(not(windows))]
    #[test]
    fn linux_enumerates_windows_users_with_agents() {
        let root = std::env::temp_dir().join(format!(
            "buffet-mnt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let users = root.join("c").join("Users");
        // yxf 装了 codex
        std::fs::create_dir_all(users.join("yxf").join(".codex")).unwrap();
        // Public 没装任何 agent → 必须被过滤掉
        std::fs::create_dir_all(users.join("Public")).unwrap();
        // /mnt/wsl 不是 Windows 盘 → 必须被过滤掉
        std::fs::create_dir_all(root.join("wsl")).unwrap();
        std::fs::create_dir_all(root.join("wslg")).unwrap();

        std::env::set_var("AGENTBUFFET_WSL_MNT", &root);
        let drives = mounted_windows_drives();
        assert_eq!(drives.len(), 1, "只应认 /c 一个盘：{drives:?}");
        assert_eq!(drives[0], root.join("c"));

        let scopes = windows_scopes();
        assert_eq!(scopes.len(), 1, "只应认装了 agent 的用户：{scopes:?}");
        assert_eq!(scopes[0].id, "win:yxf");
        assert_eq!(scopes[0].kind, Kind::Windows);
        assert_eq!(scopes[0].user.as_deref(), Some("yxf"));
        assert!(scopes[0].label().contains("Windows"));

        std::env::remove_var("AGENTBUFFET_WSL_MNT");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 用户目录扫描（平台无关）：只认装了 Agent 的用户，其余（Public/Default）
    /// 过滤掉；结果按用户名排序，保证 id 稳定。
    #[test]
    fn scan_users_keeps_only_ones_with_agents() {
        let root = std::env::temp_dir().join(format!(
            "buffet-users-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let users = root.join("Users");
        std::fs::create_dir_all(users.join("bob").join(".hermes")).unwrap();
        std::fs::create_dir_all(users.join("alice").join(".config").join("opencode")).unwrap();
        std::fs::create_dir_all(users.join("Public")).unwrap();
        std::fs::create_dir_all(users.join("carol")).unwrap();
        std::fs::write(users.join("carol").join(".claude.json"), "{}").unwrap();

        let found = scan_agent_users(&users);
        let names: Vec<&str> = found.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(
            names,
            vec!["alice", "bob", "carol"],
            "应按名排序并滤掉 Public"
        );
        assert_eq!(found[0].1, users.join("alice"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// scope id → home 的三态：native 直通、未知 scope 返回 None（绝不能
    /// 悄悄回落到别的环境），ghost 与真实环境互不干扰。
    #[test]
    fn home_for_scope_native_and_ghost() {
        let home = std::env::temp_dir().join(format!(
            "buffet-scope-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert_eq!(home_for_scope(NATIVE, &home), Some(home.clone()));
        assert_eq!(home_for_scope("wsl:GhostDistro42", &home), None);
        assert_eq!(home_for_scope("win:nosuchuser42", &home), None);
    }

    #[test]
    fn scope_by_id_hit_and_miss() {
        let scopes = vec![native_scope(Path::new("/tmp/scope-by-id"))];
        assert!(scope_by_id(&scopes, NATIVE).is_some());
        assert!(scope_by_id(&scopes, "wsl:Ubuntu-99.99").is_none());
    }

    /// 环境查不到时，注入地址必须原样回退到网关监听地址，而不是拼出半截
    /// host 或换端口。
    #[test]
    fn gateway_addr_for_ghost_scope_falls_back_untouched() {
        let home = std::env::temp_dir().join(format!(
            "buffet-gw-ghost-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert_eq!(
            gateway_addr_for("hermes@wsl:GhostDistro42", "127.0.0.1:3425", &home),
            "127.0.0.1:3425"
        );
    }
}
