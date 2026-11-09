//! 微信共存补丁的具体特征定义。
//!
//! 全部特征都已在本机 WeChat 4.1.11.55 上实测验证（见每条的注释）。
//! 所有替换都是等长的，因此文件大小不变，无需修 PE 校验和/重定位。

use crate::patch::{Edit, Expect, Pattern, PatchSpec, Rule, wide_no_nul};

/// 编号 n（0~9）对应的 ASCII 数字字节。
fn digit(n: u8) -> u8 {
    debug_assert!(n <= 9);
    b'0' + n
}

/// 尾字节替换成编号的字面量规则 —— 四条字符串补丁共用的形状。
///
/// 偏移由 `len()-1` 推导而来，所以不存在手写偏移写错的可能。
fn tail_digit(find: Vec<u8>, n: u8) -> Rule {
    let mut replace = find.clone();
    *replace.last_mut().expect("特征不能为空") = digit(n);
    Rule::Literal { find, replace }
}

/// `Weixin.exe` 上的补丁：把运行时 `LoadLibraryW` 用的宽字符串
/// `Weixin.dll` 改成 `Weixin.dl{n}`，让副本启动器去加载副本 DLL。
///
/// 实测 4.1.11.55：命中 1 处 @ `0x202110`，写入 `0x202122`。
///
/// 注意：`Weixin.exe` 里 `0x203008` 还有一个 **ASCII** 的 `Weixin.dll`，
/// 那是一张普通的模块名表（`VoipEngine.dll` / `XNet.dll` / … 挨在一起），
/// 不是导入表也不是加载路径。BetterWX 刻意不动它，我们也不动 ——
/// 改了会破坏那张表。经 PE 解析确认 `Weixin.exe` 只静态导入 `KERNEL32.dll`，
/// 所以宽字符串就是唯一的加载入口。
pub fn exe_specs(n: u8) -> Vec<PatchSpec> {
    vec![PatchSpec {
        name: "Weixin.dll → Weixin.dl{n}（启动器加载的 DLL 名）",
        variants: vec![("UTF-16LE 字符串", tail_digit(wide_no_nul("Weixin.dll"), n))],
        expect: Expect::ExactlyOne,
        diag_lead: None,
    }]
}

/// `Weixin.dll` 上的四条补丁，分别隔离：配置存储、自动登录、单实例互斥锁、窗口名。
pub fn dll_specs(n: u8) -> Vec<PatchSpec> {
    vec![
        global_config_spec(n),
        PatchSpec {
            // 实测 4.1.11.55：命中 1 处 @ 0x8E74A4D，写入 0x8E74A5D
            name: "host-redirect.xml → host-redirect.xm{n}（自动登录隔离）",
            variants: vec![("ASCII 字符串", tail_digit(b"host-redirect.xml".to_vec(), n))],
            expect: Expect::AtLeastOne,
            diag_lead: None,
        },
        PatchSpec {
            // 实测 4.1.11.55：命中 1 处 @ 0x84EDCA0，写入 0x84EDCEE
            // 这是多开的关键 —— 互斥锁改名后，副本与原版不再互相阻塞。
            name: "单实例互斥锁改名（多开的关键）",
            variants: vec![(
                "UTF-16LE 字符串",
                tail_digit(wide_no_nul("XWeChat_App_Instance_Identity_Mutex_Name"), n),
            )],
            expect: Expect::AtLeastOne,
            diag_lead: None,
        },
        PatchSpec {
            // 实测 4.1.11.55：命中 1 处 @ 0x84EDCF8，写入 0x84EDD10
            name: "窗口名改名",
            variants: vec![(
                "UTF-16LE 字符串",
                tail_digit(wide_no_nul("xWechatWindow"), n),
            )],
            expect: Expect::AtLeastOne,
            diag_lead: None,
        },
    ]
}

/// `global_config` → `global_conf{n}g`：唯一的一条**代码**补丁，只改一个字节。
///
/// 微信把 `global_config` 这个 13 字节的字符串直接用立即数写进栈上的
/// `std::string` 小缓冲区（SSO）。不同版本的编译器生成方式不一样，所以要多形态适配。
///
/// 把第 11 个字符 `i` 换成数字，运行时字符串就变成 `global_conf2g` ——
/// 长度不变（仍是 13），所以 SSO 的 size/capacity 字段无需跟着改。
fn global_config_spec(n: u8) -> PatchSpec {
    // 两种形态共有的前导：mov rax, "global_c"
    // 实测这 10 字节在 189 MB 的 Weixin.dll 里**只出现一次**，是个完美的定位锚点。
    const LEAD: &str = "48 B8 67 6C 6F 62 61 6C 5F 63";

    // 形态 A —— WeChat 4.0.x 的代码生成（BetterWX 原版特征）。
    // 实测在 4.1.11.55 上命中 0 处。
    //   48 B8 67 6C 6F 62 61 6C 5F 63   mov  rax, "global_c"
    //   48 89 05 ?? ?? ?? ??            mov  [rip+d32], rax          ; 写 [0..8)
    //   C7 05 ?? ?? ?? ?? 6F 6E 66 69   mov  dword [rip+d32], "onfi" ; 写 [8..12)
    //   66 C7 05 ?? ?? ?? ?? 67 00      mov  word  [rip+d32], "g\0"  ; 写 [12..14)
    // 要改的是 "onfi" 里的 'i'，位于模式偏移 26。
    let variant_a = Rule::Masked {
        pattern: Pattern::parse(
            "48 B8 67 6C 6F 62 61 6C 5F 63
             48 89 05 ?? ?? ?? ??
             C7 05 ?? ?? ?? ?? 6F 6E 66 69
             66 C7 05 ?? ?? ?? ?? 67 00",
        )
        .expect("形态 A 特征串合法"),
        edits: vec![Edit { offset: 26, value: digit(n) }],
    };

    // 形态 B —— WeChat 4.1.x 的代码生成。实测命中 1 处 @ 0x67001，写入 0x6701A。
    // 编译器改成了两条**重叠**的 8 字节立即数写入：
    //   48 B8 67 6C 6F 62 61 6C 5F 63   mov  rax, "global_c"
    //   48 89 05 ?? ?? ?? ??            mov  [rip+d32], rax   ; 写 [0..8)
    //   48 B8 6C 5F 63 6F 6E 66 69 67   mov  rax, "l_config"
    //   48 89 05 ?? ?? ?? ??            mov  [rip+d32], rax   ; 写 [5..13)，与上一条重叠
    // 两条 store 的 RIP 位移相差正好 5，拼出 "global_config"。
    // 要改的是 "l_config" 里的 'i'（字符串下标 11 = 5 + 6），位于模式偏移 25。
    let variant_b = Rule::Masked {
        pattern: Pattern::parse(
            "48 B8 67 6C 6F 62 61 6C 5F 63
             48 89 05 ?? ?? ?? ??
             48 B8 6C 5F 63 6F 6E 66 69 67
             48 89 05 ?? ?? ?? ??",
        )
        .expect("形态 B 特征串合法"),
        edits: vec![Edit { offset: 25, value: digit(n) }],
    };

    PatchSpec {
        name: "global_config → global_conf{n}g（设置隔离）",
        // 先试 B（当前版本），再试 A（旧版本）
        variants: vec![("形态 B / 4.1.x", variant_b), ("形态 A / 4.0.x", variant_a)],
        expect: Expect::ExactlyOne,
        diag_lead: Some(Pattern::parse(LEAD).expect("前导特征合法")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::plan;

    /// 用固定种子的 LCG 生成填充字节，保证可复现且不会碰巧包含特征。
    fn filler(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                // 压到 0x01..=0x7F 之外的范围以降低与 ASCII 特征碰撞的可能
                ((s >> 33) as u8) | 0x80
            })
            .collect()
    }

    /// 形态 B 的 34 字节，**含真实的 RIP 位移**（取自真实 DLL 的 0x67001）。
    /// 用真实位移而不是 0，才能证明通配符确实生效，而不是碰巧匹配到零字节。
    const VARIANT_B_REAL: &[u8] = &[
        0x48, 0xB8, 0x67, 0x6C, 0x6F, 0x62, 0x61, 0x6C, 0x5F, 0x63, // mov rax,"global_c"
        0x48, 0x89, 0x05, 0xE6, 0x65, 0xD3, 0x0A, // mov [rip+0x0AD365E6],rax
        0x48, 0xB8, 0x6C, 0x5F, 0x63, 0x6F, 0x6E, 0x66, 0x69, 0x67, // mov rax,"l_config"
        0x48, 0x89, 0x05, 0xDA, 0x65, 0xD3, 0x0A, // mov [rip+0x0AD365DA],rax
    ];

    const VARIANT_A_SYNTH: &[u8] = &[
        0x48, 0xB8, 0x67, 0x6C, 0x6F, 0x62, 0x61, 0x6C, 0x5F, 0x63, //
        0x48, 0x89, 0x05, 0x07, 0x78, 0xC3, 0x07, //
        0xC7, 0x05, 0x05, 0x78, 0xC3, 0x07, 0x6F, 0x6E, 0x66, 0x69, //
        0x66, 0xC7, 0x05, 0x00, 0x78, 0xC3, 0x07, 0x67, 0x00,
    ];

    /// 造一个假的 Weixin.dll：4 条特征都埋进去，位置各不相同。
    fn fake_dll(variant: &[u8]) -> (Vec<u8>, usize, usize, usize, usize) {
        let mut buf = filler(4096, 0xDEADBEEF);
        let off_cfg = buf.len();
        buf.extend_from_slice(variant);
        buf.extend_from_slice(&filler(500, 1));

        let off_mutex = buf.len();
        buf.extend_from_slice(&wide_no_nul("XWeChat_App_Instance_Identity_Mutex_Name"));
        // 真实文件里互斥锁名和窗口名只隔 9 个字节，复刻这个紧邻布局，
        // 才能测出「某条特征多读了几个字节、串到邻居去了」的 bug。
        buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let off_window = buf.len();
        buf.extend_from_slice(&wide_no_nul("xWechatWindow"));
        buf.extend_from_slice(&filler(300, 2));

        let off_host = buf.len();
        buf.extend_from_slice(b"host-redirect.xml");
        buf.extend_from_slice(&filler(1000, 3));

        (buf, off_cfg, off_mutex, off_window, off_host)
    }

    fn diff_offsets(a: &[u8], b: &[u8]) -> Vec<usize> {
        a.iter()
            .zip(b)
            .enumerate()
            .filter(|(_, (x, y))| x != y)
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn variant_b_patches_exactly_one_byte_at_offset_25() {
        let (orig, off_cfg, ..) = fake_dll(VARIANT_B_REAL);
        let mut buf = orig.clone();
        let p = plan(&dll_specs(2), &buf).unwrap();
        p.apply(&mut buf).unwrap();
        assert!(diff_offsets(&orig, &buf).contains(&(off_cfg + 25)));
        assert_eq!(buf[off_cfg + 25], b'2');
    }

    #[test]
    fn variant_a_patches_exactly_one_byte_at_offset_26() {
        let (orig, off_cfg, ..) = fake_dll(VARIANT_A_SYNTH);
        let mut buf = orig.clone();
        let p = plan(&dll_specs(7), &buf).unwrap();
        p.apply(&mut buf).unwrap();
        assert!(diff_offsets(&orig, &buf).contains(&(off_cfg + 26)));
        assert_eq!(buf[off_cfg + 26], b'7');
    }

    /// 最有价值的一条断言：改动的字节集合必须**恰好**是预期的那几个，一个不多一个不少。
    #[test]
    fn only_the_intended_bytes_change() {
        let (orig, off_cfg, off_mutex, off_window, off_host) = fake_dll(VARIANT_B_REAL);
        let mut buf = orig.clone();
        plan(&dll_specs(2), &buf).unwrap().apply(&mut buf).unwrap();

        let mut expected = vec![
            off_cfg + 25,
            off_mutex + wide_no_nul("XWeChat_App_Instance_Identity_Mutex_Name").len() - 1,
            off_window + wide_no_nul("xWechatWindow").len() - 1,
            off_host + b"host-redirect.xml".len() - 1,
        ];
        expected.sort_unstable();

        let mut actual = diff_offsets(&orig, &buf);
        actual.sort_unstable();
        assert_eq!(actual, expected, "只允许改动这 4 个字节");
        assert_eq!(buf.len(), orig.len(), "文件长度必须不变");
    }

    #[test]
    fn patched_strings_read_back_correctly() {
        let (_, off_cfg, off_mutex, off_window, off_host) = fake_dll(VARIANT_B_REAL);
        let (mut buf, ..) = fake_dll(VARIANT_B_REAL);
        plan(&dll_specs(3), &buf).unwrap().apply(&mut buf).unwrap();

        // "l_config" 立即数变成 "l_conf3g"，拼出 global_conf3g
        assert_eq!(&buf[off_cfg + 19..off_cfg + 27], b"l_conf3g");
        assert_eq!(&buf[off_host..off_host + 17], b"host-redirect.xm3");

        let read_wide = |off: usize, len: usize| -> String {
            buf[off..off + len]
                .chunks(2)
                .map(|c| c[0] as char)
                .collect()
        };
        assert_eq!(
            read_wide(off_mutex, 79),
            "XWeChat_App_Instance_Identity_Mutex_Nam3"
        );
        assert_eq!(read_wide(off_window, 25), "xWechatWindo3");
    }

    #[test]
    fn exe_patch_changes_one_byte() {
        let mut buf = filler(2048, 42);
        let off = buf.len();
        buf.extend_from_slice(&wide_no_nul("Weixin.dll"));
        buf.extend_from_slice(&filler(2048, 43));
        let orig = buf.clone();

        plan(&exe_specs(5), &buf).unwrap().apply(&mut buf).unwrap();
        assert_eq!(diff_offsets(&orig, &buf), vec![off + 18]);
        assert_eq!(buf[off + 18], b'5');
    }

    #[test]
    fn every_digit_round_trips_including_zero() {
        for n in 0..=9u8 {
            let (mut buf, off_cfg, ..) = fake_dll(VARIANT_B_REAL);
            plan(&dll_specs(n), &buf).unwrap().apply(&mut buf).unwrap();
            assert_eq!(buf[off_cfg + 25], b'0' + n, "编号 {n} 应写入对应 ASCII 数字");
        }
    }

    #[test]
    fn missing_global_config_aborts_with_diagnostic_hexdump() {
        // 埋入前导但破坏后面的指令 —— 模拟微信换了代码生成方式
        let mut broken = VARIANT_B_REAL.to_vec();
        broken[17] = 0x90; // 把第三条指令的 48 改成 nop，两种形态都匹配不上
        broken[18] = 0x90;
        let (buf, ..) = fake_dll(&broken);

        let err = plan(&dll_specs(2), &buf).unwrap_err().to_string();
        assert!(err.contains("找到了前导特征"), "应指出只是形态变了: {err}");
        assert!(err.contains("48 B8 67 6C"), "应附带上下文字节 dump: {err}");
    }

    #[test]
    fn totally_unrelated_file_is_rejected() {
        let buf = filler(10_000, 999);
        assert!(plan(&dll_specs(2), &buf).is_err());
        assert!(plan(&exe_specs(2), &buf).is_err());
    }

    #[test]
    fn duplicate_code_pattern_aborts_without_writing() {
        let (mut buf, ..) = fake_dll(VARIANT_B_REAL);
        // 再埋一份 global_config 代码 -> ExactlyOne 应当拒绝
        buf.extend_from_slice(VARIANT_B_REAL);
        let orig = buf.clone();
        assert!(plan(&dll_specs(2), &buf).is_err());
        assert_eq!(buf, orig, "失败时不得写入任何字节");
    }
}
