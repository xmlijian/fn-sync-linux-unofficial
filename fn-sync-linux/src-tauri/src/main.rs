//! 二进制入口（Tauri v2 lib 模式）。所有逻辑在 lib.rs。

fn main() {
    // 诊断模式：--selftest-nas <host[:port]> <user> <password>
    // 对真实 NAS 执行：登录 → 上传 → 下载 → 校验，然后退出。
    let args: Vec<String> = std::env::args().collect();
    for mode in ["--selftest-nas", "--selftest-sync"] {
        if let Some(pos) = args.iter().position(|a| a == mode) {
            let rest: Vec<String> = args.iter().skip(pos + 1).cloned().collect();
            if rest.len() < 3 {
                eprintln!("usage: fn-sync-client {mode} <host[:port]> <user> <password>");
                std::process::exit(2);
            }
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let code = if mode == "--selftest-nas" {
                rt.block_on(fn_sync_client_lib::selftest_nas(
                    &rest[0], &rest[1], &rest[2],
                ))
            } else {
                rt.block_on(fn_sync_client_lib::selftest_sync(
                    &rest[0], &rest[1], &rest[2],
                ))
            };
            std::process::exit(code);
        }
    }
    fn_sync_client_lib::run()
}
