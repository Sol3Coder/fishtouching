//! XPath —— 真书源用到的那个子集。
//!
//! 范围是**量出来的**，不是照着 XPath 1.0 做的。拿 3907 个真书源统计，
//! 691 条 XPath 表达式里：
//!
//! * 轴只有两个：`following-sibling::` 39 次、`preceding-sibling::` 3 次。
//!   没有 `ancestor::`、`descendant::`、`@*` 这些。
//! * 谓词只有四种形状：`[@a='v']` 366、`[n]` 148、`[函数()='v']` 41、
//!   `[子元素名]` 15，外加 10 条 `[@a=1 or @b=2]`。
//! * 最高频的几条：`//*[@id="list"]//dt[2]/following-sibling::dd/a`、
//!   `//*[text()="作者"]//text()`、`//meta[@property='og:novel:author']/@content`。
//!
//! 所以实现的是：绝对路径 `/` `//`、名字与 `*`、上面那四种谓词、两个兄弟轴、
//! `|` 并集、以及尾部的 `/@属性` `/text()` 取值器。其余（分组 `(...)`、
//! 其它轴、字符串函数）**报错**，不猜。
//!
//! 求值结果是**元素下标**（文档序），尾部取值器交给 `Html::accessor` 处理 ——
//! 和 CSS 那条路共用一套取值逻辑。

use crate::html::Html;
use crate::{Error, R};

/// 轴。真书源里只用到这四个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// `/` —— 直接子元素
    Child,
    /// `//` —— 后代
    Descendant,
    /// `following-sibling::`
    FollowingSibling,
    /// `preceding-sibling::`
    PrecedingSibling,
    /// `..`
    Parent,
    /// `.`
    SelfAxis,
}

/// 节点测试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Test {
    /// 元素名（HTML 里小写比较）
    Name(String),
    /// `*`
    Any,
    /// `text()`
    Text,
    /// `@name`
    Attr(String),
}

/// 谓词。真书源里只有这几种。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pred {
    /// `[n]` —— 位置，1 开始。负数是「从末尾数」（XPath 没这条，宽容一下）
    Index(i64),
    /// `[last()]`
    Last,
    /// `[position()=n]`
    Position(i64),
    /// `[@a='v']`
    AttrEq(String, String),
    /// `[@a!='v']`
    AttrNe(String, String),
    /// `[@a]`
    AttrExists(String),
    /// `[text()='v']`
    TextEq(String),
    /// `[contains(@a,'v')]` / `[contains(text(),'v')]`
    Contains(String, String),
    /// `[starts-with(@a,'v')]`
    StartsWith(String, String),
    /// `[子元素名]` —— 有这个名字的子元素
    ChildExists(String),
    /// `[@a=1 or @b=2]`
    Or(Vec<Pred>),
    /// `[@a=1 and @b=2]`
    And(Vec<Pred>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub axis: Axis,
    pub test: Test,
    pub preds: Vec<Pred>,
}

/// 一条 XPath：`|` 分开的若干条路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XPath {
    pub paths: Vec<Vec<Step>>,
}

impl XPath {
    /// 求值，返回**元素下标**（文档序、去重）。
    pub fn select(&self, html: &Html, root: Option<usize>) -> R<Vec<usize>> {
        let mut out: Vec<usize> = Vec::new();
        for path in &self.paths {
            // 起点可能是**文档**（`None`）而不是某个元素 —— `/html` 的意思是
            // 「根的孩子里叫 html 的那个」，所以第一步得站在文档上算。
            let mut cur: Vec<Option<usize>> = vec![root];
            for step in path {
                let mut next: Vec<Option<usize>> = Vec::new();
                for node in &cur {
                    let cands = candidates(html, *node, step);
                    next.extend(
                        apply_preds(html, &cands, &step.preds)?
                            .into_iter()
                            .map(Some),
                    );
                }
                next.sort_unstable();
                next.dedup();
                cur = next;
            }
            out.extend(cur.into_iter().flatten());
        }
        // 文档序 = `ids` 里的下标顺序，正好可以直接排
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }
}

/// 一个轴 + 节点测试给出候选节点。
fn candidates(html: &Html, node: Option<usize>, step: &Step) -> Vec<usize> {
    let raw: Vec<usize> = match step.axis {
        Axis::Child => html.children(node),
        Axis::Descendant => html.subtree(node),
        // 这几个轴要有个具体节点才有意义；站在文档上没有兄弟/父
        Axis::FollowingSibling => node.map(|n| html.siblings_after(n)).unwrap_or_default(),
        Axis::PrecedingSibling => node.map(|n| html.siblings_before(n)).unwrap_or_default(),
        Axis::Parent => node.and_then(|n| html.parent_of(n)).into_iter().collect(),
        Axis::SelfAxis => node.into_iter().collect(),
    };
    raw.into_iter()
        .filter(|i| matches_test(html, *i, &step.test))
        .collect()
}

fn matches_test(html: &Html, i: usize, test: &Test) -> bool {
    match test {
        Test::Any => true,
        Test::Name(n) => html.tag_name(i).eq_ignore_ascii_case(n),
        // `text()` 和 `@a` 当步的时候落到取值器那边，这里不会出现
        Test::Text | Test::Attr(_) => true,
    }
}

/// 按顺序应用谓词。
///
/// **位置谓词要看「同一个父节点下的候选列表」**（XPath 就是这么定义的）：
/// `/div/p[2]` 是「每个 div 的第 2 个 p」，不是「全部 p 里的第 2 个」。
/// 所以这里在 `candidates` 的结果（已经是单个父节点的候选）上算位置。
fn apply_preds(html: &Html, cands: &[usize], preds: &[Pred]) -> R<Vec<usize>> {
    let mut cur: Vec<usize> = cands.to_vec();
    for p in preds {
        match p {
            Pred::Index(n) => {
                let len = cur.len() as i64;
                let idx = if *n < 0 { len + *n } else { *n - 1 };
                cur = if idx >= 0 && idx < len {
                    vec![cur[idx as usize]]
                } else {
                    Vec::new()
                };
            }
            Pred::Last => {
                cur = cur.last().copied().into_iter().collect();
            }
            Pred::Position(n) => {
                let idx = *n - 1;
                cur = if idx >= 0 && (idx as usize) < cur.len() {
                    vec![cur[idx as usize]]
                } else {
                    Vec::new()
                };
            }
            _ => cur.retain(|i| matches_pred(html, *i, p)),
        }
    }
    Ok(cur)
}

fn matches_pred(html: &Html, i: usize, p: &Pred) -> bool {
    match p {
        Pred::AttrEq(a, v) => html.attr(i, a) == *v,
        Pred::AttrNe(a, v) => html.attr(i, a) != *v,
        Pred::AttrExists(a) => html.has_attr(i, a),
        // `text()` 用**直接文本子节点**（`own_text`），不是整棵子树的文本。
        // 用 `text()` 的话，`//*[contains(text(),'状态')]` 会把 html/body/div
        // 这些祖先全匹配上 —— XPath 的 text() 节点集不含后代文本。
        Pred::TextEq(v) => html.own_text(i).trim() == v.trim(),
        Pred::Contains(what, v) => match what.as_str() {
            "text()" => html.own_text(i).contains(v.as_str()),
            _ => html.attr(i, what).contains(v.as_str()),
        },
        Pred::StartsWith(what, v) => match what.as_str() {
            "text()" => html.own_text(i).starts_with(v.as_str()),
            _ => html.attr(i, what).starts_with(v.as_str()),
        },
        Pred::ChildExists(name) => html
            .children(Some(i))
            .iter()
            .any(|c| html.tag_name(*c).eq_ignore_ascii_case(name)),
        Pred::Or(ps) => ps.iter().any(|x| matches_pred(html, i, x)),
        Pred::And(ps) => ps.iter().all(|x| matches_pred(html, i, x)),
        // 位置谓词在 apply_preds 里处理掉了
        Pred::Index(_) | Pred::Last | Pred::Position(_) => true,
    }
}

// ── 解析 ──────────────────────────────────────────────────────────────

/// 解析一条 XPath。支持 `a | b`（并集）。
pub fn parse(expr: &str) -> R<XPath> {
    let mut paths = Vec::new();
    for part in split_union(expr) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        paths.push(parse_path(part)?);
    }
    if paths.is_empty() {
        return Err(Error::Unsupported(format!("空的 XPath：{expr}")));
    }
    Ok(XPath { paths })
}

/// 在顶层（方括号和引号之外）按 `|` 切。
fn split_union(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            '[' | '(' => {
                depth += 1;
                cur.push(c);
            }
            ']' | ')' => {
                depth -= 1;
                cur.push(c);
            }
            '|' if depth <= 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn parse_path(s: &str) -> R<Vec<Step>> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    let mut steps: Vec<Step> = Vec::new();

    // `.` / `..` 开头的相对路径
    if chars.first() == Some(&'.') {
        if chars.get(1) == Some(&'.') {
            steps.push(Step {
                axis: Axis::Parent,
                test: Test::Any,
                preds: Vec::new(),
            });
            i = 2;
        } else {
            steps.push(Step {
                axis: Axis::SelfAxis,
                test: Test::Any,
                preds: Vec::new(),
            });
            i = 1;
        }
    }

    while i < chars.len() {
        // `(` 开头是分组：XPath 1.0 有，但真书源里只出现 1 次，不做
        if chars[i] == '(' {
            return Err(Error::Unsupported(format!("XPath 里的分组：{s}")));
        }
        let descendant = if chars[i] == '/' {
            if chars.get(i + 1) == Some(&'/') {
                i += 2;
                true
            } else {
                i += 1;
                false
            }
        } else if steps.is_empty() && i == 0 {
            // 相对路径，第一个步没有前导 `/`
            false
        } else {
            return Err(Error::Unsupported(format!(
                "XPath 里意料之外的字符 `{}`：{s}",
                chars[i]
            )));
        };
        if i >= chars.len() {
            break; // 结尾多一个 `/`，忽略
        }
        let (step, ni) = parse_step(&chars, i, descendant, s)?;
        i = ni;
        steps.push(step);
    }
    if steps.is_empty() {
        return Err(Error::Unsupported(format!("解析不出步的 XPath：{s}")));
    }
    Ok(steps)
}

fn parse_step(chars: &[char], mut i: usize, descendant: bool, full: &str) -> R<(Step, usize)> {
    // 先读一段可能的名字（也可能是 `axis::` 或 `text()`）
    let start = i;
    let mut word = String::new();
    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '-') {
        word.push(chars[i]);
        i += 1;
    }
    // `::` → 轴
    let mut axis = if descendant {
        Axis::Descendant
    } else {
        Axis::Child
    };
    if chars.get(i) == Some(&':') && chars.get(i + 1) == Some(&':') {
        i += 2;
        axis = match word.to_ascii_lowercase().as_str() {
            "following-sibling" => Axis::FollowingSibling,
            "preceding-sibling" => Axis::PrecedingSibling,
            "child" => Axis::Child,
            "descendant" => Axis::Descendant,
            "self" => Axis::SelfAxis,
            "parent" => Axis::Parent,
            other => {
                return Err(Error::Unsupported(format!("XPath 的轴 `{other}::`")));
            }
        };
        // 轴后面还要一个名字
        word.clear();
        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '-')
        {
            word.push(chars[i]);
            i += 1;
        }
    } else if word == ".." {
        return Ok((
            Step {
                axis: Axis::Parent,
                test: Test::Any,
                preds: Vec::new(),
            },
            start + 2,
        ));
    }

    // `/..` —— 父节点轴
    if word.is_empty() && chars.get(i) == Some(&'.') && chars.get(i + 1) == Some(&'.') {
        i += 2;
        return Ok((
            Step {
                axis: Axis::Parent,
                test: Test::Any,
                preds: Vec::new(),
            },
            i,
        ));
    }

    let test = if word == "*" || word.is_empty() && chars.get(i) == Some(&'*') {
        if word.is_empty() {
            i += 1;
        }
        Test::Any
    } else if word == "text" && chars.get(i) == Some(&'(') {
        // `text()`
        i += 1;
        if chars.get(i) == Some(&')') {
            i += 1;
        }
        Test::Text
    } else if word.is_empty() {
        return Err(Error::Unsupported(format!("XPath 里缺节点名：{full}")));
    } else {
        Test::Name(word)
    };

    // 谓词
    let mut preds = Vec::new();
    while chars.get(i) == Some(&'[') {
        let (p, ni) = parse_pred(chars, i, full)?;
        i = ni;
        preds.push(p);
    }
    Ok((Step { axis, test, preds }, i))
}

fn parse_pred(chars: &[char], mut i: usize, full: &str) -> R<(Pred, usize)> {
    debug_assert_eq!(chars.get(i), Some(&'['));
    i += 1;
    let start = i;
    let mut depth = 1i32;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if i >= chars.len() {
        return Err(Error::Unsupported(format!("XPath 的 `[` 没闭合：{full}")));
    }
    let inner: String = chars[start..i].iter().collect();
    let next = i + 1;
    Ok((parse_pred_text(inner.trim())?, next))
}

/// 把一个谓词表达式解析成 [`Pred`]。
fn parse_pred_text(s: &str) -> R<Pred> {
    // `or` / `and`（顶层）
    if let Some(parts) = split_top(s, " or ") {
        let ps = parts
            .into_iter()
            .map(|p| parse_pred_text(p.trim()))
            .collect::<R<Vec<_>>>()?;
        return Ok(Pred::Or(ps));
    }
    if let Some(parts) = split_top(s, " and ") {
        let ps = parts
            .into_iter()
            .map(|p| parse_pred_text(p.trim()))
            .collect::<R<Vec<_>>>()?;
        return Ok(Pred::And(ps));
    }
    // `last()`
    if s == "last()" {
        return Ok(Pred::Last);
    }
    // `position()=n`
    if let Some(rest) = s.strip_prefix("position()") {
        let rest = rest.trim_start_matches(['=', ' ']);
        return rest
            .parse::<i64>()
            .map(Pred::Position)
            .map_err(|_| Error::Unsupported(format!("XPath 的 `position(){rest}`")));
    }
    // `contains(...)` / `starts-with(...)`
    for (fname, mk) in [
        ("contains", Pred::Contains as fn(String, String) -> Pred),
        (
            "starts-with",
            Pred::StartsWith as fn(String, String) -> Pred,
        ),
    ] {
        if let Some(rest) = s.strip_prefix(fname) {
            let rest = rest
                .trim_start()
                .strip_prefix('(')
                .ok_or_else(|| Error::Unsupported(format!("XPath 的 `{s}`")))?;
            let inner = rest.strip_suffix(')').unwrap_or(rest);
            let (a, b) = inner.split_once(',').ok_or_else(|| {
                Error::Unsupported(format!("XPath 的 `{s}`（{fname} 要两个参数）"))
            })?;
            // 第一个参数可能写成 `@attr`（要去掉 `@`）或 `text()`
            let a = strip_quotes(a.trim());
            let a = a.strip_prefix('@').unwrap_or(&a).to_string();
            return Ok(mk(a, strip_quotes(b.trim())));
        }
    }
    // `text()='v'`
    if let Some(rest) = s.strip_prefix("text()") {
        let rest = rest.trim();
        if let Some(v) = rest.strip_prefix("==").or_else(|| rest.strip_prefix('=')) {
            return Ok(Pred::TextEq(strip_quotes(v.trim())));
        }
        return Err(Error::Unsupported(format!("XPath 的 `{s}`")));
    }
    // `@a='v'` / `@a!='v'` / `@a`
    if let Some(rest) = s.strip_prefix('@') {
        let (name, tail) = split_attr(rest);
        let tail = tail.trim();
        if tail.is_empty() {
            return Ok(Pred::AttrExists(name));
        }
        if let Some(v) = tail.strip_prefix("!=") {
            return Ok(Pred::AttrNe(name, strip_quotes(v.trim())));
        }
        if let Some(v) = tail.strip_prefix('=') {
            return Ok(Pred::AttrEq(name, strip_quotes(v.trim())));
        }
        return Err(Error::Unsupported(format!("XPath 的 `{s}`")));
    }
    // `[n]`
    if let Ok(n) = s.parse::<i64>() {
        return Ok(Pred::Index(n));
    }
    // `[子元素名]`
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        && !s.is_empty()
    {
        return Ok(Pred::ChildExists(s.to_string()));
    }
    Err(Error::Unsupported(format!("XPath 的谓词 `[{s}]`")))
}

/// 读属性名，返回 `(名字, 剩下的部分)`。
fn split_attr(s: &str) -> (String, &str) {
    let end = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == ':'))
        .unwrap_or(s.len());
    (s[..end].to_string(), &s[end..])
}

fn strip_quotes(s: &str) -> String {
    let t = s.trim();
    let b = t.as_bytes();
    if t.len() >= 2
        && ((b[0] == b'\'' && b[t.len() - 1] == b'\'') || (b[0] == b'"' && b[t.len() - 1] == b'"'))
    {
        return t[1..t.len() - 1].to_string();
    }
    t.to_string()
}

/// 在顶层（引号外）按 `sep` 切。
fn split_top(s: &str, sep: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut quote: Option<char> = None;
    let mut last = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = s[i..].chars().next().expect("一直在字符边界上");
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += c.len_utf8();
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            i += c.len_utf8();
            continue;
        }
        if s[i..].starts_with(sep) {
            out.push(s[last..i].to_string());
            i += sep.len();
            last = i;
            continue;
        }
        i += c.len_utf8();
    }
    if out.is_empty() {
        return None;
    }
    out.push(s[last..].to_string());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h() -> Html<'static> {
        Html::parse(
            r#"<html><body>
                 <div id="list"><dl>
                   <dt>作者</dt><dd><a href="/a">甲</a></dd>
                   <dt>状态</dt><dd><a href="/b">乙</a></dd>
                 </dl></div>
                 <meta property="og:novel:author" content="天蚕土豆">
                 <meta property="og:image" content="/img.jpg">
                 <div class="x"><p>一</p><p>二</p><p>三</p></div>
                 <ul><li><span>有span</span></li><li>没有</li></ul>
               </body></html>"#,
        )
    }

    fn tag(h: &Html, i: usize) -> String {
        h.tag_name(i)
    }

    fn attrs(h: &Html, ids: &[usize], name: &str) -> Vec<String> {
        ids.iter().map(|i| h.attr(*i, name)).collect()
    }

    fn sel(expr: &str) -> Vec<usize> {
        let doc = h();
        parse(expr)
            .unwrap_or_else(|e| panic!("解析 {expr} 失败：{e}"))
            .select(&doc, None)
            .unwrap_or_else(|e| panic!("求值 {expr} 失败：{e}"))
    }

    #[test]
    fn absolute_and_descendant_paths() {
        let doc = h();
        let ids = sel("//meta");
        assert_eq!(ids.len(), 2);
        assert_eq!(
            attrs(&doc, &ids, "property"),
            ["og:novel:author", "og:image"]
        );
        // `/` 是直接子级
        let ids = sel("/html/body/div");
        assert_eq!(ids.len(), 2, "两个顶层 div");
        assert_eq!(tag(&doc, ids[0]), "div");
    }

    /// 最高频的一条（26 次）：`//*[@id="list"]//dt[2]/following-sibling::dd/a`
    #[test]
    fn the_most_common_real_expression() {
        let doc = h();
        let ids = sel(r#"//*[@id="list"]//dt[2]/following-sibling::dd/a"#);
        assert_eq!(ids.len(), 1);
        assert_eq!(doc.attr(ids[0], "href"), "/b");
        assert_eq!(doc.text(ids[0]), "乙");
    }

    #[test]
    fn following_and_preceding_siblings() {
        let doc = h();
        // dt[1] 后面的兄弟 dd
        // XPath 的 following-sibling **不**在下一个 dt 处停下 —— 是两个 dd
        let ids = sel(r#"//dt[1]/following-sibling::dd"#);
        assert_eq!(ids.len(), 2);
        assert_eq!(doc.text(ids[0]), "甲");
        // 倒数第二个 dt 之前的兄弟
        let ids = sel("//dd[1]/preceding-sibling::dt");
        assert_eq!(ids.len(), 1);
        assert_eq!(doc.text(ids[0]), "作者");
    }

    /// 位置谓词是**每个父节点各算各的**（XPath 的语义）。
    #[test]
    fn position_predicates_are_per_parent() {
        let doc = h();
        let ids = sel("//div[@class='x']/p[2]");
        assert_eq!(ids.len(), 1);
        assert_eq!(doc.text(ids[0]), "二");
        // 全部 p 里的第 2 个只应该有一个（不是每组一个）
        let all = sel("//div[@class='x']/p");
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn attribute_predicates() {
        assert_eq!(
            attrs(&h(), &sel(r#"//meta[@property='og:image']"#), "content"),
            ["/img.jpg"]
        );
        // 带引号的两种都认
        assert_eq!(sel(r#"//meta[@property="og:image"]"#).len(), 1);
        // 不等
        assert_eq!(sel("//meta[@property!='og:image']").len(), 1);
        // 存在
        assert_eq!(sel("//meta[@property]").len(), 2);
        assert_eq!(sel("//meta[@nope]").len(), 0);
    }

    #[test]
    fn text_predicates_and_functions() {
        let doc = h();
        let ids = sel(r#"//*[text()="作者"]"#);
        assert_eq!(ids.len(), 1);
        assert_eq!(doc.text(ids[0]), "作者");
        // contains / starts-with
        assert_eq!(sel(r#"//meta[contains(@property,'author')]"#).len(), 1);
        assert_eq!(sel(r#"//meta[starts-with(@property,'og:novel')]"#).len(), 1);
        assert_eq!(sel(r#"//*[contains(text(),'状态')]"#).len(), 1);
    }

    #[test]
    fn child_name_predicate() {
        // `//li[span]` —— 有 span 子元素的那个 li
        let doc = h();
        let ids = sel("//li[span]");
        assert_eq!(ids.len(), 1);
        assert_eq!(doc.text(ids[0]), "有span");
    }

    #[test]
    fn or_and_predicates() {
        let ids = sel(r#"//meta[@property='og:image' or @property='og:novel:author']"#);
        assert_eq!(ids.len(), 2, "两个都该中");
        let ids = sel(r#"//meta[@property='og:image' and @content='/img.jpg']"#);
        assert_eq!(ids.len(), 1);
    }

    #[test]
    fn union_dedupes_and_sorts_in_document_order() {
        // 并集结果必须是文档序（`ids` 下标顺序）
        let ids = sel("//meta[@property='og:image'] | //div[@id='list']");
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "结果要按文档序：{ids:?}"
        );
        let unduped = sel("//meta | //meta");
        assert_eq!(unduped.len(), 2, "并集不该重复");
    }

    #[test]
    fn last_and_position() {
        let doc = h();
        let ids = sel("//div[@class='x']/p[last()]");
        assert_eq!(doc.text(ids[0]), "三");
        let ids = sel("//div[@class='x']/p[position()=1]");
        assert_eq!(doc.text(ids[0]), "一");
    }

    #[test]
    fn parent_and_self() {
        let doc = h();
        let ids = sel("//a[@href='/a']/..");
        assert_eq!(tag(&doc, ids[0]), "dd");
        let ids = sel("//a[@href='/a']/parent::dd");
        assert_eq!(tag(&doc, ids[0]), "dd");
    }

    #[test]
    fn unsupported_syntax_errors_instead_of_guessing() {
        for bad in [
            "(//div)[1]",         // 分组
            "//div/ancestor::x",  // 没用到的轴
            "//div[count(p)=2]",  // 字符串/数值函数
            "//div[@a>1]",        // 比较运算符
            "//div/following::x", // 没用到的轴
        ] {
            assert!(
                parse(bad).map(|x| x.select(&h(), None)).is_err(),
                "{bad} 应当报错，而不是给出猜出来的结果"
            );
        }
    }

    #[test]
    fn malformed_input_does_not_panic() {
        for bad in ["", "//", "/", "//[", "//div[", "//div[@", "@@", "//a/"] {
            let _ = parse(bad).map(|x| x.select(&h(), None));
        }
    }

    #[test]
    fn the_accessor_is_split_off_by_the_caller() {
        // 这里只管路径 —— `/@content` 和 `/text()` 由 crawl 那边拆成取值器，
        // 和 CSS 那条路共用 `Html::accessor`
        let doc = h();
        let ids = parse(r#"//meta[@property='og:image']"#)
            .unwrap()
            .select(&doc, None)
            .unwrap();
        let got = doc.accessor(&ids.iter().map(|i| Some(*i)).collect::<Vec<_>>(), "content");
        assert_eq!(got, ["/img.jpg"]);
    }
}
