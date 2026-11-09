//! 共存副本的创建与删除。
//!
//! 核心保证：**原始的 `Weixin.exe` / `Weixin.dll` 永远只读打开，从不写入。**
//! 整个 crate 里唯一以写方式打开文件的地方在 `staging.rs`，而它只碰临时文件。
//! 因此「删掉副本」就等于完全还原，不需要 BetterWX 那套 `.bak` 备份。

use anyhow::{Context, Result, bail};
use memmap2::Mmap;
use std::path::Path;

use crate::install::{Install, Instance};
use crate::patch::{PatchPlan, plan};
use crate::patterns::{dll_specs, exe_specs};
use crate::staging::{StagedFile, ensure_space};

/// 进度/日志回调。
pub type Log<'a> = &'a mut dyn FnMut(String);

/// 只读映射一个文件。
fn map_ro(path: &Path) -> Result<Mmap> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("无法读取 {}（文件可能正被占用）", path.display()))?;
    // SAFETY: 只读映射。理论上其他进程改动该文件会造成数据竞争，但这里映射的是
    // 微信的安装文件，正常情况下不会被并发改写；即便被改，我们也只是读到不一致的
    // 字节从而匹配失败，不会越界访问。
    unsafe { Mmap::map(&file) }.with_context(|| format!("无法映射 {}", path.display()))
}

/// 把一次规划结果打印成人类可读的日志。
fn report(label: &str, p: &PatchPlan, log: Log) {
    for hit in &p.hits {
        let sites = hit
            .sites
            .iter()
            .map(|s| format!("{s:#x}"))
            .collect::<Vec<_>>()
            .join(", ");
        log(format!(
            "  [√] {} · {}（{}）：命中 {} 处，写入 {}",
            label,
            hit.name,
            hit.variant,
            hit.offsets.len(),
            if sites.is_empty() { "—".into() } else { sites },
        ));
    }
}

/// 只扫描不写入：验证当前微信版本的全部特征是否都能匹配上。
///
/// 标准用户即可运行，不需要管理员权限 —— 这是排查「新版本微信是否还支持」最快的方式。
pub fn dry_run(inst: &Install, n: u8, log: Log) -> Result<()> {
    check_digit(n)?;
    log(format!("试运行检查（编号 {n}），不会写入任何文件"));
    log(format!("安装目录：{}", inst.base.display()));
    log(format!("版本目录：{}", inst.version));

    let exe_src = inst.exe();
    let dll_src = inst.dll();

    log(format!("扫描 {} …", exe_src.display()));
    let exe_map = map_ro(&exe_src)?;
    let exe_plan = plan(&exe_specs(n), &exe_map)?;
    report("Weixin.exe", &exe_plan, log);

    log(format!("扫描 {}（189 MB，请稍候）…", dll_src.display()));
    let dll_map = map_ro(&dll_src)?;
    let dll_plan = plan(&dll_specs(n), &dll_map)?;
    report("Weixin.dll", &dll_plan, log);

    log(format!(
        "全部 {} 个特征均匹配成功，该版本受支持。",
        exe_plan.hits.len() + dll_plan.hits.len()
    ));
    Ok(())
}

/// 创建编号为 `n` 的共存副本。
pub fn create(inst: &Install, n: u8, force: bool, log: Log) -> Result<()> {
    check_digit(n)?;
    check_version_current(inst)?;

    let exe_src = inst.exe();
    let dll_src = inst.dll();
    let exe_dst = inst.base.join(format!("Weixin{n}.exe"));
    let dll_dst = inst.version_dir.join(format!("Weixin.dl{n}"));

    // ---- 前置检查 ----
    if !force && (exe_dst.exists() || dll_dst.exists()) {
        bail!(
            "编号 {n} 已被占用：\n  {}\n  {}\n请先删除，或选择「覆盖」。",
            exe_dst.display(),
            dll_dst.display()
        );
    }

    let exe_len = std::fs::metadata(&exe_src)?.len();
    let dll_len = std::fs::metadata(&dll_src)?.len();
    ensure_space(&inst.version_dir, exe_len + dll_len)?;

    // ---- 第一步：只读扫描，确认两个文件的特征都在 ----
    // 必须在复制之前做完。否则一旦 DLL 特征失配，就白复制了 189 MB，
    // 还可能留下一个指向不存在的 DLL 的启动器。
    log("检查特征…".into());
    let exe_map = map_ro(&exe_src)?;
    let exe_plan = plan(&exe_specs(n), &exe_map)?;
    report("Weixin.exe", &exe_plan, log);
    drop(exe_map);

    let dll_map = map_ro(&dll_src)?;
    let dll_plan = plan(&dll_specs(n), &dll_map)?;
    report("Weixin.dll", &dll_plan, log);
    drop(dll_map);

    // ---- 第二步：复制到临时文件 ----
    log(format!("复制 Weixin.exe → Weixin{n}.exe …"));
    let exe_stage = StagedFile::copy_from(&exe_src, exe_dst.clone())?;

    log(format!(
        "复制 Weixin.dll → Weixin.dl{n}（{:.0} MB，这一步最慢）…",
        dll_len as f64 / 1e6
    ));
    let dll_stage = StagedFile::copy_from(&dll_src, dll_dst.clone())?;

    // ---- 第三步：在副本上打补丁 ----
    log("写入补丁…".into());
    {
        let mut m = exe_stage.map_mut()?;
        if m.len() as u64 != exe_len {
            bail!("复制出的 Weixin{n}.exe 大小异常");
        }
        exe_plan.apply(&mut m)?;
        m.flush().context("刷新 Weixin.exe 副本失败")?;
    }
    {
        let mut m = dll_stage.map_mut()?;
        if m.len() as u64 != dll_len {
            bail!("复制出的 Weixin.dl{n} 大小异常");
        }
        dll_plan.apply(&mut m)?;
        m.flush().context("刷新 Weixin.dll 副本失败")?;
    }

    // ---- 第四步：两阶段提交 ----
    // 先提交 DLL 再提交 EXE。万一 EXE 提交失败，留下的是一个用不到的 DLL（无害），
    // 而不是一个指向不存在 DLL 的启动器（双击必崩，且用户不知道该删什么）。
    dll_stage.commit()?;
    if let Err(e) = exe_stage.commit() {
        let _ = std::fs::remove_file(&dll_dst);
        return Err(e.context("提交启动器失败，已回滚 DLL 副本"));
    }

    log(format!("完成：{}", exe_dst.display()));
    log(format!("完成：{}", dll_dst.display()));
    log("原始 Weixin.exe / Weixin.dll 未被改动。".into());
    Ok(())
}

/// 删除编号为 `n` 的共存副本，连同旧版本目录里的残留。
pub fn remove(inst: &Install, target: &Instance, log: Log) -> Result<()> {
    let n = target.n;
    check_digit(n)?;

    if target.running {
        bail!("Weixin{n}.exe 正在运行，请先退出该副本再删除。");
    }

    let mut removed = 0usize;

    let exe = inst.base.join(format!("Weixin{n}.exe"));
    if exe.is_file() {
        guard_removable(&exe)?;
        std::fs::remove_file(&exe)
            .with_context(|| format!("删除 {} 失败", exe.display()))?;
        log(format!("已删除 {}", exe.display()));
        removed += 1;
    }

    // 当前版本目录 + 所有旧版本目录里的 Weixin.dl{n} 都要清掉
    for (_, dir) in crate::install::version_dirs(&inst.base) {
        let dll = dir.join(format!("Weixin.dl{n}"));
        if dll.is_file() {
            guard_removable(&dll)?;
            std::fs::remove_file(&dll)
                .with_context(|| format!("删除 {} 失败", dll.display()))?;
            log(format!("已删除 {}", dll.display()));
            removed += 1;
        }
    }

    if removed == 0 {
        log(format!("编号 {n} 没有可删除的文件。"));
    }
    Ok(())
}

/// 删除前的最后一道保险：文件名必须严格符合我们自己生成的命名。
///
/// 原版叫 `Weixin.exe` / `Weixin.dll`，永远匹配不上这两个模式，
/// 所以即使上游逻辑出错也不可能误删原文件。
fn guard_removable(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let is_copy_exe = name.len() == "Weixin0.exe".len()
        && name.starts_with("Weixin")
        && name.ends_with(".exe")
        && name.as_bytes()[6].is_ascii_digit();
    let is_copy_dll = name.len() == "Weixin.dl0".len()
        && name.starts_with("Weixin.dl")
        && name.as_bytes()[9].is_ascii_digit();

    if !is_copy_exe && !is_copy_dll {
        bail!("拒绝删除 {}：文件名不符合共存副本的命名规则", path.display());
    }
    if !path.is_file() {
        bail!("拒绝删除 {}：不是普通文件", path.display());
    }
    Ok(())
}

/// 启动一个共存副本。
pub fn launch(target: &Instance) -> Result<()> {
    if !target.is_complete() {
        bail!("编号 {} 的副本不完整，无法启动。", target.n);
    }
    std::process::Command::new(&target.exe)
        .current_dir(target.exe.parent().unwrap_or(Path::new(".")))
        .spawn()
        .with_context(|| format!("启动 {} 失败", target.exe.display()))?;
    Ok(())
}

fn check_digit(n: u8) -> Result<()> {
    if n > 9 {
        bail!("编号必须是 0~9");
    }
    Ok(())
}

/// 确认 `inst.version_dir` 仍然是 base 下最新的版本目录。
///
/// [`Install`] 是探测那一刻的快照。微信会在后台自动升级、新建版本目录，
/// 而副本启动器是**运行时**去找最新版本目录的。拿着过期的快照创建副本，
/// DLL 会落进旧目录，`create` 一路成功，双击副本却报「由于环境异常，不能继续使用。」——
/// 最难查的那种失败：每一步都说成功了。宁可在动手前报错。
fn check_version_current(inst: &Install) -> Result<()> {
    let Some((newest, _)) = crate::install::version_dirs(&inst.base).into_iter().next() else {
        bail!(
            "{} 下已经找不到含 Weixin.dll 的版本目录，请点「重新检测」。",
            inst.base.display()
        );
    };
    if newest != inst.version {
        bail!(
            "微信已从 {} 升级到 {}。\n\
             现在创建会把 DLL 写进旧版本目录，副本启动时会报「环境异常」。\n\
             请点「重新检测」后重试。",
            inst.version,
            newest
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_accepts_only_generated_names() {
        // 这些是我们自己生成的，可以删
        for name in ["Weixin0.exe", "Weixin9.exe", "Weixin.dl0", "Weixin.dl7"] {
            let p = std::env::temp_dir().join(name);
            std::fs::write(&p, b"x").unwrap();
            assert!(guard_removable(&p).is_ok(), "{name} 应允许删除");
            let _ = std::fs::remove_file(&p);
        }
    }

    /// 微信在界面开着的时候自动升级过，缓存的 `Install` 就指向了旧版本目录。
    /// 这时创建必须报错 —— 从前它会一路成功，把 DLL 写进旧目录，
    /// 副本启动时报「由于环境异常，不能继续使用。」
    #[test]
    fn create_refuses_a_stale_version_dir() {
        let base = std::env::temp_dir().join("wx-tool-stale-version");
        let _ = std::fs::remove_dir_all(&base);
        let old = base.join("4.1.11.55");
        let new = base.join("4.1.12.55");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        for d in [&old, &new] {
            std::fs::write(d.join("Weixin.dll"), b"not really a dll").unwrap();
        }
        std::fs::write(base.join("Weixin.exe"), b"not really an exe").unwrap();

        let stale = Install {
            base: base.clone(),
            version_dir: old,
            version: "4.1.11.55".into(),
        };
        let mut sink = |_: String| {};
        let err = format!("{:#}", create(&stale, 3, false, &mut sink).unwrap_err());
        assert!(err.contains("4.1.12.55"), "错误里应点明新版本号：{err}");
        // 必须在碰任何文件之前就拦下来
        assert!(!base.join("Weixin3.exe").exists(), "不得留下启动器");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 对**真实安装**的只读校验。默认跳过（没装微信的机器上跑不了），
    /// 用 `cargo test -- --ignored --nocapture` 显式运行。
    ///
    /// 微信升级后重跑这个测试，就能立刻知道特征是否还有效。
    #[test]
    #[ignore = "需要本机装有微信"]
    fn dry_run_against_real_install() {
        let inst = match crate::install::detect() {
            Ok(i) => i,
            Err(e) => {
                println!("跳过：{e:#}");
                return;
            }
        };
        println!("安装：{} 版本 {}", inst.base.display(), inst.version);

        let mut lines = Vec::new();
        let mut sink = |s: String| {
            println!("{s}");
            lines.push(s);
        };
        dry_run(&inst, 2, &mut sink).expect("真实安装上全部特征都应匹配");

        // 5 条特征各命中 1 处
        let hits = lines.iter().filter(|l| l.contains("[√]")).count();
        assert_eq!(hits, 5, "应有 5 条特征命中");
        for l in lines.iter().filter(|l| l.contains("[√]")) {
            assert!(l.contains("命中 1 处"), "每条特征都应恰好命中 1 处：{l}");
        }
    }

    /// 端到端验证：把真实的 `Weixin.exe` / `Weixin.dll` 复制到临时目录搭一个假安装，
    /// 在上面完整跑一遍 `create`，再逐字节比对结果。
    ///
    /// 这样既验证了「复制 → 打补丁 → 两阶段提交」的完整链路，又完全不碰用户的真实安装。
    /// 用 `cargo test -- --ignored --nocapture` 运行。
    #[test]
    #[ignore = "需要本机装有微信，且会占用约 400 MB 临时空间"]
    fn end_to_end_in_a_sandbox() {
        let real = match crate::install::detect() {
            Ok(i) => i,
            Err(e) => {
                println!("跳过：{e:#}");
                return;
            }
        };

        let sandbox = std::env::temp_dir().join("wx-tool-e2e");
        let _ = std::fs::remove_dir_all(&sandbox);
        let vdir = sandbox.join(&real.version);
        std::fs::create_dir_all(&vdir).unwrap();

        println!("搭建沙箱 {} …", sandbox.display());
        std::fs::copy(real.exe(), sandbox.join("Weixin.exe")).unwrap();
        std::fs::copy(real.dll(), vdir.join("Weixin.dll")).unwrap();

        let fake = crate::install::from_base(&sandbox).unwrap();
        let mut sink = |s: String| println!("{s}");
        create(&fake, 2, false, &mut sink).expect("沙箱里创建副本应当成功");

        // ---- 逐字节比对 ----
        let orig_exe = std::fs::read(fake.exe()).unwrap();
        let copy_exe = std::fs::read(sandbox.join("Weixin2.exe")).unwrap();
        let orig_dll = std::fs::read(fake.dll()).unwrap();
        let copy_dll = std::fs::read(vdir.join("Weixin.dl2")).unwrap();

        let diff = |a: &[u8], b: &[u8]| -> Vec<usize> {
            assert_eq!(a.len(), b.len(), "补丁必须等长，文件大小不能变");
            a.iter().zip(b).enumerate().filter(|(_, (x, y))| x != y).map(|(i, _)| i).collect()
        };

        let exe_diff = diff(&orig_exe, &copy_exe);
        let dll_diff = diff(&orig_dll, &copy_dll);
        println!("Weixin2.exe 改动字节：{exe_diff:x?}");
        println!("Weixin.dl2  改动字节：{dll_diff:x?}");

        assert_eq!(exe_diff.len(), 1, "启动器只应改 1 个字节");
        assert_eq!(dll_diff.len(), 4, "DLL 只应改 4 个字节");
        assert_eq!(copy_exe[exe_diff[0]], b'2');
        for &o in &dll_diff {
            assert_eq!(copy_dll[o], b'2', "改动处应写入编号 2");
        }

        // 原始文件必须一字未动
        assert_eq!(orig_exe, std::fs::read(real.exe()).unwrap(), "原版 exe 不得被改动");

        // 副本里应该能读出改名后的字符串
        let find = |h: &[u8], n: &[u8]| memchr::memmem::find(h, n).is_some();
        assert!(find(&copy_dll, b"host-redirect.xm2"));
        assert!(find(&copy_dll, b"l_conf2g"), "global_config 应变成 global_conf2g");
        assert!(find(&copy_exe, &crate::patch::wide_no_nul("Weixin.dl2")));
        assert!(
            find(&copy_dll, &crate::patch::wide_no_nul("XWeChat_App_Instance_Identity_Mutex_Nam2"))
        );
        assert!(find(&copy_dll, &crate::patch::wide_no_nul("xWechatWindo2")));

        // ---- 删除路径 ----
        let target =
            crate::install::instances(&fake).into_iter().find(|i| i.n == 2).unwrap();
        assert!(target.is_complete(), "创建后应显示为完整");
        remove(&fake, &target, &mut sink).unwrap();
        assert!(!sandbox.join("Weixin2.exe").exists());
        assert!(!vdir.join("Weixin.dl2").exists());
        assert!(fake.exe().exists() && fake.dll().exists(), "原文件必须还在");

        let _ = std::fs::remove_dir_all(&sandbox);
        println!("沙箱端到端验证通过");
    }

    #[test]
    fn guard_refuses_originals_and_anything_else() {
        for name in [
            "Weixin.exe",   // 原版启动器
            "Weixin.dll",   // 原版主模块
            "Weixin10.exe", // 长度不对
            "WeixinX.exe",  // 不是数字
            "Weixin.dlX",
            "Uninstall.exe",
        ] {
            let p = std::env::temp_dir().join(name);
            std::fs::write(&p, b"x").unwrap();
            assert!(guard_removable(&p).is_err(), "{name} 必须拒绝删除");
            let _ = std::fs::remove_file(&p);
        }
    }
}
