//! load_env_file：启动/热重载把 env 文件载入进程环境——仅补充未设置的变量，
//! 启动方显式导出的值优先；文件不存在时为 no-op。

#[test]
fn load_env_file_fills_missing_and_keeps_existing() {
    let dir = std::env::temp_dir().join(format!("buffet-envload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("env");
    std::fs::write(
        &f,
        "# comment\nBUFFET_ENVLOAD_MISSING=sk-new\nBUFFET_ENVLOAD_EXISTING=sk-from-file\n",
    )
    .unwrap();
    std::env::set_var("BUFFET_ENVLOAD_EXISTING", "sk-from-process");

    let n = agentbuffet::load_env_file(&f);
    assert!(n >= 1, "至少载入 MISSING 一项");
    assert_eq!(std::env::var("BUFFET_ENVLOAD_MISSING").unwrap(), "sk-new");
    assert_eq!(
        std::env::var("BUFFET_ENVLOAD_EXISTING").unwrap(),
        "sk-from-process",
        "进程已有变量不得被文件覆盖"
    );

    std::env::remove_var("BUFFET_ENVLOAD_MISSING");
    std::env::remove_var("BUFFET_ENVLOAD_EXISTING");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_env_file_missing_file_is_noop() {
    let dir = std::env::temp_dir().join(format!("buffet-envload-none-{}", std::process::id()));
    assert_eq!(agentbuffet::load_env_file(&dir.join("env")), 0);
}
