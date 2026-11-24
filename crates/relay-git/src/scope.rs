//! 写入范围（write_scope）：纯路径校验、匹配与两层授权交集。
//!
//! 依据 06「工作副本生命周期」§3 与 04 字段约束：仅接受仓库相对路径，
//! 文件精确匹配，目录前缀必须以 `/` 结尾并按组件比较，无 glob；条目须
//! 为 UTF-8 NFC（`unicode-normalization` 全量校验），禁止空组件、
//! `.`/`..`、绝对路径、控制字符和 `.git` 路径组件；两层授权用集合包含
//! 关系求交。
//!
//! 集合内冲突检测：字节重复拒绝；大小写折叠冲突用 `unicase`（完整
//! Unicode case folding，ς 折叠到 σ）检测；两条规则规范化等价时
//! `to_nfc` 归一后字节相同，同样按冲突拒绝。NFC 全量校验保证通过校验
//! 的路径不存在被规范化放宽授权的问题。匹配始终按原始路径字节进行，
//! 不使用折叠或规范化结果扩大授权。

//! NFC 说明：归一化管线由本实现按 UAX#15 完成，基于
//! `unicode-normalization` 的单点原语（`decompose_canonical` 递归分解、
//! `canonical_combining_class` 排序键、`char::compose` 组合，Hangul 按标准
//! 算法组合）；不使用该 crate 的流式 `nfc()`/`nfd()` 迭代器。
//! 本包只做路径规则：不解析 glob（元字符按字面量精确匹配），不涉及
//! 文件系统链接检查，也不声称已解决。
//!
//! 交集语义为保守子集：保留「对方层包含本方规则」的规则并集。目录规则
//! 只能被对方的目录规则包含；文件规则可被文件精确规则或目录前缀包含。
//! 由此得到的路径覆盖 ⊆ 精确集合交集，不会放宽授权。

use std::fmt;

use unicase::UniCase;
use unicode_normalization::char::{canonical_combining_class, compose, decompose_canonical};

/// 单层授权最多条目数（04：write_scope 最多 64 条）。
pub const MAX_SCOPE_RULES: usize = 64;

/// 单条规则最大字节数（04：单条 ≤ 240 B，编码后计）。
pub const MAX_SCOPE_RULE_BYTES: usize = 240;

/// 规则种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeRuleKind {
    /// 文件精确匹配（raw 不以 `/` 结尾）。
    File,
    /// 目录前缀匹配（raw 以 `/` 结尾，按路径组件比较）。
    Directory,
}

/// 校验通过的写入范围规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeRule {
    raw: String,
    kind: ScopeRuleKind,
}

impl ScopeRule {
    /// 原始条目文本。
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// 规则种类。
    pub fn kind(&self) -> ScopeRuleKind {
        self.kind
    }

    /// 覆盖判断。`path` 必须已通过 [`validate_entry`]（`WriteScope::covers`
    /// 会先校验）：文件精确匹配；目录规则按组件前缀匹配——`src/` 匹配
    /// `src/main.rs` 与 `src/a/b.rs`，不匹配 `src2/x.rs`，也不匹配根下
    /// 名为 `src` 的文件。
    pub fn covers_path(&self, path: &str) -> bool {
        match self.kind {
            ScopeRuleKind::File => self.raw == path,
            ScopeRuleKind::Directory => path.starts_with(&self.raw),
        }
    }
}

/// 写入范围校验与匹配错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeError {
    /// 条目为空。
    EmptyEntry,
    /// 条目超过 [`MAX_SCOPE_RULE_BYTES`] 字节。
    EntryTooLong { entry: String, len: usize },
    /// 条目数超过 [`MAX_SCOPE_RULES`]。
    TooManyRules { count: usize },
    /// 绝对路径（以 `/` 开头）。
    AbsolutePath { path: String },
    /// 路径组件为空（如 `src//x`）。
    EmptyComponent { path: String },
    /// 非法组件：`.`、`..` 或 `.git`（大小写变体一并拒绝）。
    InvalidComponent { path: String, component: String },
    /// 含控制字符、不可见格式字符或反斜杠。
    ControlCharacter { path: String },
    /// 条目不是 Unicode NFC 形式（完整 NFC 校验，见模块文档）。
    NotNfc { path: String },
    /// 集合内重复规则（字节级相同）。
    DuplicateRule { first: String, second: String },
    /// 集合内大小写折叠冲突（不同字节串折叠后相同）。
    CaseConflict { first: String, second: String },
    /// `covers` 传入的路径以 `/` 结尾：写入目标是文件。
    DirectoryPath { path: String },
}

impl fmt::Display for ScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeError::EmptyEntry => write!(f, "写入范围条目为空"),
            ScopeError::EntryTooLong { entry, len } => write!(
                f,
                "写入范围条目 {entry} 长度 {len} 字节，超过 {MAX_SCOPE_RULE_BYTES} 字节上限"
            ),
            ScopeError::TooManyRules { count } => {
                write!(f, "写入范围共 {count} 条，超过 {MAX_SCOPE_RULES} 条上限")
            }
            ScopeError::AbsolutePath { path } => {
                write!(f, "写入范围条目 {path} 是绝对路径")
            }
            ScopeError::EmptyComponent { path } => {
                write!(f, "写入范围条目 {path} 含空路径组件")
            }
            ScopeError::InvalidComponent { path, component } => write!(
                f,
                "写入范围条目 {path} 含非法组件 {component}（`.`、`..` 或 `.git` 及其大小写变体）"
            ),
            ScopeError::ControlCharacter { path } => {
                write!(f, "写入范围条目 {path} 含控制字符、不可见格式字符或反斜杠")
            }
            ScopeError::NotNfc { path } => write!(f, "写入范围条目 {path} 不是 Unicode NFC 形式"),
            ScopeError::DuplicateRule { first, second } => {
                write!(f, "写入范围重复规则：{first} 与 {second}")
            }
            ScopeError::CaseConflict { first, second } => {
                write!(f, "写入范围大小写冲突：{first} 与 {second} 折叠后相同")
            }
            ScopeError::DirectoryPath { path } => {
                write!(f, "校验路径 {path} 以 / 结尾：写入目标是文件")
            }
        }
    }
}

impl std::error::Error for ScopeError {}

/// 校验单条写入范围条目（规则与被校验路径共用同一套规则）。
pub fn validate_entry(entry: &str) -> Result<ScopeRule, ScopeError> {
    if entry.is_empty() {
        return Err(ScopeError::EmptyEntry);
    }
    if entry.len() > MAX_SCOPE_RULE_BYTES {
        return Err(ScopeError::EntryTooLong {
            entry: entry.to_owned(),
            len: entry.len(),
        });
    }
    if entry.chars().any(is_forbidden_char) {
        return Err(ScopeError::ControlCharacter {
            path: entry.to_owned(),
        });
    }
    ensure_nfc(entry)?;
    if entry.starts_with('/') {
        return Err(ScopeError::AbsolutePath {
            path: entry.to_owned(),
        });
    }
    let kind = if entry.ends_with('/') {
        ScopeRuleKind::Directory
    } else {
        ScopeRuleKind::File
    };
    // 目录规则的末尾 `/` 是种类标记；其余空组件一律非法。
    let body = entry.strip_suffix('/').unwrap_or(entry);
    for component in body.split('/') {
        if component.is_empty() {
            return Err(ScopeError::EmptyComponent {
                path: entry.to_owned(),
            });
        }
        if component == "." || component == ".." || component.to_lowercase() == ".git" {
            return Err(ScopeError::InvalidComponent {
                path: entry.to_owned(),
                component: component.to_owned(),
            });
        }
    }
    Ok(ScopeRule {
        raw: entry.to_owned(),
        kind,
    })
}

/// 控制字符（Cc）、不可见格式字符与反斜杠：Git 恒以 `/` 分隔路径，
/// 反斜杠只会造成匹配歧义。
fn is_forbidden_char(c: char) -> bool {
    let cp = c as u32;
    (0x00..=0x1F).contains(&cp)
        || (0x7F..=0x9F).contains(&cp)
        || (0x200B..=0x200F).contains(&cp)
        || (0x202A..=0x202E).contains(&cp)
        || (0x2060..=0x2064).contains(&cp)
        || cp == 0xFEFF
        || cp == 0x00AD
        || c == '\\'
}

/// 完整 NFD（规范化分解 + canonical ordering）：逐字符递归分解
/// （`decompose_canonical` 含递归与单字符内排序），再对 starter 之后的
/// 非 starter 连段按组合类稳定排序（等组合类保持原相对顺序）。
fn nfd_chars(s: &str) -> Vec<char> {
    let mut chars: Vec<char> = Vec::new();
    for c in s.chars() {
        decompose_canonical(c, |d| chars.push(d));
    }
    let mut result: Vec<char> = Vec::with_capacity(chars.len());
    let mut run: Vec<(u8, char)> = Vec::new();
    for c in chars {
        let cc = canonical_combining_class(c);
        if cc == 0 {
            result.extend(run.drain(..).map(|(_, d)| d));
            result.push(c);
        } else {
            run.push((cc, c));
        }
    }
    run.sort_by_key(|(class, _)| *class);
    result.extend(run.drain(..).map(|(_, d)| d));
    result
}

/// 完整 Unicode NFC 归一化：对 [`nfd_chars`] 的输出按 UAX#15 贪心规范
/// 组合。阻塞规则：存在位于 starter 与当前字符之间、组合类 ≥ 当前字符
/// 的字符（NFD 段内组合类非降，最近非 starter 的组合类即最大值）；
/// Hangul L+V 与 LV+T 按标准算法组合，不受阻塞规则影响。
fn to_nfc(s: &str) -> String {
    let ordered = nfd_chars(s);
    let mut out: Vec<char> = Vec::with_capacity(ordered.len());
    let mut starter: Option<usize> = None;
    let mut last_cc: u8 = 0;
    for c in ordered {
        let cc = canonical_combining_class(c);
        let mut composed = false;
        if let Some(sp) = starter {
            if let Some(composed_char) = compose_hangul(out[sp], c) {
                out[sp] = composed_char;
                composed = true;
            }
        }
        if !composed {
            if let Some(sp) = starter {
                let blocked = cc != 0 && last_cc >= cc;
                if !blocked {
                    if let Some(composed_char) = compose(out[sp], c) {
                        out[sp] = composed_char;
                        composed = true;
                    }
                }
            }
        }
        // 组合成功时保留被移除字符的组合类作为阻塞基准（NFD 段内
        // 组合类非降，等组合类的后续字符仍被阻塞）；未组合时按追加
        // 字符更新，starter 归零。
        last_cc = cc;
        if !composed {
            if cc == 0 {
                starter = Some(out.len());
            }
            out.push(c);
        }
    }
    out.into_iter().collect()
}

/// Hangul 算法组合（UAX#15）：L+V 组成 LV 音节，LV+T 组成 LVT 音节。
fn compose_hangul(prev: char, c: char) -> Option<char> {
    const L_BASE: u32 = 0x1100;
    const V_BASE: u32 = 0x1161;
    const S_BASE: u32 = 0xAC00;
    const L_COUNT: u32 = 19;
    const V_COUNT: u32 = 21;
    const T_COUNT: u32 = 28;
    let cp = prev as u32;
    let cc = c as u32;
    // L（U+1100..=U+1112）+ V（U+1161..=U+1175）→ LV。
    if (L_BASE..L_BASE + L_COUNT).contains(&cp) && (V_BASE..V_BASE + V_COUNT).contains(&cc) {
        let index = (cp - L_BASE) * V_COUNT + (cc - V_BASE);
        return char::from_u32(S_BASE + index * T_COUNT);
    }
    // LV（%28 == 0）+ T（U+11A8..=U+11C2）→ LVT。
    if (S_BASE..=0xD7A3).contains(&cp)
        && (cp - S_BASE).is_multiple_of(T_COUNT)
        && (0x11A8..=0x11C2).contains(&cc)
    {
        return char::from_u32(cp + (cc - 0x11A7));
    }
    None
}

/// 完整 Unicode NFC 校验：条目与其 NFC 规范化结果不一致即拒绝
///（含组合标记序列、Hangul Jamo 组合、CJK 兼容象形文字 U+F900 一类
/// 规范分解条目）。
fn ensure_nfc(entry: &str) -> Result<(), ScopeError> {
    if to_nfc(entry) != entry {
        return Err(ScopeError::NotNfc {
            path: entry.to_owned(),
        });
    }
    Ok(())
}

/// 单层写入范围：校验通过的规则集合，集合内无重复、无大小写折叠冲突。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteScope {
    rules: Vec<ScopeRule>,
}

impl WriteScope {
    /// 校验并构建一层授权。条目数与单条字节数按 04 上限检查；集合内
    /// 字节重复或大小写折叠冲突即拒绝。
    pub fn new(entries: &[&str]) -> Result<WriteScope, ScopeError> {
        if entries.len() > MAX_SCOPE_RULES {
            return Err(ScopeError::TooManyRules {
                count: entries.len(),
            });
        }
        let mut rules = Vec::with_capacity(entries.len());
        for entry in entries {
            rules.push(validate_entry(entry)?);
        }
        for i in 0..rules.len() {
            for other in &rules[i + 1..] {
                if rules[i].raw == other.raw {
                    return Err(ScopeError::DuplicateRule {
                        first: rules[i].raw.clone(),
                        second: other.raw.clone(),
                    });
                }
                if UniCase::new(rules[i].raw.as_str()) == UniCase::new(other.raw.as_str()) {
                    return Err(ScopeError::CaseConflict {
                        first: rules[i].raw.clone(),
                        second: other.raw.clone(),
                    });
                }
            }
        }
        Ok(WriteScope { rules })
    }

    /// 已校验的规则列表。
    pub fn rules(&self) -> &[ScopeRule] {
        &self.rules
    }

    /// 路径是否被本层授权覆盖。路径本身须为合法的文件相对路径
    /// （不得以 `/` 结尾）。
    pub fn covers(&self, path: &str) -> Result<bool, ScopeError> {
        if path.ends_with('/') {
            return Err(ScopeError::DirectoryPath {
                path: path.to_owned(),
            });
        }
        validate_entry(path)?;
        Ok(self.rules.iter().any(|r| r.covers_path(path)))
    }

    /// 本层是否包含规则 `r`（集合包含关系，06：两层授权用集合包含关系
    /// 求交）：文件规则要求本层存在覆盖该路径的规则；目录规则要求本层
    /// 存在以其为前缀的目录规则（文件规则覆盖不了整个目录）。
    fn contains_rule(&self, r: &ScopeRule) -> bool {
        match r.kind {
            ScopeRuleKind::File => self.rules.iter().any(|b| b.covers_path(&r.raw)),
            ScopeRuleKind::Directory => self
                .rules
                .iter()
                .any(|b| b.kind == ScopeRuleKind::Directory && r.raw.starts_with(&b.raw)),
        }
    }

    /// 两层授权交集（保守子集）：保留 A 中被 B 包含、以及 B 中被 A 包含
    /// 的规则。得到的覆盖 ⊆ 精确集合交集，不会放宽授权；目录与文件规则
    /// 交错的组合会下取近似（失败关闭）。
    pub fn intersection(a: &WriteScope, b: &WriteScope) -> WriteScope {
        let mut rules = Vec::new();
        for r in a.rules() {
            if b.contains_rule(r) {
                rules.push(r.clone());
            }
        }
        for r in b.rules() {
            if a.contains_rule(r) && !rules.contains(r) {
                rules.push(r.clone());
            }
        }
        WriteScope { rules }
    }
}
