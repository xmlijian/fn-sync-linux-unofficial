fn main() {
    // 应用内定义的 "mdns" 插件需要声明命令权限，
    // 官方前端的 `plugin:mdns|browse_many` / `stop_browse` 才能通过 ACL。
    // 生成后需在 capabilities 中引用 `mdns:default` 一并授权。
    let attributes = tauri_build::Attributes::new().plugin(
        "mdns",
        tauri_build::InlinedPlugin::new().commands(&["browse_many", "stop_browse"]),
    );
    tauri_build::try_build(attributes).expect("tauri build failed");
}
