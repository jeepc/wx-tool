//! 管理员权限检测与 UAC 提权重启。
//!
//! 策略：**启动时不提权**。只读操作（探测安装、列出副本、试运行检查）标准用户就能做，
//! 不该弹 UAC。只有真的要往 `C:\Program Files` 写东西、而且当前确实写不进去时，
//! 才提示用户提权重启。

use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// 当前进程是否以管理员权限运行。
pub fn is_elevated() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess 返回伪句柄，token 是合法可写指针。
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret_len = 0u32;
    // SAFETY: elevation 的大小如实传入，token 有效。
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
    };
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// 真正试着往目录里写一个探针文件。
///
/// 比「是不是管理员」准确：有些机器上 `Program Files` 的 ACL 被改过，
/// 普通用户也能写；反过来也可能提了权还是写不进去（只读介质、EDR 拦截）。
pub fn can_write_dir(dir: &Path) -> bool {
    let probe = dir.join(".wx-tool-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 按 MSVCRT 的规则给单个参数加引号。
///
/// 反斜杠只有在紧挨着引号时才有转义含义，此时每个都要翻倍。搞错的话，
/// 像 `C:\Program Files\Tencent\Weixin\` 这种以反斜杠结尾的路径在提权重启后会被拆坏。
pub fn quote_arg(arg: &OsStr) -> OsString {
    let s = arg.to_string_lossy();
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        return arg.to_os_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // 前面累积的反斜杠翻倍，再转义这个引号
                out.push_str(&"\\".repeat(backslashes + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // 结尾的反斜杠要翻倍，否则会转义掉我们自己加的收尾引号
    out.push_str(&"\\".repeat(backslashes));
    out.push('"');
    OsString::from(out)
}

/// 以管理员身份重新启动本程序，并把 `args` 作为命令行参数传过去。
///
/// 成功返回后调用方应当立即退出当前进程 —— 提权后的新进程会接管工作。
pub fn relaunch_elevated(args: &[OsString]) -> Result<()> {
    let exe = std::env::current_exe().context("无法取得当前程序路径")?;
    let params: Vec<String> = args
        .iter()
        .map(|a| quote_arg(a).to_string_lossy().into_owned())
        .collect();
    let params = params.join(" ");

    let verb = wide(OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let par = wide(OsStr::new(&params));

    // SAFETY: 四个宽字符串都以 NUL 结尾且在调用期间存活；窗口句柄传 null 表示无父窗口。
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            if params.is_empty() { std::ptr::null() } else { par.as_ptr() },
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };

    // ShellExecuteW 返回值 <= 32 表示失败
    let code = rc as usize;
    match code {
        c if c > 32 => Ok(()),
        // ERROR_CANCELLED：用户在 UAC 对话框上点了「否」
        1223 => bail!("已取消提权。创建共存副本需要管理员权限。"),
        c => bail!("提权重启失败（ShellExecute 返回 {c}）"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    /// 用系统自己的解析器验证引号规则 —— 这是唯一靠谱的验证方式。
    fn roundtrip(args: &[&str]) {
        let quoted: Vec<String> = args
            .iter()
            .map(|a| quote_arg(OsStr::new(a)).to_string_lossy().into_owned())
            .collect();
        // CommandLineToArgvW 把第一个 token 当成程序名，前面补一个占位
        let cmdline = format!("prog.exe {}", quoted.join(" "));
        let w: Vec<u16> = OsStr::new(&cmdline).encode_wide().chain(std::iter::once(0)).collect();

        let mut argc = 0i32;
        let argv = unsafe { CommandLineToArgvW(w.as_ptr(), &mut argc) };
        assert!(!argv.is_null(), "解析失败: {cmdline}");

        let mut parsed = Vec::new();
        for i in 1..argc {
            let p = unsafe { *argv.offset(i as isize) };
            let len = (0..).take_while(|&j| unsafe { *p.offset(j) } != 0).count();
            let slice = unsafe { std::slice::from_raw_parts(p, len) };
            parsed.push(String::from_utf16_lossy(slice));
        }
        unsafe { LocalFree(argv.cast()) };

        let expected: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        assert_eq!(parsed, expected, "命令行 `{cmdline}` 解析结果不符");
    }

    #[test]
    fn quoting_survives_the_system_parser() {
        roundtrip(&["simple"]);
        roundtrip(&[r"C:\Program Files\Tencent\Weixin"]);
        roundtrip(&[r"C:\Program Files\Tencent\Weixin\"]); // 结尾反斜杠
        roundtrip(&["--install-path", r"C:\path with space\"]);
        roundtrip(&[r#"has"quote"#]);
        roundtrip(&["中文路径 带空格"]);
        roundtrip(&["create", "2", r"C:\a b\c\"]);
    }

    #[test]
    fn plain_args_are_not_quoted_needlessly() {
        assert_eq!(quote_arg(OsStr::new("create")), OsString::from("create"));
        assert_eq!(quote_arg(OsStr::new("2")), OsString::from("2"));
    }
}
