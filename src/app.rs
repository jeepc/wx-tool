//! egui 界面。
//!
//! 耗时操作（复制 189 MB + 打补丁）全部放在工作线程里跑，通过 channel 把日志
//! 回传给 UI 线程，界面不会卡住。

use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use crate::coexist;
use crate::elevate;
use crate::install::{self, Install, Instance};

/// 启动时要自动执行的动作（用于 UAC 提权重启后接着干原来的活）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    Create { n: u8, force: bool },
    Remove { n: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Create { n: u8, force: bool },
    Remove { n: u8 },
    DryRun { n: u8 },
}

enum Msg {
    Log(String),
    Done(Result<(), String>),
}

#[derive(Clone)]
struct LogLine {
    text: String,
    kind: LineKind,
}

#[derive(Clone, Copy, PartialEq)]
enum LineKind {
    Info,
    Good,
    Bad,
}

pub struct App {
    install: Option<Install>,
    detect_error: Option<String>,
    instances: Vec<Instance>,
    log: Vec<LogLine>,
    rx: Option<Receiver<Msg>>,
    busy: bool,
    /// 需要用户确认的操作
    confirm: Option<(String, Action)>,
    /// 需要提权才能进行的操作
    needs_elevation: Option<Action>,
    pending: Option<Pending>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, pending: Option<Pending>) -> Self {
        install_fonts(&cc.egui_ctx);

        let mut app = App {
            install: None,
            detect_error: None,
            instances: Vec::new(),
            log: Vec::new(),
            rx: None,
            busy: false,
            confirm: None,
            needs_elevation: None,
            pending,
        };
        app.refresh();
        app
    }

    fn info(&mut self, s: impl Into<String>) {
        self.log.push(LogLine { text: s.into(), kind: LineKind::Info });
    }
    fn good(&mut self, s: impl Into<String>) {
        self.log.push(LogLine { text: s.into(), kind: LineKind::Good });
    }
    fn bad(&mut self, s: impl Into<String>) {
        self.log.push(LogLine { text: s.into(), kind: LineKind::Bad });
    }

    /// 重新探测安装位置并刷新副本列表。
    fn refresh(&mut self) {
        match install::detect() {
            Ok(inst) => {
                if self.install.is_none() {
                    self.good(format!("找到微信 {} —— {}", inst.version, inst.base.display()));
                }
                self.instances = install::instances(&inst);
                self.install = Some(inst);
                self.detect_error = None;
            }
            Err(e) => {
                self.detect_error = Some(format!("{e:#}"));
                self.instances.clear();
                self.install = None;
            }
        }
    }

    /// 用户手动指定 Weixin.exe。
    fn pick_manually(&mut self) {
        let picked = rfd::FileDialog::new()
            .set_title("选择微信的 Weixin.exe")
            .add_filter("Weixin.exe", &["exe"])
            .pick_file();
        let Some(exe) = picked else { return };
        let Some(base) = exe.parent().map(PathBuf::from) else {
            self.bad("无法确定所选文件的所在目录");
            return;
        };
        match install::from_base(&base) {
            Ok(inst) => {
                self.good(format!("已手动指定：{}（{}）", inst.base.display(), inst.version));
                self.instances = install::instances(&inst);
                self.install = Some(inst);
                self.detect_error = None;
            }
            Err(e) => self.bad(format!("{e:#}")),
        }
    }

    /// 启动一个动作。需要写权限但当前写不进去时，转为请求提权。
    fn start(&mut self, action: Action) {
        let Some(stale) = self.install.clone() else { return };

        // 微信会在后台自动升级：新建版本目录、清掉旧的。`self.install` 只在启动、
        // 动作结束、点「重新检测」时刷新，界面开着不动就会过期。拿过期快照去创建，
        // DLL 落进旧版本目录，而副本启动器运行时只认最新目录 —— 创建报成功，
        // 双击却是「由于环境异常，不能继续使用。」所以动手前按 base 重新推导一次。
        // 用 from_base 而不是 detect，是为了保住用户「手动选择」的安装目录。
        let inst = match install::from_base(&stale.base) {
            Ok(fresh) => fresh,
            Err(e) => {
                self.bad(format!("{e:#}"));
                return;
            }
        };
        let upgraded = (inst.version != stale.version)
            .then(|| format!("微信已升级：{} → {}，按新版本目录操作。", stale.version, inst.version));
        if upgraded.is_some() {
            self.instances = install::instances(&inst);
        }
        self.install = Some(inst.clone());

        let needs_write = !matches!(action, Action::DryRun { .. });
        if needs_write
            && !(elevate::can_write_dir(&inst.base) && elevate::can_write_dir(&inst.version_dir))
        {
            self.needs_elevation = Some(action);
            return;
        }

        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.busy = true;
        self.log.clear();
        // 放在 clear 之后，否则这条提示会被一起清掉
        if let Some(note) = upgraded {
            self.info(note);
        }

        std::thread::spawn(move || {
            let mut sink = |s: String| {
                let _ = tx.send(Msg::Log(s));
            };
            let result = run_action(&inst, action, &mut sink);
            let _ = tx.send(Msg::Done(result.map_err(|e| format!("{e:#}"))));
        });
    }

    fn drain(&mut self, ctx: &egui::Context) {
        let mut finished = None;
        let mut lines = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Log(s) => lines.push(s),
                    Msg::Done(r) => finished = Some(r),
                }
            }
        } else {
            return;
        }

        for l in lines {
            self.info(l);
        }
        if let Some(result) = finished {
            match result {
                Ok(()) => self.good("操作完成。"),
                Err(e) => self.bad(format!("失败：{e}")),
            }
            self.busy = false;
            self.rx = None;
            self.refresh();
        }
        if self.busy {
            // 工作线程还在跑，保持重绘以便日志实时刷新
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
    }
}

fn run_action(inst: &Install, action: Action, log: coexist::Log) -> anyhow::Result<()> {
    match action {
        Action::DryRun { n } => coexist::dry_run(inst, n, log),
        Action::Create { n, force } => coexist::create(inst, n, force, log),
        Action::Remove { n } => {
            let target = install::instances(inst)
                .into_iter()
                .find(|i| i.n == n)
                .ok_or_else(|| anyhow::anyhow!("找不到编号 {n}"))?;
            coexist::remove(inst, &target, log)
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // 提权重启后自动继续原来的操作
        if self.install.is_some()
            && let Some(p) = self.pending.take()
        {
            let action = match p {
                Pending::Create { n, force } => Action::Create { n, force },
                Pending::Remove { n } => Action::Remove { n },
            };
            self.start(action);
        }

        self.drain(&ctx);

        egui::Panel::top("head").show(ui, |ui| self.header(ui));
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(120.0)
            .min_size(60.0)
            .show(ui, |ui| self.log_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.body(ui));

        self.dialogs(&ctx);
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading("微信多开");
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("为每个账号生成一份独立的共存副本")
                    .weak()
                    .size(12.5),
            );
        });
        ui.add_space(6.0);

        // 安装信息做成一张浅色卡片，和下面的列表拉开层次
        egui::Frame::default()
            .fill(ui.visuals().faint_bg_color)
            .corner_radius(6)
            .inner_margin(egui::Margin::symmetric(12, 9))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                match &self.install {
                    Some(inst) => {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new("安装位置").weak().size(12.0));
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(inst.base.display().to_string())
                                    .monospace()
                                    .size(12.5),
                            );
                            ui.add_space(12.0);
                            ui.label(egui::RichText::new("版本").weak().size(12.0));
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(&inst.version).monospace().size(12.5),
                            );
                        });
                    }
                    None => {
                        ui.colored_label(RED, "未找到微信安装");
                    }
                }
                if let Some(err) = self.detect_error.clone() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(err).color(AMBER).size(12.0));
                }
            });

        ui.add_space(7.0);
        let busy = self.busy;
        let mut do_refresh = false;
        let mut do_pick = false;
        let mut do_check = false;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                do_refresh = ui.button("重新检测").clicked();
                do_pick = ui.button("手动选择 Weixin.exe…").clicked();
                if self.install.is_some() {
                    do_check = ui
                        .button("检查兼容性")
                        .on_hover_text(
                            "只扫描特征、不写任何文件，用来确认当前微信版本是否受支持。\
                             无需管理员权限。",
                        )
                        .clicked();
                }
            });
            // 忙碌指示放到最右边，不要挤在按钮后面
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if busy {
                    ui.add(egui::Spinner::new().size(16.0));
                    ui.label(egui::RichText::new("处理中…").weak().size(12.0));
                }
            });
        });
        ui.add_space(8.0);

        if do_refresh {
            self.refresh();
        }
        if do_pick {
            self.pick_manually();
        }
        if do_check {
            self.start(Action::DryRun { n: 1 });
        }
    }

    fn body(&mut self, ui: &mut egui::Ui) {
        if self.install.is_none() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label("请先让程序找到微信，或手动选择 Weixin.exe。");
            });
            return;
        }

        let busy = self.busy;
        let mut queued: Option<Action> = None;
        let mut queued_confirm: Option<(String, Action)> = None;
        let mut launch_err: Option<String> = None;

        let created = self.instances.iter().filter(|i| i.is_complete()).count();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("共存副本").strong().size(13.5));
            ui.label(
                egui::RichText::new(format!("已创建 {created} / 10")).weak().size(12.0),
            );
        });
        ui.add_space(6.0);

        egui::ScrollArea::vertical()
            // 不让滚动区域收缩到内容宽度，否则滚动条会跑到窗口中间
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (idx, inst) in self.instances.iter().enumerate() {
                    let bg = if idx % 2 == 0 {
                        ui.visuals().faint_bg_color
                    } else {
                        egui::Color32::TRANSPARENT
                    };

                    egui::Frame::default()
                        .fill(bg)
                        .corner_radius(5)
                        // 纵向留白压到 3，十行才能在 1080p@150% 的屏幕上一屏放下
                        .inner_margin(egui::Margin::symmetric(10, 3))
                        .show(ui, |ui| {
                            // 撑满可用宽度，行才不会挤在左边
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let (text, color, dot) = status_of(inst);

                                ui.label(egui::RichText::new(dot).color(color).size(10.0));
                                ui.add_space(2.0);
                                ui.label(
                                    egui::RichText::new(format!("微信 {}", inst.n))
                                        .monospace()
                                        .size(13.0),
                                );
                                ui.add_space(10.0);
                                ui.label(egui::RichText::new(text).color(color).size(12.0))
                                    .on_hover_text(format!(
                                        "启动器：{}\nDLL：{}",
                                        inst.exe.display(),
                                        inst.dll.display()
                                    ));

                                // 操作按钮右对齐（注意 right_to_left 里控件是从右往左排的）
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.add_enabled_ui(!busy, |ui| {
                                            if inst.is_complete() {
                                                if ui.button("删除").clicked() {
                                                    queued_confirm = Some((
                                                        format!(
                                                            "删除微信 {} 的共存副本？\n\n\
                                                             只会删掉生成的 Weixin{}.exe 和 \
                                                             Weixin.dl{}，原版微信和聊天记录\
                                                             都不受影响。",
                                                            inst.n, inst.n, inst.n
                                                        ),
                                                        Action::Remove { n: inst.n },
                                                    ));
                                                }
                                                if ui
                                                    .button("重建")
                                                    .on_hover_text(
                                                        "微信升级后副本会失效，重建即可恢复",
                                                    )
                                                    .clicked()
                                                {
                                                    queued_confirm = Some((
                                                        format!(
                                                            "重新生成微信 {} 的共存副本？\
                                                             现有文件会被覆盖。",
                                                            inst.n
                                                        ),
                                                        Action::Create {
                                                            n: inst.n,
                                                            force: true,
                                                        },
                                                    ));
                                                }
                                                if ui.button("启动").clicked()
                                                    && let Err(e) = coexist::launch(inst)
                                                {
                                                    launch_err = Some(format!("{e:#}"));
                                                }
                                            } else if inst.is_broken() {
                                                if ui.button("清理").clicked() {
                                                    queued = Some(Action::Remove { n: inst.n });
                                                }
                                                if ui.button("重建").clicked() {
                                                    queued = Some(Action::Create {
                                                        n: inst.n,
                                                        force: true,
                                                    });
                                                }
                                            } else if ui.button("创建").clicked() {
                                                queued = Some(Action::Create {
                                                    n: inst.n,
                                                    force: false,
                                                });
                                            }
                                        });
                                    },
                                );
                            });
                        });
                }

                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new(
                        "共存副本与原版共用聊天记录，但设置、自动登录、互斥锁和窗口名都是独立的，\
                         因此可以和原版同时登录不同账号。\n\
                         创建过程不会修改原版的 Weixin.exe / Weixin.dll，删除副本即完全还原。",
                    )
                    .weak()
                    .size(12.0),
                );
                ui.add_space(6.0);
            });

        if let Some(a) = queued {
            self.start(a);
        }
        if let Some(c) = queued_confirm {
            self.confirm = Some(c);
        }
        if let Some(e) = launch_err {
            self.bad(e);
        }
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        let mut clear = false;
        ui.horizontal(|ui| {
            ui.strong("日志");
            if !self.log.is_empty() {
                clear = ui.small_button("清空").clicked();
            }
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.log.is_empty() {
                    ui.weak("（暂无输出）");
                }
                for line in &self.log {
                    let color = match line.kind {
                        LineKind::Info => ui.visuals().weak_text_color(),
                        LineKind::Good => GREEN,
                        LineKind::Bad => RED,
                    };
                    ui.label(
                        egui::RichText::new(&line.text).monospace().size(12.0).color(color),
                    );
                }
            });
        if clear {
            self.log.clear();
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        // ---- 确认对话框 ----
        if let Some((msg, action)) = self.confirm.clone() {
            let mut decision: Option<bool> = None;
            egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.heading("请确认");
                ui.add_space(8.0);
                ui.label(msg);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("确定").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("取消").clicked() {
                        decision = Some(false);
                    }
                });
            });
            match decision {
                Some(true) => {
                    self.confirm = None;
                    self.start(action);
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }

        // ---- 提权对话框 ----
        if let Some(action) = self.needs_elevation {
            let mut relaunch = false;
            let mut cancel = false;
            egui::Modal::new(egui::Id::new("elevate")).show(ctx, |ui| {
                ui.set_max_width(440.0);
                ui.heading("需要管理员权限");
                ui.add_space(8.0);
                if elevate::is_elevated() {
                    // 已经是管理员却仍然写不进去：多半是杀软/EDR 拦了，或者盘是只读的。
                    // 这时候再提权一次也没用，得说清楚。
                    ui.label(
                        "当前已经是管理员，但仍然无法写入微信的安装目录。\n\
                         常见原因是安全软件拦截了对 Program Files 的写入，\
                         或该分区为只读。\n请暂时关闭安全软件后重试。",
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        cancel = ui.button("知道了").clicked();
                    });
                } else {
                    ui.label(
                        "微信装在 Program Files 里，创建或删除共存副本需要管理员权限。\n\
                         点击「以管理员身份重启」后，程序会重新启动并自动继续这次操作。",
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        relaunch = ui.button("以管理员身份重启").clicked();
                        cancel = ui.button("取消").clicked();
                    });
                }
            });
            if relaunch {
                let args = resume_args(action);
                match elevate::relaunch_elevated(&args) {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        self.needs_elevation = None;
                        self.bad(format!("{e:#}"));
                    }
                }
            } else if cancel {
                self.needs_elevation = None;
            }
        }
    }
}

/// 提权重启时传给新进程的命令行参数。
fn resume_args(action: Action) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    match action {
        Action::Create { n, force } => {
            let mut v = vec![OsString::from("--create"), OsString::from(n.to_string())];
            if force {
                v.push(OsString::from("--force"));
            }
            v
        }
        Action::Remove { n } => vec![OsString::from("--remove"), OsString::from(n.to_string())],
        Action::DryRun { .. } => Vec::new(),
    }
}

const GREEN: egui::Color32 = egui::Color32::from_rgb(64, 168, 96);
const GREY: egui::Color32 = egui::Color32::from_rgb(135, 135, 135);
const AMBER: egui::Color32 = egui::Color32::from_rgb(214, 146, 42);
const RED: egui::Color32 = egui::Color32::from_rgb(203, 82, 82);

/// 返回 (状态文字, 颜色, 状态圆点)。
fn status_of(inst: &Instance) -> (String, egui::Color32, &'static str) {
    const DOT: &str = "●";
    const RING: &str = "○";

    if inst.is_complete() {
        let mut s = "已创建".to_string();
        if inst.running {
            s.push_str(" · 运行中");
        }
        if !inst.stale_dirs.is_empty() {
            s.push_str(&format!("（旧版本残留：{}）", inst.stale_dirs.join(", ")));
        }
        return (s, GREEN, DOT);
    }
    if inst.exe_exists && !inst.dll_exists {
        let hint = if inst.stale_dirs.is_empty() {
            "缺少 DLL，需重建".to_string()
        } else {
            format!("微信已升级，DLL 还在 {} —— 需重建", inst.stale_dirs.join(", "))
        };
        return (hint, AMBER, DOT);
    }
    if !inst.exe_exists && inst.dll_exists {
        return ("缺少启动器，需重建".to_string(), AMBER, DOT);
    }
    ("未创建".to_string(), GREY, RING)
}

/// 加载系统里的中文字体。
///
/// 不内嵌字体：微软雅黑在每台 Windows 上都有，内嵌会让二进制平白大 10~20 MB。
fn install_fonts(ctx: &egui::Context) {
    let candidates = [
        (r"C:\Windows\Fonts\msyh.ttc", 0u32),
        (r"C:\Windows\Fonts\msyh.ttf", 0),
        (r"C:\Windows\Fonts\simhei.ttf", 0),
        (r"C:\Windows\Fonts\simsun.ttc", 0),
    ];

    let Some((bytes, index)) =
        candidates.iter().find_map(|(p, i)| std::fs::read(p).ok().map(|b| (b, *i)))
    else {
        return; // 找不到中文字体也不至于崩，只是中文会显示成方框
    };

    let mut data = egui::FontData::from_owned(bytes);
    data.index = index; // .ttc 是字体集合，要指定用其中哪一款

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("cjk".to_owned(), std::sync::Arc::new(data));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "cjk".to_owned());
    // 等宽族里放在末尾：ASCII 仍用等宽字体，中文回退到雅黑
    fonts.families.entry(egui::FontFamily::Monospace).or_default().push("cjk".to_owned());

    ctx.set_fonts(fonts);
}
