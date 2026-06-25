//! 检索 query 的结构化 operator 解析。
//!
//! 把用户输入拆成「自由文本」+「字段约束」两部分:
//!   - 自由文本交给 BM25 + 语义向量打分(search::hybrid);
//!   - 字段约束交给 SQLite 过滤(store::doc_ids_matching),不进 tantivy、不重建索引。
//!
//! 支持的算子:`tag:` `path:` `type:` `kb:` `title:`;`-field:value` 取反;
//! `field:"带空格的值"` 引号包裹;裸 `"短语"` 并入自由文本。
//! 标签按嵌套前缀匹配:`tag:a` 命中 `a` 与 `a/b`。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Tag,
    Path,
    Type,
    Kb,
    Title,
    /// frontmatter 属性:值形如 `key=val`(无 `=` 则判断 key 是否存在)。
    Prop,
}

/// 一条字段约束。`negate` 为 true 表示「不满足才通过」(对应 `-field:value`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
    pub field: Field,
    pub value: String,
    pub negate: bool,
}

impl Constraint {
    /// 单条约束对一篇文档是否成立(未计 negate)。比较一律忽略 ASCII 大小写。
    /// `props` 为 frontmatter 属性的 JSON 对象串(空串 = 无属性)。
    fn hit(&self, kb_id: &str, rel_path: &str, ext: &str, tags: &str, title: &str, props: &str) -> bool {
        let v = self.value.to_lowercase();
        match self.field {
            Field::Kb => kb_id.to_lowercase() == v,
            // path / title:子串包含,便于按目录或标题片段筛
            Field::Path => rel_path.to_lowercase().contains(v.as_str()),
            Field::Title => title.to_lowercase().contains(v.as_str()),
            // type:扩展名精确(允许写成 .md)
            Field::Type => ext.to_lowercase().as_str() == v.trim_start_matches('.'),
            // tag:逗号分隔,整段相等或按 "/" 前缀命中嵌套子标签
            Field::Tag => tags.split(',').any(|t| {
                let t = t.trim().to_lowercase();
                !t.is_empty() && (t == v || t.starts_with(&format!("{v}/")))
            }),
            // prop:key=val(无 = 则只判断 key 是否存在);值做子串匹配,容忍列表串
            Field::Prop => prop_hit(props, &self.value),
        }
    }
}

/// frontmatter 属性匹配:`spec` 形如 `key=val` 或仅 `key`。
fn prop_hit(props: &str, spec: &str) -> bool {
    if props.is_empty() {
        return false;
    }
    let (key, want) = match spec.split_once('=') {
        Some((k, v)) => (k.trim().to_lowercase(), Some(v.trim().to_lowercase())),
        None => (spec.trim().to_lowercase(), None),
    };
    let map = match serde_json::from_str::<serde_json::Value>(props) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return false,
    };
    let found = map.iter().find(|(k, _)| k.to_lowercase() == key);
    match (found, want) {
        (Some((_, val)), Some(w)) => {
            let vs = match val {
                serde_json::Value::String(s) => s.to_lowercase(),
                other => other.to_string().to_lowercase(),
            };
            vs == w || vs.contains(w.as_str())
        }
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// 一篇文档是否通过全部约束(多条之间 AND;negate 取反)。
#[allow(clippy::too_many_arguments)]
pub fn doc_allowed(
    constraints: &[Constraint],
    kb_id: &str,
    rel_path: &str,
    ext: &str,
    tags: &str,
    title: &str,
    props: &str,
) -> bool {
    constraints
        .iter()
        .all(|c| c.hit(kb_id, rel_path, ext, tags, title, props) != c.negate)
}

fn field_of(key: &str) -> Option<Field> {
    match key {
        "tag" | "tags" => Some(Field::Tag),
        "path" => Some(Field::Path),
        "type" | "ext" => Some(Field::Type),
        "kb" => Some(Field::Kb),
        "title" => Some(Field::Title),
        "prop" | "property" => Some(Field::Prop),
        _ => None,
    }
}

/// 把原始 query 拆成 (自由文本, 约束列表)。
/// 无法识别为算子的 token 一律归入自由文本(含未知前缀如 `http://`)。
pub fn parse(raw: &str) -> (String, Vec<Constraint>) {
    let mut free: Vec<String> = Vec::new();
    let mut constraints: Vec<Constraint> = Vec::new();
    for tok in tokenize(raw) {
        // `-field:value` 取反;裸 `-foo`(无冒号)按普通自由文本处理
        let (negate, body) = match tok.strip_prefix('-') {
            Some(rest) if rest.contains(':') => (true, rest),
            _ => (false, tok.as_str()),
        };
        if let Some((key, value)) = body.split_once(':') {
            if let Some(field) = field_of(&key.to_lowercase()) {
                let value = value.trim();
                if !value.is_empty() {
                    constraints.push(Constraint {
                        field,
                        value: value.to_string(),
                        negate,
                    });
                    continue;
                }
            }
        }
        if !tok.is_empty() {
            free.push(tok);
        }
    }
    (free.join(" "), constraints)
}

/// 按空白切词,但双引号内的空白不切;引号本身丢弃。
fn tokenize(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut has_token = false;
    for ch in raw.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quote => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(field: Field, value: &str, negate: bool) -> Constraint {
        Constraint {
            field,
            value: value.to_string(),
            negate,
        }
    }

    #[test]
    fn parse_splits_free_text_and_constraints() {
        let (free, cs) = parse("配置表 tag:规范 path:vcs -type:png");
        assert_eq!(free, "配置表");
        assert_eq!(
            cs,
            vec![
                c(Field::Tag, "规范", false),
                c(Field::Path, "vcs", false),
                c(Field::Type, "png", true),
            ]
        );
    }

    #[test]
    fn parse_quoted_value_keeps_spaces() {
        let (free, cs) = parse(r#"title:"配置 表" 战斗"#);
        assert_eq!(free, "战斗");
        assert_eq!(cs, vec![c(Field::Title, "配置 表", false)]);
    }

    #[test]
    fn parse_bare_quotes_join_free_text() {
        let (free, cs) = parse(r#""数值 公式""#);
        assert_eq!(free, "数值 公式");
        assert!(cs.is_empty());
    }

    #[test]
    fn parse_unknown_prefix_is_free_text() {
        let (free, cs) = parse("看 http://x.com");
        assert_eq!(free, "看 http://x.com");
        assert!(cs.is_empty());
    }

    #[test]
    fn parse_aliases_and_empty_value() {
        let (free, cs) = parse("tags:bug ext:md kb:");
        assert_eq!(free, "kb:"); // 空值不成算子 → 退回自由文本
        assert_eq!(
            cs,
            vec![c(Field::Tag, "bug", false), c(Field::Type, "md", false)]
        );
    }

    #[test]
    fn tag_matches_nested_prefix_not_substring() {
        let cs = vec![c(Field::Tag, "a", false)];
        assert!(doc_allowed(&cs, "kb", "p", "md", "a,x", "t", "")); // 整段相等
        assert!(doc_allowed(&cs, "kb", "p", "md", "a/b", "t", "")); // 嵌套子标签
        assert!(!doc_allowed(&cs, "kb", "p", "md", "abc", "t", "")); // 不做子串误命中
    }

    #[test]
    fn type_matches_extension_with_optional_dot() {
        assert!(doc_allowed(&[c(Field::Type, ".MD", false)], "k", "p", "md", "", "", ""));
        assert!(!doc_allowed(&[c(Field::Type, "md", false)], "k", "p", "png", "", "", ""));
    }

    #[test]
    fn negate_excludes_match() {
        let cs = vec![c(Field::Path, "vcs", true)];
        assert!(doc_allowed(&cs, "k", "upload/a.md", "md", "", "", ""));
        assert!(!doc_allowed(&cs, "k", "vcs/repo/a.md", "md", "", "", ""));
    }

    #[test]
    fn multiple_constraints_are_anded() {
        let cs = vec![c(Field::Tag, "规范", false), c(Field::Type, "md", false)];
        assert!(doc_allowed(&cs, "k", "p", "md", "规范,配置", "", ""));
        assert!(!doc_allowed(&cs, "k", "p", "png", "规范", "", "")); // 类型不符
        assert!(!doc_allowed(&cs, "k", "p", "md", "配置", "", "")); // 标签不符
    }

    #[test]
    fn prop_matches_key_value_and_existence() {
        let props = r#"{"status":"Done","owner":"yvo"}"#;
        assert!(doc_allowed(&[c(Field::Prop, "status=done", false)], "k", "p", "md", "", "", props));
        assert!(doc_allowed(&[c(Field::Prop, "status", false)], "k", "p", "md", "", "", props)); // 仅判存在
        assert!(!doc_allowed(&[c(Field::Prop, "status=wip", false)], "k", "p", "md", "", "", props));
        assert!(!doc_allowed(&[c(Field::Prop, "missing", false)], "k", "p", "md", "", "", props));
        assert!(!doc_allowed(&[c(Field::Prop, "status=done", false)], "k", "p", "md", "", "", "")); // 无属性
    }

    #[test]
    fn parse_prop_operator() {
        let (free, cs) = parse("方案 prop:status=done property:owner");
        assert_eq!(free, "方案");
        assert_eq!(
            cs,
            vec![c(Field::Prop, "status=done", false), c(Field::Prop, "owner", false)]
        );
    }
}
