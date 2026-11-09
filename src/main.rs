// GUI 程序，不要在启动时弹出控制台黑框。
#![windows_subsystem = "windows"]

mod app;
mod coexist;
mod elevate;
mod install;
mod patch;
mod patterns;
mod staging;

use app::Pending;

/// 解析命令行。
///
/// 这些参数不是给用户手敲的 —— 它们是提权重启时本程序传给自己的，
/// 用来在新进程里接着完成用户刚才点的那个操作。
fn parse_pending() -> Option<Pending> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let force = args.iter().any(|a| a == "--force");

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--create" => {
                let n = it.next()?.parse::<u8>().ok()?;
                return (n <= 9).then_some(Pending::Create { n, force });
            }
            "--remove" => {
                let n = it.next()?.parse::<u8>().ok()?;
                return (n <= 9).then_some(Pending::Remove { n });
            }
            _ => {}
        }
    }
    None
}

/// 窗口/任务栏图标。exe 文件本身的图标是 build.rs 通过 Windows 资源嵌入的，
/// 而窗口内显示的这个要在运行时单独设置。
fn window_icon() -> Option<eframe::egui::IconData> {
    const PNG: &[u8] = include_bytes!("../assets/icon-256.png");
    match eframe::icon_data::from_png_bytes(PNG) {
        Ok(icon) => Some(icon),
        Err(e) => {
            eprintln!("加载窗口图标失败: {e}");
            None
        }
    }
}

fn main() -> eframe::Result<()> {
    let pending = parse_pending();

    let mut viewport = eframe::egui::ViewportBuilder::default()
        // 高度控制在 640 逻辑像素以内：1080p 屏幕在 150% 缩放下可用高度约 660，
        // 再高就会超出屏幕，十个槽位也就一屏放不下了
        .with_inner_size([880.0, 640.0])
        .with_min_inner_size([700.0, 460.0])
        .with_title("微信多开工具");
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions { viewport, ..Default::default() };

    eframe::run_native(
        "微信多开工具",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, pending)))),
    )
}
