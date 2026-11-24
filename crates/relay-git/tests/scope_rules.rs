//! WP2-A1 写入范围契约测试：校验、匹配、冲突与两层交集。

use relay_git::{validate_entry, ScopeError, WriteScope, MAX_SCOPE_RULES, MAX_SCOPE_RULE_BYTES};

#[test]
fn file_and_directory_rules_match_by_components() {
    // 文件精确匹配（单独一层，避免目录规则覆盖到近似路径）。
    let file_only = WriteScope::new(&["src/main.rs"]).unwrap();
    assert!(file_only.covers("src/main.rs").unwrap());
    assert!(!file_only.covers("src/main.rs.bak").unwrap());
    assert!(!file_only.covers("src/lib.rs").unwrap());

    let scope = WriteScope::new(&["src/", "tests/integration/"]).unwrap();
    // 目录规则按组件前缀：覆盖其下任意深度，不匹配 src2/。
    assert!(scope.covers("src/lib.rs").unwrap());
    assert!(scope.covers("src/a/b/c.rs").unwrap());
    assert!(!scope.covers("src2/x.rs").unwrap(), "src/ 不得匹配 src2/");
    assert!(!scope.covers("src-api/x.rs").unwrap());
    // 目录规则不匹配根下同名文件。
    assert!(!scope.covers("src").unwrap());
    // 嵌套目录规则按组件对齐。
    assert!(scope.covers("tests/integration/a.rs").unwrap());
    assert!(!scope.covers("tests/integration_helper.rs").unwrap());
    assert!(!scope.covers("tests/x.rs").unwrap());
}

#[test]
fn invalid_entries_are_rejected() {
    for (entry, expected) in [
        ("/etc/passwd", "AbsolutePath"),
        ("src//x.rs", "EmptyComponent"),
        ("src/./x.rs", "InvalidComponent"),
        ("src/../etc", "InvalidComponent"),
        ("src/.git/config", "InvalidComponent"),
        ("a/.Git/config", "InvalidComponent"),
        ("a/.gitignore.d/x", "ok"),
        ("a\u{0001}b", "ControlCharacter"),
        ("a\u{007f}b", "ControlCharacter"),
        ("a\u{009f}b", "ControlCharacter"),
        ("src\\windows.rs", "ControlCharacter"),
        ("src/\u{FEFF}x.rs", "ControlCharacter"),
        ("cafe\u{301}/x.rs", "NotNfc"),
        ("\u{212A}ernel/x.rs", "NotNfc"),
        // 回归：纯 LV 音节 가 + 终声 T jamo 必组合成 각，jamo 序列不是
        // NFC；而 LVT 音节 한 + 另一个 T 已无法组合，是合法 NFC。
        ("가\u{11A8}/x.rs", "NotNfc"),
        ("한\u{11A8}/x.rs", "ok"),
        // 回归：CJK 兼容象形文字 U+F900 有规范分解（→ U+8C48），不是 NFC。
        ("\u{F900}/x.rs", "NotNfc"),
        // x + U+0301 无预组合形式，本身就是合法 NFC（正例）。
        ("x\u{301}/x.rs", "ok"),
        ("src", "ok"),
        ("src/", "ok"),
        ("", "EmptyEntry"),
    ] {
        let result = validate_entry(entry);
        match expected {
            "ok" => assert!(result.is_ok(), "{entry} 应通过: {result:?}"),
            kind => {
                assert!(result.is_err(), "{entry} 应被拒绝");
                assert!(
                    format!("{:?}", result.unwrap_err()).starts_with(kind),
                    "entry {entry} 应为 {kind}"
                );
            }
        }
    }
    // 合法 NFC 正例：预组合拉丁/希腊重音、韩文音节、CJK 豈（U+F900 的
    // 规范分解目标 U+8C48）。
    assert!(validate_entry("caf\u{e9}/x.rs").is_ok());
    assert!(validate_entry("\u{d55c}글/x.rs").is_ok());
    assert!(validate_entry("\u{8C48}/x.rs").is_ok());
    assert!(validate_entry("σ/径.rs").is_ok());
    assert!(validate_entry("ς/径.rs").is_ok());
}

#[test]
fn byte_and_count_limits_match_spec() {
    // 04：单条 ≤ 240 B（编码后计）、最多 64 条。
    let ok240 = "\u{e9}".repeat(120); // 120 × 2 B = 240 B
    assert_eq!(ok240.len(), MAX_SCOPE_RULE_BYTES);
    assert!(validate_entry(&ok240).is_ok());
    let too_long = "\u{e9}".repeat(121);
    assert_eq!(too_long.len(), MAX_SCOPE_RULE_BYTES + 2);
    let err = validate_entry(&too_long).unwrap_err();
    assert!(
        matches!(err, ScopeError::EntryTooLong { len: 242, .. }),
        "{err:?}"
    );

    let entries: Vec<String> = (0..MAX_SCOPE_RULES).map(|i| format!("dir{i}/")).collect();
    let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
    assert!(WriteScope::new(&refs).is_ok());
    let mut over = entries;
    over.push("extra/".to_owned());
    let refs: Vec<&str> = over.iter().map(String::as_str).collect();
    let err = WriteScope::new(&refs).unwrap_err();
    assert!(
        matches!(err, ScopeError::TooManyRules { count: 65 }),
        "{err:?}"
    );
}

#[test]
fn greek_sigma_variants_are_case_conflict() {
    // 回归：ς（final sigma）按 case folding 折叠到 σ，两条规则冲突；
    // to_lowercase 不会把 ς 变成 σ，须用完整 case folding。
    let err = WriteScope::new(&["σ/", "ς/"]).unwrap_err();
    assert!(matches!(err, ScopeError::CaseConflict { .. }), "{err:?}");
    // 单独使用各自合法（正例保留）。
    assert!(validate_entry("σ/径.rs").is_ok());
    assert!(validate_entry("ς/径.rs").is_ok());
}

#[test]
fn set_conflicts_are_rejected() {
    // 字节重复。
    let err = WriteScope::new(&["src/", "src/"]).unwrap_err();
    assert!(matches!(err, ScopeError::DuplicateRule { .. }), "{err:?}");
    // 大小写折叠冲突（ASCII）。
    let err = WriteScope::new(&["src/", "SRC/"]).unwrap_err();
    assert!(matches!(err, ScopeError::CaseConflict { .. }), "{err:?}");
    // 大小写折叠冲突（非 ASCII，std 全量小写表）。
    let err = WriteScope::new(&["caf\u{e9}/", "caf\u{c9}/"]).unwrap_err();
    assert!(matches!(err, ScopeError::CaseConflict { .. }), "{err:?}");
    // .git 组件大小写变体在单条校验即拒绝（见 invalid_entries），
    // 不同字节串的规范化冲突因保守 NFC 策略无法同时入集。
}

#[test]
fn covers_validates_the_path_too() {
    let scope = WriteScope::new(&["src/"]).unwrap();
    assert!(matches!(
        scope.covers("/etc/passwd"),
        Err(ScopeError::AbsolutePath { .. })
    ));
    assert!(matches!(
        scope.covers("../x.rs"),
        Err(ScopeError::InvalidComponent { .. })
    ));
    assert!(matches!(
        scope.covers("src/.git/config"),
        Err(ScopeError::InvalidComponent { .. })
    ));
    assert!(matches!(
        scope.covers("src/"),
        Err(ScopeError::DirectoryPath { .. })
    ));
    // x + U+0301 无预组合形式，是合法 NFC：只受规则覆盖判断影响。
    assert_eq!(scope.covers("x\u{301}.rs"), Ok(false));
}

#[test]
fn two_layer_intersection_is_conservative_subset() {
    // 同层规则 → 交集保持。
    let project = WriteScope::new(&["src/", "tests/"]).unwrap();
    let task = WriteScope::new(&["src/"]).unwrap();
    let both = WriteScope::intersection(&project, &task);
    assert!(both.covers("src/a.rs").unwrap());
    assert!(
        !both.covers("tests/a.rs").unwrap(),
        "tests/ 只在一层，交集不应覆盖"
    );

    // 目录规则 ∩ 目录内文件规则 → 只保留文件规则（失败关闭）。
    let layer_a = WriteScope::new(&["src/"]).unwrap();
    let layer_b = WriteScope::new(&["src/main.rs"]).unwrap();
    let both = WriteScope::intersection(&layer_a, &layer_b);
    assert!(both.covers("src/main.rs").unwrap());
    assert!(
        !both.covers("src/other.rs").unwrap(),
        "精确集合交集不含该路径"
    );

    // 完全不相交 → 交集为空。
    let layer_c = WriteScope::new(&["docs/"]).unwrap();
    let both = WriteScope::intersection(&layer_a, &layer_c);
    assert!(!both.covers("src/a.rs").unwrap());
    assert!(!both.covers("docs/a.md").unwrap());
}
