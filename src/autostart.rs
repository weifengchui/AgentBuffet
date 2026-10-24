//! 开机自启：Linux/WSL 写 systemd 用户级单元并以 wants 符号链接启用（等价
//! `systemctl --user enable`）；macOS 写 LaunchAgent plist（`~/Library/LaunchAgents`，
//! 下次登录由 launchd 加载）；Windows 写开始菜单启动目录 .bat。均为纯文件系统操作、
//! 零外部进程。启用后下次登录（开机）自动拉起 `AgentBuffet serve`。

use serde::Serialize;
use std::path::{Path, PathBuf};

/// 自启状态（CLI 与 UI 共用返回值）。
#[derive(Debug, Clone, Serialize)]
pub struct AutostartStatus {
    /// 实现方式：`systemd-user`、`launchagent` 或 `windows-startup`。
    pub method: &'static str,
    /// 是否已启用（存在启用链接 / 启动脚本）。
    pub enabled: bool,
    /// 单元文件 / 启动脚本路径。
    pub unit_path: String,
    /// wants 符号链接路径（Windows 为空串）。
    pub link_path: String,
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::*;

    fn paths(home: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let unit_dir = home.join(".config").join("systemd").join("user");
        let unit = unit_dir.join("agentbuffet.service");
        let link = unit_dir
            .join("default.target.wants")
            .join("agentbuffet.service");
        (unit_dir, unit, link)
    }

    fn unit_text() -> String {
        let exe = std::env::current_exe()
            .map(|x| x.display().to_string())
            .unwrap_or_else(|_| "AgentBuffet".into());
        format!(
            "[Unit]\nDescription=AgentBuffet AI gateway\nAfter=network-online.target\n\n\
             [Service]\nExecStart={exe} serve\nRestart=on-failure\nRestartSec=2\n\
             EnvironmentFile=-%h/.agentbuffet/env\n\n\
             [Install]\nWantedBy=default.target\n"
        )
    }

    pub fn status(home: &Path) -> AutostartStatus {
        let (_, unit, link) = paths(home);
        AutostartStatus {
            method: "systemd-user",
            enabled: link.exists(),
            unit_path: unit.display().to_string(),
            link_path: link.display().to_string(),
        }
    }

    pub fn enable(home: &Path) -> Result<AutostartStatus, String> {
        let (unit_dir, unit, link) = paths(home);
        std::fs::create_dir_all(&unit_dir)
            .map_err(|e| format!("创建 {} 失败: {e}", unit_dir.display()))?;
        std::fs::write(&unit, unit_text())
            .map_err(|e| format!("写入 {} 失败: {e}", unit.display()))?;
        let wants_dir = link
            .parent()
            .ok_or_else(|| "启用链接路径非法".to_string())?;
        std::fs::create_dir_all(wants_dir)
            .map_err(|e| format!("创建 {} 失败: {e}", wants_dir.display()))?;
        if std::fs::symlink_metadata(&link).is_ok() {
            let _ = std::fs::remove_file(&link);
        }
        std::os::unix::fs::symlink(&unit, &link)
            .map_err(|e| format!("创建启用链接 {} 失败: {e}", link.display()))?;
        Ok(status(home))
    }

    pub fn disable(home: &Path) -> Result<AutostartStatus, String> {
        let (_, unit, link) = paths(home);
        for p in [&link, &unit] {
            match std::fs::remove_file(p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("删除 {} 失败: {e}", p.display())),
            }
        }
        Ok(status(home))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    fn agents_dir(home: &Path) -> PathBuf {
        home.join("Library").join("LaunchAgents")
    }

    fn plist_path(home: &Path) -> PathBuf {
        agents_dir(home).join("com.agentbuffet.gateway.plist")
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    fn plist_text(home: &Path) -> String {
        let exe = xml_escape(
            &std::env::current_exe()
                .map(|x| x.display().to_string())
                .unwrap_or_else(|_| "AgentBuffet".into()),
        );
        let log = xml_escape(&home.join(".agentbuffet").join("launchd.log").display().to_string());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>com.agentbuffet.gateway</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>serve</string>
	</array>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key><false/>
	</dict>
	<key>StandardOutPath</key><string>{log}</string>
	<key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
        )
    }

    pub fn status(home: &Path) -> AutostartStatus {
        let plist = plist_path(home);
        AutostartStatus {
            method: "launchagent",
            enabled: plist.exists(),
            unit_path: plist.display().to_string(),
            link_path: String::new(),
        }
    }

    pub fn enable(home: &Path) -> Result<AutostartStatus, String> {
        let dir = agents_dir(home);
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
        // launchd 打开日志路径要求目录已存在；env 文件由 serve 启动时自行载入
        std::fs::create_dir_all(home.join(".agentbuffet"))
            .map_err(|e| format!("创建 {} 失败: {e}", home.join(".agentbuffet").display()))?;
        std::fs::write(plist_path(home), plist_text(home))
            .map_err(|e| format!("写入 LaunchAgent 失败: {e}"))?;
        Ok(status(home))
    }

    pub fn disable(home: &Path) -> Result<AutostartStatus, String> {
        let plist = plist_path(home);
        match std::fs::remove_file(&plist) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("删除 {} 失败: {e}", plist.display())),
        }
        Ok(status(home))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;

    fn startup_dir(home: &Path) -> PathBuf {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Roaming"))
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
    }

    fn bat_path(home: &Path) -> PathBuf {
        startup_dir(home).join("agentbuffet.bat")
    }

    pub fn status(home: &Path) -> AutostartStatus {
        let bat = bat_path(home);
        AutostartStatus {
            method: "windows-startup",
            enabled: bat.exists(),
            unit_path: bat.display().to_string(),
            link_path: String::new(),
        }
    }

    pub fn enable(home: &Path) -> Result<AutostartStatus, String> {
        let dir = startup_dir(home);
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
        let exe = std::env::current_exe()
            .map(|x| x.display().to_string())
            .unwrap_or_else(|_| "AgentBuffet.exe".into());
        std::fs::write(bat_path(home), format!("@echo off\r\n\"{exe}\" serve\r\n"))
            .map_err(|e| format!("写入启动脚本失败: {e}"))?;
        Ok(status(home))
    }

    pub fn disable(home: &Path) -> Result<AutostartStatus, String> {
        let bat = bat_path(home);
        match std::fs::remove_file(&bat) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("删除 {} 失败: {e}", bat.display())),
        }
        Ok(status(home))
    }
}

pub fn status(home: &Path) -> AutostartStatus {
    imp::status(home)
}

pub fn enable(home: &Path) -> Result<AutostartStatus, String> {
    imp::enable(home)
}

pub fn disable(home: &Path) -> Result<AutostartStatus, String> {
    imp::disable(home)
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_home() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("buffet-autostart-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    static SEQ: AtomicU32 = AtomicU32::new(0);

    #[test]
    fn enable_writes_unit_and_wants_symlink() {
        let home = temp_home();
        let s = enable(&home).expect("enable ok");
        assert!(s.enabled);
        assert_eq!(s.method, "systemd-user");
        let unit = home.join(".config/systemd/user/agentbuffet.service");
        let text = std::fs::read_to_string(&unit).unwrap();
        assert!(text.contains(" serve"), "unit: {text}");
        assert!(text.contains("EnvironmentFile=-%h/.agentbuffet/env"));
        assert!(text.contains("WantedBy=default.target"));
        let link = home.join(".config/systemd/user/default.target.wants/agentbuffet.service");
        assert!(std::fs::symlink_metadata(&link)
            .expect("link exists")
            .file_type()
            .is_symlink());

        // 幂等：重复 enable 不报错，链接仍在
        let s2 = enable(&home).expect("re-enable ok");
        assert!(s2.enabled);

        // disable：链接与单元都删除，状态回到未启用；重复 disable 幂等
        let s3 = disable(&home).expect("disable ok");
        assert!(!s3.enabled);
        assert!(!unit.exists());
        assert!(std::fs::symlink_metadata(&link).is_err());
        disable(&home).expect("re-disable ok");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    fn temp_home() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("buffet-autostart-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn enable_writes_launchagent_plist() {
        let home = temp_home();
        let s = enable(&home).expect("enable ok");
        assert!(s.enabled);
        assert_eq!(s.method, "launchagent");
        let plist = home.join("Library/LaunchAgents/com.agentbuffet.gateway.plist");
        let text = std::fs::read_to_string(&plist).unwrap();
        assert!(text.contains("<string>serve</string>"), "plist: {text}");
        assert!(text.contains("<key>RunAtLoad</key>"));
        assert!(text.contains("<key>KeepAlive</key>"));
        assert!(text.contains(".agentbuffet"));

        // 幂等：重复 enable 不报错，plist 仍在
        let s2 = enable(&home).expect("re-enable ok");
        assert!(s2.enabled);

        // disable：plist 删除，状态回到未启用；重复 disable 幂等
        let s3 = disable(&home).expect("disable ok");
        assert!(!s3.enabled);
        assert!(!plist.exists());
        disable(&home).expect("re-disable ok");
    }
}
