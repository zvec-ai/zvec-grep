use regex::Regex;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;
use unicode_width::UnicodeWidthChar;

pub(super) fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static PDF assembly regex")
}
pub(super) fn clean(text: &str) -> String {
    text.nfkc().collect::<String>().trim().to_owned()
}
pub(super) fn norm(text: &str) -> String {
    clean(text)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}
pub(super) fn cjk(c: char) -> bool {
    c.width() == Some(2)
}
pub(super) fn adjacent_cjk(a: &str, b: &str) -> bool {
    a.chars().last().is_some_and(cjk) && b.chars().next().is_some_and(cjk)
}
pub(super) fn join(a: &mut String, b: &str) {
    if b.is_empty() {
        return;
    }
    if !a.is_empty() {
        if a.ends_with('-') && b.starts_with(char::is_lowercase) {
            a.pop();
        } else if !adjacent_cjk(a, b) {
            a.push(' ');
        }
    }
    a.push_str(b);
}
pub(super) fn closed(text: &str) -> bool {
    static PATTERN: LazyLock<Regex> =
        LazyLock::new(|| re(r#"[.!?…。！？:：;；][”’」』）)\]\"]*\s*$"#));
    PATTERN.is_match(text)
}
pub(super) fn bullet(text: &str) -> bool {
    static P: LazyLock<Regex> =
        LazyLock::new(|| re(r"^\s*(?:[•●○▪◦➢➤►◆◇・]|[Ø]\s|>>|»\s|[-–]\s|[①-⑳])"));
    P.is_match(text)
}
pub(super) fn enumeration(text: &str) -> bool {
    static P: LazyLock<Regex> =
        LazyLock::new(|| re(r"^\s*(?:\(\d{1,2}\)\s*\S|\d{1,2}[.)]\s|[a-z][.)]\s)"));
    P.is_match(&clean(text))
}
pub(super) fn list(text: &str) -> bool {
    bullet(text) || enumeration(text)
}
pub(super) fn caption(text: &str) -> bool {
    static P: LazyLock<Regex> = LazyLock::new(|| {
        re(r"(?i)^(?:(?:figure|fig\.|table|tab\.)\s*\d+[.:]|[图図表]\s*\d+[：:.\s])")
    });
    P.is_match(text)
}
pub(super) fn numbered(text: &str) -> Option<usize> {
    static ROOT: LazyLock<Regex> =
        LazyLock::new(|| re(r"^第[一二三四五六七八九十百零〇\d]+(?:部分|章|篇|部)"));
    static MID: LazyLock<Regex> = LazyLock::new(|| re(r"^[一二三四五六七八九十]+[、.]"));
    static SMALL: LazyLock<Regex> = LazyLock::new(|| re(r"^\([一二三四五六七八九十]+\)"));
    static PAREN: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{1,2}\)\s*\S"));
    static NUM: LazyLock<Regex> =
        LazyLock::new(|| re(r"^(\d+(?:\.\d+)*)(?:\.\s+|\s+|[A-Za-z\u{3040}-\u{9fff}])"));
    let s = clean(text);
    if ROOT.is_match(&s) {
        Some(1)
    } else if MID.is_match(&s) || PAREN.is_match(&s) {
        Some(2)
    } else if SMALL.is_match(&s) {
        Some(3)
    } else {
        NUM.captures(&s).and_then(|c| {
            let n = &c[1];
            (n.split('.').next()?.len() <= 2).then(|| 1 + n.matches('.').count())
        })
    }
}
pub(super) fn flow(a: &str, b: &str) -> bool {
    let b = b.trim_start();
    !closed(a)
        && b.chars().next().is_some_and(|c| {
            c.is_lowercase()
                || cjk(c) && a.chars().last().is_some_and(cjk)
                || a.ends_with(['-', '‐']) && c.is_alphabetic()
        })
}

pub(super) fn number_only(s: &str) -> bool {
    static P: LazyLock<Regex> = LazyLock::new(|| re(r"^\d+(?:\.\d+)*\.?$"));
    P.is_match(s)
}
pub(super) fn short_number(s: &str) -> bool {
    static P: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{1,2}$"));
    P.is_match(s)
}
pub(super) fn formula_number(s: &str) -> bool {
    static P: LazyLock<Regex> = LazyLock::new(|| re(r"^\(\d{1,3}\)$"));
    P.is_match(s)
}
