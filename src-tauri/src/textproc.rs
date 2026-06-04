//! 文本处理:HTML 抽纯文本、关键词提取(jieba)、抽取式摘要(bge 向量)。
//!
//! 这些都不改动磁盘上的原文件 —— 只为「搜索索引 / 摘要 / 关键词」服务。
//! HTML 文件本身保持原样(交互原型),这里只是把它的可读文字抽出来给索引用。

use std::collections::HashMap;
use std::sync::OnceLock;

use jieba_rs::Jieba;

use crate::embed::Embedder;

/// 摘要字符上限。
pub const SUMMARY_LEN: usize = 200;

// ============ HTML → 纯文本 ============

/// 定位并跳过 `<tag ...>...</tag>` 整块(用于 script / style)。
/// `lower` 是 html 的 ASCII 小写副本(字节位置与原串一致)。
fn skip_block(lower: &str, at: usize, tag: &str) -> Option<usize> {
    if !lower[at..].starts_with(&format!("<{tag}")) {
        return None;
    }
    let rel = lower[at..].find(&format!("</{tag}"))?;
    let from = at + rel;
    let gt = lower[from..].find('>')?;
    Some(from + gt + 1)
}

/// 把 HTML 抽成纯文本:去掉 script / style 整块与所有标签,解码常见实体,折叠空白。
pub fn html_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase(); // ASCII 小写不改字节长度,位置可共用
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(end) =
                skip_block(&lower, i, "script").or_else(|| skip_block(&lower, i, "style"))
            {
                out.push(' ');
                i = end;
                continue;
            }
            match lower[i..].find('>') {
                Some(gt) => {
                    out.push(' ');
                    i += gt + 1;
                }
                None => break, // 标签未闭合,丢弃剩余
            }
            continue;
        }
        // 复制到下一个 '<' 之前的文本
        let next = lower[i..].find('<').map(|p| i + p).unwrap_or(bytes.len());
        out.push_str(&html[i..next]);
        i = next;
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 取文档「可索引正文」:HTML 抽纯文本,其它(md / 转换结果等)原样返回。
pub fn clean_body(raw: &str, ext: &str) -> String {
    if ext == "html" || ext == "htm" {
        html_to_text(raw)
    } else {
        raw.to_string()
    }
}

// ============ 关键词提取(jieba 分词 + 词频)============

static JIEBA: OnceLock<Jieba> = OnceLock::new();
fn jieba() -> &'static Jieba {
    JIEBA.get_or_init(Jieba::new)
}

/// 常见中文虚词 / 停用词 —— 词频法的主要噪音来源。
const STOPWORDS: &[&str] = &[
    "的", "了", "是", "在", "和", "与", "及", "或", "也", "就", "都", "而", "对", "把", "被",
    "为", "以", "等", "这", "那", "有", "我", "你", "他", "她", "它", "们", "个", "并", "可",
    "会", "要", "到", "上", "下", "中", "内", "由", "其", "之", "该", "各", "但", "则", "若",
    "如", "按", "做", "用", "时", "后", "前", "里", "从", "向", "给", "让", "使", "能", "不",
    "没", "很", "再", "又", "还", "一个", "可以", "进行", "以及", "通过", "由于", "因为",
    "所以", "如果", "需要", "这个", "那个", "我们", "他们", "什么", "这些", "那些",
];

/// 从正文按 jieba 分词后取高频实词作关键词,最多 `top_k` 个。
pub fn keywords(text: &str, top_k: usize) -> Vec<String> {
    let mut freq: HashMap<&str, usize> = HashMap::new();
    for tok in jieba().cut(text, true) {
        let t = tok.trim();
        let n = t.chars().count();
        if !(2..=10).contains(&n) {
            continue;
        }
        if STOPWORDS.contains(&t) {
            continue;
        }
        // 纯数字 / 纯 ASCII 标点跳过
        if t.chars().all(|c| c.is_ascii_digit() || c.is_ascii_punctuation()) {
            continue;
        }
        *freq.entry(t).or_insert(0) += 1;
    }
    let mut v: Vec<(&str, usize)> = freq.into_iter().collect();
    // 频次降序;同频按词排序保证稳定
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    v.into_iter().take(top_k).map(|(k, _)| k.to_string()).collect()
}

// ============ 抽取式摘要(bge 句向量)============

/// 把文本切成句子(中文标点 / 换行为界,过短的丢弃)。
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut flush = |cur: &mut String| {
        let t = cur.trim();
        if t.chars().count() >= 6 {
            out.push(t.to_string());
        }
        cur.clear();
    };
    for c in text.chars() {
        cur.push(c);
        if matches!(c, '。' | '\u{FF01}' | '\u{FF1F}' | '!' | '?' | '\u{FF1B}' | ';' | '\n') {
            flush(&mut cur);
        }
    }
    flush(&mut cur);
    out
}

/// 折叠空白并截断到 `SUMMARY_LEN`。
fn truncate(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(SUMMARY_LEN)
        .collect()
}

/// 抽取式摘要:把正文切句、用 bge 编码,挑出最贴近全文中心的几句拼成摘要。
/// 句子太少或 embed 不可用时,退化为「正文前若干字」。
/// 去掉目录项 / 引导点 / 纯页码等噪音行(PDF/长文档常见),避免它们污染摘要。
/// 只过滤很明确的目录/页码特征,不动正常正文。
fn strip_toc_noise(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let t = line.trim();
            if t.is_empty() {
                return false;
            }
            // 目录项:行尾是 "…… 页码" 或 "....(多个点) 页码"
            if t.ends_with(|c: char| c.is_ascii_digit())
                && (t.contains("……") || t.matches('.').count() >= 4)
            {
                return false;
            }
            // 纯页码行:"- 1 -" / "1" / "—12—"
            let core: String = t
                .chars()
                .filter(|c| !matches!(c, '-' | '–' | '—' | ' ' | '\t'))
                .collect();
            if !core.is_empty() && core.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
            true
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn summarize(embedder: &Embedder, text: &str) -> String {
    let cleaned = strip_toc_noise(text);
    let text = cleaned.trim();
    if text.is_empty() {
        return String::new();
    }
    let sentences = split_sentences(text);
    if sentences.len() <= 3 {
        return truncate(text);
    }
    // 控制成本:最多取前 30 句参与
    let cap: Vec<String> = sentences.into_iter().take(30).collect();
    let vecs = match embedder.embed(&cap, "") {
        Ok(v) if v.len() == cap.len() && !v.is_empty() => v,
        _ => return truncate(text),
    };
    // 质心 = 句向量均值;每句打分 = 与质心的点积。
    let dim = vecs[0].len();
    let mut centroid = vec![0f32; dim];
    for v in &vecs {
        for (i, x) in v.iter().enumerate() {
            centroid[i] += x;
        }
    }
    let mut scored: Vec<(usize, f32)> = vecs
        .iter()
        .enumerate()
        .map(|(i, v)| (i, v.iter().zip(&centroid).map(|(a, b)| a * b).sum()))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut pick: Vec<usize> = scored.into_iter().take(3).map(|(i, _)| i).collect();
    pick.sort_unstable(); // 按原文顺序拼接
    let joined = pick
        .iter()
        .map(|&i| cap[i].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    truncate(&joined)
}
