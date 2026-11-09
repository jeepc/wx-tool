//! 通用的二进制补丁引擎：掩码模式匹配 + 等长字面量替换。
//!
//! 语义对齐 BetterWX 的 `_utils.py`（`replace` / `wildcard_replace`），但有几点刻意的差异：
//!
//! 1. 模式缺失时**报错**，而不是像原版那样打印 `[WARN] SKIPPED!` 后继续 ——
//!    静默跳过会产出一个「看起来成功了但其实没隔离干净」的坏副本。
//! 2. 不做「already patched」幂等检测。我们永远从原始文件复制，源必然是干净的。
//! 3. 代码补丁要求**恰好命中一处**；命中多处说明模式不够特异，宁可报错也不乱改代码。

use anyhow::{Context, Result, anyhow, bail};

/// 掩码模式。`mask[i] == true` 表示 `bytes[i]` 必须精确匹配，`false` 表示通配（`??`）。
#[derive(Debug, Clone)]
pub struct Pattern {
    bytes: Vec<u8>,
    mask: Vec<bool>,
    /// 最长连续具体字节段的起点，用作 `memmem` 预筛锚点
    anchor_off: usize,
    anchor: Vec<u8>,
}

impl Pattern {
    /// 解析 `"48 B8 67 ?? ?? 05"` 形式的模式串。空白（含换行）无关紧要，`??` 为通配。
    ///
    /// 半通配（如 `?A`）和奇数长度都视为错误 —— 与 BetterWX 的 `wildcard_tokenize` 一致。
    pub fn parse(spec: &str) -> Result<Pattern> {
        let compact: String = spec
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_uppercase();

        if !compact.len().is_multiple_of(2) {
            bail!("模式长度为奇数（{} 个字符），无法按字节切分", compact.len());
        }

        let chars: Vec<char> = compact.chars().collect();
        let mut bytes = Vec::with_capacity(chars.len() / 2);
        let mut mask = Vec::with_capacity(chars.len() / 2);

        for pair in chars.chunks(2) {
            let (a, b) = (pair[0], pair[1]);
            match (a, b) {
                ('?', '?') => {
                    bytes.push(0);
                    mask.push(false);
                }
                _ if a == '?' || b == '?' => bail!("非法的半通配字节 `{a}{b}`，通配必须写成 `??`"),
                _ => {
                    let hi = a.to_digit(16).with_context(|| format!("非法的十六进制字符 `{a}`"))?;
                    let lo = b.to_digit(16).with_context(|| format!("非法的十六进制字符 `{b}`"))?;
                    bytes.push((hi * 16 + lo) as u8);
                    mask.push(true);
                }
            }
        }

        Pattern::build(bytes, mask)
    }

    fn build(bytes: Vec<u8>, mask: Vec<bool>) -> Result<Pattern> {
        if bytes.is_empty() {
            bail!("模式为空");
        }
        // 选最长的连续具体字节段作为锚点：段越长，memmem 预筛的误命中越少。
        let (mut best_off, mut best_len) = (0usize, 0usize);
        let (mut cur_off, mut cur_len) = (0usize, 0usize);
        for (i, &m) in mask.iter().enumerate() {
            if m {
                if cur_len == 0 {
                    cur_off = i;
                }
                cur_len += 1;
                if cur_len > best_len {
                    best_off = cur_off;
                    best_len = cur_len;
                }
            } else {
                cur_len = 0;
            }
        }
        if best_len == 0 {
            bail!("模式全部是通配符，无法用于搜索");
        }
        let anchor = bytes[best_off..best_off + best_len].to_vec();
        Ok(Pattern { bytes, mask, anchor_off: best_off, anchor })
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn matches_at(&self, hay: &[u8], start: usize) -> bool {
        let Some(end) = start.checked_add(self.bytes.len()) else {
            return false;
        };
        if end > hay.len() {
            return false;
        }
        let window = &hay[start..end];
        self.bytes
            .iter()
            .zip(&self.mask)
            .zip(window)
            .all(|((&want, &must), &got)| !must || want == got)
    }

    /// 找出所有匹配位置（含重叠）。先用 `memmem` 搜锚点段，再逐字节校验掩码 ——
    /// 在 189 MB 的 `Weixin.dll` 上这比朴素双重循环快一两个数量级。
    pub fn find_all(&self, hay: &[u8]) -> Vec<usize> {
        let n = self.bytes.len();
        if n == 0 || hay.len() < n {
            return Vec::new();
        }

        let finder = memchr::memmem::Finder::new(&self.anchor);
        let mut out = Vec::new();
        let mut cursor = 0usize;
        // 每次只前进 1 字节，保证重叠匹配也能被发现 —— 我们的安全模型依赖
        // 「恰好命中一处」的判断，漏计数会让一个本该报错的文件被悄悄放行。
        while let Some(rel) = finder.find(&hay[cursor..]) {
            let hit = cursor + rel;
            cursor = hit + 1;
            if let Some(start) = hit.checked_sub(self.anchor_off)
                && self.matches_at(hay, start)
            {
                out.push(start);
            }
            if cursor >= hay.len() {
                break;
            }
        }
        out
    }
}

/// 命中模式后要写入的一个字节：`offset` 是相对模式起点的偏移。
#[derive(Debug, Clone, Copy)]
pub struct Edit {
    pub offset: usize,
    pub value: u8,
}

/// 一条补丁规则。
#[derive(Debug, Clone)]
pub enum Rule {
    /// 等长字面量全量替换（对应 BetterWX 的 `replace()`）。
    Literal { find: Vec<u8>, replace: Vec<u8> },
    /// 掩码模式 + 定点字节改写（对应 BetterWX 的 `wildcard_replace()`）。
    Masked { pattern: Pattern, edits: Vec<Edit> },
}

/// 一次扫描的产物：命中的起始偏移列表，以及要写入的 (绝对偏移, 新字节) 列表。
type ScanResult = (Vec<usize>, Vec<(usize, u8)>);

impl Rule {
    fn scan(&self, data: &[u8]) -> Result<ScanResult> {
        match self {
            Rule::Literal { find, replace } => {
                if find.len() != replace.len() {
                    bail!(
                        "字面量替换必须等长（find {} 字节，replace {} 字节）",
                        find.len(),
                        replace.len()
                    );
                }
                let offsets: Vec<usize> =
                    memchr::memmem::find_iter(data, find.as_slice()).collect();
                let mut edits = Vec::new();
                for &off in &offsets {
                    for (i, (&f, &r)) in find.iter().zip(replace.iter()).enumerate() {
                        if f != r {
                            edits.push((off + i, r));
                        }
                    }
                }
                Ok((offsets, edits))
            }
            Rule::Masked { pattern, edits } => {
                let offsets = pattern.find_all(data);
                let mut out = Vec::new();
                for &off in &offsets {
                    for e in edits {
                        if e.offset >= pattern.len() {
                            bail!("Edit 偏移 {} 超出模式长度 {}", e.offset, pattern.len());
                        }
                        out.push((off + e.offset, e.value));
                    }
                }
                Ok((offsets, out))
            }
        }
    }
}

/// 对命中数量的期望。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// 必须恰好命中一处（代码补丁用：命中多处说明模式不够特异）
    ExactlyOne,
    /// 至少命中一处，全部替换（字符串补丁用，对应 BetterWX 的全量 replace）
    AtLeastOne,
}

/// 一个补丁点。`variants` 中**任一**命中即算成功，用于同一功能在不同微信版本上的
/// 多种代码形态（例如 `global_config` 在 4.0.x 与 4.1.x 下的两种编译产物）。
#[derive(Debug, Clone)]
pub struct PatchSpec {
    pub name: &'static str,
    pub variants: Vec<(&'static str, Rule)>,
    pub expect: Expect,
    /// 全部形态都失配时，用这个更短的「前导」模式定位大致位置并 dump 上下文字节。
    /// 微信升级换了代码生成方式时，这段 dump 就是补新形态所需的全部信息。
    pub diag_lead: Option<Pattern>,
}

/// 单个补丁点的命中情况，用于 UI 日志。
#[derive(Debug, Clone)]
pub struct Hit {
    pub name: &'static str,
    pub variant: &'static str,
    pub offsets: Vec<usize>,
    /// 实际写入的绝对偏移
    pub sites: Vec<usize>,
}

/// 规划结果：只描述「哪个绝对偏移要写成什么字节」，不改动任何数据。
#[derive(Debug, Clone, Default)]
pub struct PatchPlan {
    pub edits: Vec<(usize, u8)>,
    pub hits: Vec<Hit>,
}

impl PatchPlan {
    /// 把规划好的改动写入缓冲区。
    pub fn apply(&self, data: &mut [u8]) -> Result<()> {
        let len = data.len();
        for &(off, val) in &self.edits {
            let slot = data
                .get_mut(off)
                .ok_or_else(|| anyhow!("补丁偏移 {off:#x} 超出文件范围（{len} 字节）"))?;
            *slot = val;
        }
        Ok(())
    }
}

/// 对 `data` 规划全部补丁。任何一个补丁点失配或命中数不符合期望，都返回错误且不写入任何东西。
pub fn plan(specs: &[PatchSpec], data: &[u8]) -> Result<PatchPlan> {
    let mut out = PatchPlan::default();

    for spec in specs {
        let mut chosen = None;
        for (variant_name, rule) in &spec.variants {
            let (offsets, edits) = rule
                .scan(data)
                .with_context(|| format!("补丁 `{}` 的形态 `{variant_name}` 扫描失败", spec.name))?;
            if !offsets.is_empty() {
                chosen = Some((*variant_name, offsets, edits));
                break;
            }
        }

        let Some((variant_name, offsets, edits)) = chosen else {
            bail!("{}", miss_report(spec, data));
        };

        if spec.expect == Expect::ExactlyOne && offsets.len() != 1 {
            bail!(
                "补丁 `{}`（形态 `{variant_name}`）命中 {} 处，预期恰好 1 处：{}\n\
                 模式不够特异，已中止，未写入任何字节。",
                spec.name,
                offsets.len(),
                offsets.iter().map(|o| format!("{o:#x}")).collect::<Vec<_>>().join(", ")
            );
        }

        out.hits.push(Hit {
            name: spec.name,
            variant: variant_name,
            offsets,
            sites: edits.iter().map(|&(o, _)| o).collect(),
        });
        out.edits.extend(edits);
    }

    Ok(out)
}

/// 补丁点完全失配时，生成尽可能有用的诊断信息。
fn miss_report(spec: &PatchSpec, data: &[u8]) -> String {
    let mut msg = format!(
        "补丁 `{}` 未找到匹配（已尝试 {} 种形态：{}）",
        spec.name,
        spec.variants.len(),
        spec.variants.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(" / ")
    );

    if let Some(lead) = &spec.diag_lead {
        let sites = lead.find_all(data);
        if sites.is_empty() {
            msg.push_str("\n前导特征也不存在，该文件可能根本不是微信主模块。");
        } else {
            msg.push_str(&format!(
                "\n但在 {} 处找到了前导特征，说明只是代码生成方式变了。上下文字节：",
                sites.len()
            ));
            for &s in sites.iter().take(3) {
                let end = (s + 64).min(data.len());
                let dump = data[s..end]
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                msg.push_str(&format!("\n  {s:#x}: {dump}"));
            }
            msg.push_str("\n请把以上字节反馈给开发者以补充新形态。");
        }
    }
    msg.push_str("\n微信版本可能不受支持，已中止，未写入任何字节。");
    msg
}

/// 把 ASCII 字符串转成 UTF-16LE，但**结尾不带 NUL**。
///
/// 精确复现 BetterWX 的 `"\x00".join(s)` —— 那是「在字符之间插 NUL」，
/// 所以最后一个字符后面没有 0 字节。这一点极易写错：直接 `encode_utf16`
/// 再转字节会多出一个尾部 0，导致模式匹配不到。
pub fn wide_no_nul(s: &str) -> Vec<u8> {
    debug_assert!(s.is_ascii(), "wide_no_nul 仅用于 ASCII 字符串: {s:?}");
    let mut out = Vec::with_capacity(s.len() * 2);
    for b in s.bytes() {
        out.push(b);
        out.push(0);
    }
    out.pop(); // 去掉最后一个字符后面的 NUL
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_no_nul_has_no_trailing_zero() {
        let w = wide_no_nul("Weixin.dll");
        assert_eq!(w.len(), 19, "10 个字符应产生 19 字节（字符间插 NUL，结尾不补）");
        assert_eq!(
            w,
            vec![
                0x57, 0x00, 0x65, 0x00, 0x69, 0x00, 0x78, 0x00, 0x69, 0x00, 0x6E, 0x00, 0x2E,
                0x00, 0x64, 0x00, 0x6C, 0x00, 0x6C,
            ]
        );
        assert_ne!(*w.last().unwrap(), 0, "结尾必须不是 NUL");
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(Pattern::parse("48 B").is_err(), "奇数长度应报错");
        assert!(Pattern::parse("48 ?A").is_err(), "半通配应报错");
        assert!(Pattern::parse("48 ZZ").is_err(), "非十六进制应报错");
        assert!(Pattern::parse("").is_err(), "空模式应报错");
        assert!(Pattern::parse("?? ??").is_err(), "全通配应报错");
    }

    #[test]
    fn anchor_picks_longest_literal_run() {
        // 前面 2 字节通配，中间 4 字节具体，后面 1 字节具体
        let p = Pattern::parse("?? ?? 11 22 33 44 ?? 55").unwrap();
        assert_eq!(p.anchor_off, 2);
        assert_eq!(p.anchor, vec![0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn find_all_handles_leading_wildcards() {
        // 模式以通配开头时，锚点在中间，start = hit - anchor_off 必须正确回退
        let p = Pattern::parse("?? ?? AA BB").unwrap();
        let hay = [0x00u8, 0x99, 0xAA, 0xBB, 0xAA, 0xBB];
        assert_eq!(p.find_all(&hay), vec![0, 2]);
    }

    #[test]
    fn find_all_skips_underflow_at_buffer_start() {
        // 锚点出现在缓冲区最开头，而模式要求它前面还有 2 字节 -> 不能 panic，应跳过
        let p = Pattern::parse("?? ?? AA BB").unwrap();
        let hay = [0xAAu8, 0xBB, 0x00, 0x00];
        assert_eq!(p.find_all(&hay), Vec::<usize>::new());
    }

    #[test]
    fn find_all_finds_overlapping_matches() {
        let p = Pattern::parse("AA AA").unwrap();
        let hay = [0xAAu8, 0xAA, 0xAA];
        assert_eq!(p.find_all(&hay), vec![0, 1], "重叠匹配必须都能找到");
    }

    #[test]
    fn wildcards_match_any_byte_including_nul_and_ff() {
        let p = Pattern::parse("11 ?? 22").unwrap();
        assert!(p.matches_at(&[0x11, 0x00, 0x22], 0));
        assert!(p.matches_at(&[0x11, 0xFF, 0x22], 0));
        assert!(!p.matches_at(&[0x11, 0xFF, 0x23], 0));
    }

    #[test]
    fn exactly_one_rejects_multiple_hits_without_writing() {
        let spec = PatchSpec {
            name: "test",
            variants: vec![(
                "only",
                Rule::Masked {
                    pattern: Pattern::parse("AA BB").unwrap(),
                    edits: vec![Edit { offset: 1, value: 0x99 }],
                },
            )],
            expect: Expect::ExactlyOne,
            diag_lead: None,
        };
        let data = [0xAAu8, 0xBB, 0x00, 0xAA, 0xBB];
        let err = plan(&[spec], &data).unwrap_err().to_string();
        assert!(err.contains("命中 2 处"), "错误信息应说明命中数: {err}");
    }

    #[test]
    fn missing_pattern_is_an_error_not_a_silent_skip() {
        let spec = PatchSpec {
            name: "test",
            variants: vec![(
                "only",
                Rule::Literal { find: b"nope".to_vec(), replace: b"NOPE".to_vec() },
            )],
            expect: Expect::AtLeastOne,
            diag_lead: None,
        };
        assert!(plan(&[spec], b"haystack").is_err());
    }
}
