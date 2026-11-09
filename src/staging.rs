//! 「先写临时文件、全部成功后再改名就位」的两阶段提交。
//!
//! BetterWX 是先写 `Weixin{n}.exe` 再去处理 `Weixin.dll` 的。如果 DLL 的特征匹配失败
//! （比如微信升级到了不认识的版本），用户就会留下一个指向根本不存在的 `Weixin.dl{n}`
//! 的启动器 —— 双击必然失败，而且不知道该删哪个文件。这里两个文件都补完才一起就位。

use anyhow::{Context, Result, bail};
use memmap2::MmapMut;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

/// 一个尚未就位的输出文件。未 `commit` 就被丢弃时，临时文件会被自动删除。
pub struct StagedFile {
    tmp: PathBuf,
    dest: PathBuf,
    committed: bool,
}

impl StagedFile {
    /// 把 `src` 复制成 `dest` 旁边的临时文件。临时文件与 `dest` 同目录，
    /// 因此必然同卷，最后的改名是原子的。
    pub fn copy_from(src: &Path, dest: PathBuf) -> Result<Self> {
        if src == dest {
            bail!("源文件与目标文件相同：{}", src.display());
        }
        let mut tmp = dest.clone();
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        tmp.set_file_name(format!("{name}.wxtmp"));

        // 上一次异常退出可能留下过临时文件
        let _ = std::fs::remove_file(&tmp);

        std::fs::copy(src, &tmp).with_context(|| {
            format!("复制 {} → {} 失败", src.display(), tmp.display())
        })?;

        Ok(StagedFile { tmp, dest, committed: false })
    }

    /// 以读写方式映射临时文件的全部内容。
    pub fn map_mut(&self) -> Result<MmapMut> {
        use std::os::windows::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            // share_mode(0)：独占打开，其他进程连开都开不了
            .share_mode(0)
            .open(&self.tmp)
            .with_context(|| format!("无法打开临时文件 {}", self.tmp.display()))?;

        // SAFETY: 内存映射的安全前提是「映射期间文件内容不被外部改动、文件不被截断」。
        // 这里三重保证：
        //   1. 这个文件是我们自己几秒前创建的临时文件，还没有对外暴露过路径；
        //   2. 上面用 share_mode(0) 独占打开，其他进程无法获得句柄；
        //   3. Windows 不允许截断或删除存在活动 section 对象的文件。
        unsafe { MmapMut::map_mut(&file) }
            .with_context(|| format!("无法映射 {}", self.tmp.display()))
    }

    /// 把临时文件改名到最终位置。已存在的同名文件会被替换。
    ///
    /// 调用前必须先释放 `map_mut` 返回的映射 —— 存在活动 section 时改名会失败。
    pub fn commit(mut self) -> Result<()> {
        std::fs::rename(&self.tmp, &self.dest).with_context(|| {
            format!("改名 {} → {} 失败", self.tmp.display(), self.dest.display())
        })?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// 目标目录所在卷的可用字节数。
pub fn free_space(dir: &Path) -> Option<u64> {
    let w: Vec<u16> = OsStr::new(dir).encode_wide().chain(std::iter::once(0)).collect();
    let mut free: u64 = 0;
    // SAFETY: w 是以 NUL 结尾的合法宽字符串路径，free 是合法可写指针，
    // 其余两个输出参数传 null 是 API 允许的。
    let ok = unsafe {
        GetDiskFreeSpaceExW(w.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut())
    };
    (ok != 0).then_some(free)
}

/// 检查磁盘空间是否够放下这次要生成的文件。
pub fn ensure_space(dir: &Path, needed: u64) -> Result<()> {
    // 留 64 MB 余量，避免刚好填满卷
    let want = needed.saturating_add(64 * 1024 * 1024);
    if let Some(free) = free_space(dir)
        && free < want
    {
        bail!(
            "{} 所在磁盘空间不足：需要约 {:.1} GB，可用 {:.1} GB",
            dir.display(),
            want as f64 / 1e9,
            free as f64 / 1e9
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join("wx-tool-tests");
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    #[test]
    fn drop_without_commit_leaves_nothing_behind() {
        let src = scratch("src_a.bin");
        let dest = scratch("dest_a.bin");
        std::fs::write(&src, b"hello").unwrap();
        let _ = std::fs::remove_file(&dest);

        let tmp_path = {
            let staged = StagedFile::copy_from(&src, dest.clone()).unwrap();
            let t = staged.tmp.clone();
            assert!(t.is_file(), "临时文件应已创建");
            t
        }; // 这里 drop

        assert!(!tmp_path.exists(), "未提交的临时文件必须被清理");
        assert!(!dest.exists(), "未提交时不得产生目标文件");
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn commit_moves_into_place_and_replaces_existing() {
        let src = scratch("src_b.bin");
        let dest = scratch("dest_b.bin");
        std::fs::write(&src, b"new-content").unwrap();
        std::fs::write(&dest, b"old").unwrap();

        let staged = StagedFile::copy_from(&src, dest.clone()).unwrap();
        let tmp = staged.tmp.clone();
        staged.commit().unwrap();

        assert!(!tmp.exists(), "提交后临时文件应消失");
        assert_eq!(std::fs::read(&dest).unwrap(), b"new-content", "应替换旧文件");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn patch_through_mmap_then_commit() {
        let src = scratch("src_c.bin");
        let dest = scratch("dest_c.bin");
        std::fs::write(&src, b"AAAA").unwrap();
        let _ = std::fs::remove_file(&dest);

        let staged = StagedFile::copy_from(&src, dest.clone()).unwrap();
        {
            let mut m = staged.map_mut().unwrap();
            m[2] = b'Z';
            m.flush().unwrap();
        } // 必须先释放映射，否则改名会失败
        staged.commit().unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"AAZA");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn refuses_when_src_equals_dest() {
        let p = scratch("same.bin");
        std::fs::write(&p, b"x").unwrap();
        assert!(StagedFile::copy_from(&p, p.clone()).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
