//! 把应用图标和版本信息编进 exe 的 Windows 资源段。
//!
//! 这样文件资源管理器、任务栏、快捷方式上显示的都是我们自己的图标，
//! 而不是 Rust 默认的空白图标。窗口内的图标是运行时另外设置的（见 main.rs）。
//!
//! 图标由 packaging\make-icon.ps1 生成。

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // 图标缺失时不要让整个构建失败 —— 没有图标程序照样能跑，
    // 只是难看一点；重新生成一下 assets\icon.ico 即可。
    if !std::path::Path::new("assets/icon.ico").exists() {
        println!(
            "cargo:warning=找不到 assets/icon.ico，exe 将没有图标。\
             运行 `pwsh packaging\\make-icon.ps1` 生成。"
        );
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set("ProductName", "微信多开工具");
    res.set("FileDescription", "微信多开（共存）工具");
    res.set("CompanyName", "wx-tool");

    if let Err(e) = res.compile() {
        println!("cargo:warning=嵌入 Windows 资源失败: {e}");
    }
}
