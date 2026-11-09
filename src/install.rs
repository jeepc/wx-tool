//! 微信安装位置的探测、版本目录选择、以及共存副本的枚举。

use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, REG_EXPAND_SZ,
    REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};

/// 一次完整的安装定位结果。
#[derive(Debug, Clone)]
pub struct Install {
    /// 安装根目录，`Weixin.exe` 所在处
    pub base: PathBuf,
    /// 当前版本子目录，`Weixin.dll` 所在处
    pub version_dir: PathBuf,
    /// 版本目录名，如 `4.1.11.55`
    pub version: String,
}

impl Install {
    pub fn exe(&self) -> PathBuf {
        self.base.join("Weixin.exe")
    }
    pub fn dll(&self) -> PathBuf {
        self.version_dir.join("Weixin.dll")
    }
}

/// 一个共存副本的状态。
#[derive(Debug, Clone)]
pub struct Instance {
    pub n: u8,
    pub exe: PathBuf,
    pub dll: PathBuf,
    pub exe_exists: bool,
    pub dll_exists: bool,
    /// 残留在**旧**版本目录里的 `Weixin.dl{n}`。微信自动升级后会出现这种情况：
    /// 副本启动器仍会去新版本目录找 DLL，找不到就起不来，需要重新创建。
    pub stale_dirs: Vec<String>,
    /// 该副本进程是否正在运行
    pub running: bool,
}

impl Instance {
    /// 副本是否可用（两个文件都在当前版本目录下就位）
    pub fn is_complete(&self) -> bool {
        self.exe_exists && self.dll_exists
    }
    /// 是否存在但残缺 —— 包括微信升级后 DLL 留在旧目录的情况
    pub fn is_broken(&self) -> bool {
        (self.exe_exists || self.dll_exists) && !self.is_complete()
    }
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// `RegCloseKey` 的 RAII 包装，避免提前 return 时漏关句柄。
struct HKeyGuard(HKEY);
impl Drop for HKeyGuard {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

/// 读取一个 REG_SZ / REG_EXPAND_SZ 字符串值。
fn read_reg_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
    let subkey_w = wide(subkey);
    let value_w = wide(value);

    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: subkey_w 是以 NUL 结尾的合法宽字符串，hkey 是合法的可写指针。
    let rc = unsafe {
        RegOpenKeyExW(root, subkey_w.as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut hkey)
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let _guard = HKeyGuard(hkey);

    // 先问长度
    let mut kind: u32 = 0;
    let mut cb: u32 = 0;
    // SAFETY: 传 null 数据指针配合非空长度指针是 RegQueryValueExW 的标准探长度用法。
    let rc = unsafe {
        RegQueryValueExW(
            hkey,
            value_w.as_ptr(),
            std::ptr::null(),
            &mut kind,
            std::ptr::null_mut(),
            &mut cb,
        )
    };
    if rc != ERROR_SUCCESS || cb == 0 {
        return None;
    }
    if kind != REG_SZ && kind != REG_EXPAND_SZ {
        return None;
    }

    let mut buf = vec![0u16; cb as usize / 2 + 1];
    let mut cb2 = cb;
    // SAFETY: buf 至少有 cb 字节容量，cb2 如实描述了它的大小。
    let rc = unsafe {
        RegQueryValueExW(
            hkey,
            value_w.as_ptr(),
            std::ptr::null(),
            &mut kind,
            buf.as_mut_ptr().cast::<u8>(),
            &mut cb2,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }

    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = OsString::from_wide(&buf[..len]).to_string_lossy().into_owned();
    if s.is_empty() { None } else { Some(s) }
}

/// 从注册表读安装路径。先查 HKCU（安装器写的位置），再退回 HKLM（全机安装）。
pub fn install_path_from_registry() -> Option<PathBuf> {
    const SUBKEY: &str = r"Software\Tencent\Weixin";
    read_reg_string(HKEY_CURRENT_USER, SUBKEY, "InstallPath")
        .or_else(|| read_reg_string(HKEY_LOCAL_MACHINE, SUBKEY, "InstallPath"))
        .map(PathBuf::from)
}

/// 把 `4.1.11.55` 这样的目录名解析成可比较的数字序列。非法格式返回 `None`。
fn parse_version(name: &str) -> Option<Vec<u32>> {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() < 2 || parts.len() > 4 {
        return None;
    }
    parts.iter().map(|p| p.parse::<u32>().ok()).collect()
}

/// 列出所有含 `Weixin.dll` 的合法版本目录，按版本号**从新到旧**排序。
///
/// BetterWX 取 `iterdir()` 的第一个结果 —— 那是文件系统顺序，不是版本顺序，
/// 微信升级后留下旧目录时会打错文件。这里按数字段比较。
pub fn version_dirs(base: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut found: Vec<(Vec<u32>, String, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let ver = parse_version(&name)?;
            let path = e.path();
            path.join("Weixin.dll").is_file().then_some((ver, name, path))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, n, p)| (n, p)).collect()
}

/// 从安装根目录推导出完整的 [`Install`]。
pub fn from_base(base: &Path) -> Result<Install> {
    let base = base
        .canonicalize()
        .with_context(|| format!("无法访问目录 {}", base.display()))?;
    let base = strip_verbatim(base);

    let exe = base.join("Weixin.exe");
    if !exe.is_file() {
        bail!("{} 里没有 Weixin.exe，这不像是微信的安装目录", base.display());
    }
    check_pe64(&exe)?;

    let dirs = version_dirs(&base);
    let Some((version, version_dir)) = dirs.into_iter().next() else {
        bail!(
            "{} 下找不到含 Weixin.dll 的版本目录（形如 4.1.11.55）",
            base.display()
        );
    };

    Ok(Install { base, version_dir, version })
}

/// 自动探测：注册表 → 常见默认路径。
pub fn detect() -> Result<Install> {
    if let Some(p) = install_path_from_registry()
        && let Ok(inst) = from_base(&p)
    {
        return Ok(inst);
    }
    for candidate in [r"C:\Program Files\Tencent\Weixin", r"C:\Program Files (x86)\Tencent\Weixin"]
    {
        if let Ok(inst) = from_base(Path::new(candidate)) {
            return Ok(inst);
        }
    }
    bail!(
        "没能自动找到微信安装目录。\n\
         已尝试：注册表 HKCU/HKLM 的 Software\\Tencent\\Weixin\\InstallPath，以及默认安装路径。\n\
         请用「手动选择」按钮指定 Weixin.exe。"
    )
}

/// 去掉 `canonicalize` 产生的 `\\?\` 前缀，纯粹为了显示好看。
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

/// 确认是 64 位 PE 文件。挡住「路径指到了快捷方式 / .tmp / 3.9 版本」这类误操作。
pub fn check_pe64(path: &Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("无法打开 {}", path.display()))?;

    let mut mz = [0u8; 2];
    f.read_exact(&mut mz).with_context(|| format!("{} 太小", path.display()))?;
    if &mz != b"MZ" {
        bail!("{} 不是 PE 文件（缺少 MZ 头）", path.display());
    }

    f.seek(SeekFrom::Start(0x3C))?;
    let mut lfanew = [0u8; 4];
    f.read_exact(&mut lfanew)?;
    let pe_off = u32::from_le_bytes(lfanew) as u64;

    f.seek(SeekFrom::Start(pe_off))?;
    let mut hdr = [0u8; 6];
    f.read_exact(&mut hdr)
        .with_context(|| format!("{} 的 PE 头越界", path.display()))?;
    if &hdr[..4] != b"PE\0\0" {
        bail!("{} 不是 PE 文件（PE 签名不对）", path.display());
    }
    let machine = u16::from_le_bytes([hdr[4], hdr[5]]);
    if machine != 0x8664 {
        bail!(
            "{} 不是 x64 程序（machine = {machine:#06x}）。本工具只支持 64 位微信。",
            path.display()
        );
    }
    Ok(())
}

/// 枚举当前正在运行的进程名（小写）及其 PID。
pub fn running_processes() -> Vec<(u32, String)> {
    let mut out = Vec::new();
    // SAFETY: 标准的 ToolHelp 快照用法；失败时返回 INVALID_HANDLE_VALUE，下面已判断。
    let snap: HANDLE = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return out;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

    // SAFETY: entry.dwSize 已按 API 要求填好，snap 是有效快照句柄。
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) } != 0;
    while ok {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        let name = OsString::from_wide(&entry.szExeFile[..len])
            .to_string_lossy()
            .to_ascii_lowercase();
        out.push((entry.th32ProcessID, name));
        ok = unsafe { Process32NextW(snap, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snap) };
    out
}

/// 扫描 0~9 十个槽位的当前状态。
pub fn instances(inst: &Install) -> Vec<Instance> {
    let procs = running_processes();
    let others: Vec<(String, PathBuf)> = version_dirs(&inst.base)
        .into_iter()
        .filter(|(name, _)| *name != inst.version)
        .collect();

    (0..=9u8)
        .map(|n| {
            let exe = inst.base.join(format!("Weixin{n}.exe"));
            let dll = inst.version_dir.join(format!("Weixin.dl{n}"));
            let proc_name = format!("weixin{n}.exe");
            let stale_dirs = others
                .iter()
                .filter(|(_, dir)| dir.join(format!("Weixin.dl{n}")).is_file())
                .map(|(name, _)| name.clone())
                .collect();
            Instance {
                n,
                exe_exists: exe.is_file(),
                dll_exists: dll.is_file(),
                exe,
                dll,
                stale_dirs,
                running: procs.iter().any(|(_, p)| *p == proc_name),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_sort_is_numeric_not_lexicographic() {
        let mut v = [
            parse_version("4.1.9.10").unwrap(),
            parse_version("4.1.11.55").unwrap(),
            parse_version("4.0.6.26").unwrap(),
        ];
        v.sort_by(|a, b| b.cmp(a));
        // 字典序会把 "4.1.9.10" 排在 "4.1.11.55" 前面，数字序不会
        assert_eq!(v[0], vec![4, 1, 11, 55]);
    }

    #[test]
    fn parse_version_rejects_non_version_dirs() {
        assert!(parse_version("locales").is_none());
        assert!(parse_version("4.x").is_none());
        assert!(parse_version("4").is_none());
        assert!(parse_version("1.2.3.4.5").is_none());
        assert_eq!(parse_version("4.1.11.55"), Some(vec![4, 1, 11, 55]));
    }
}
