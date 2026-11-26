//! 二进制入口（Tauri v2 lib 模式）。所有逻辑在 lib.rs。

fn main() {
    // 诊断模式：--selftest-nas <host[:port]> <user> <password>
    // 对真实 NAS 执行：登录 → 上传 → 下载 → 校验，然后退出。
    let args: Vec<String> = std::env::args().collect();
    // 无需密码的按需同步自检（使用已保存的 NAS 会话），用于安装后验证。
    if args.iter().any(|a| a == "--selftest-ondemand-saved") {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        std::process::exit(rt.block_on(fn_sync_client_lib::selftest_ondemand_saved()));
    }
    if args.iter().any(|a| a == "--selftest-ondemand-reconnect") {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        std::process::exit(rt.block_on(fn_sync_client_lib::selftest_ondemand_reconnect()));
    }
    for mode in ["--selftest-nas", "--selftest-sync", "--selftest-ondemand"] {
        if let Some(pos) = args.iter().position(|a| a == mode) {
            let rest: Vec<String> = args.iter().skip(pos + 1).cloned().collect();
            if rest.len() < 3 {
                eprintln!("usage: fn-sync-client {mode} <host[:port]> <user> <password>");
                std::process::exit(2);
            }
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let code = match mode {
                "--selftest-nas" => {
                    rt.block_on(fn_sync_client_lib::selftest_nas(&rest[0], &rest[1], &rest[2]))
                }
                "--selftest-sync" => {
                    rt.block_on(fn_sync_client_lib::selftest_sync(&rest[0], &rest[1], &rest[2]))
                }
                _ => rt.block_on(fn_sync_client_lib::selftest_ondemand(
                    &rest[0], &rest[1], &rest[2],
                )),
            };
            std::process::exit(code);
        }
    }
    fn_sync_client_lib::run()
}
