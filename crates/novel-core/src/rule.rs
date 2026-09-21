//! legado 的规则引擎 —— 切分、前缀识别、`##` 替换、`{{ }}` 表达式。
//!
//! 移植自 `RuleAnalyzer.kt` / `AnalyzeRule.kt`，只覆盖**不需要 JS 解释器**的那部分。
//! 遇到真的要 JS 的规则（`@js:`、`{{java.ajax(...)}}`、`@webjs:`）返回
//! [`Error::Unsupported`]，绝不猜 —— 猜错的表现是「静默抓到脏数据」，比报错难查得多。

use std::time::{SystemTime, UNIX_EPOCH};

pub use crate::{Error, R};

// ── 规则切分 ────────────────────────────────────────────────────────────

/// 按 `seps` 里**最先出现**的那个切分，但跳过 `[...]` / `(...)` 里的分隔符。
///
/// 对应 `RuleAnalyzer.splitRule`。Kotlin 那边用递归 + 平衡组跳过实现，
/// 效果等价于「一遍扫描，跳过方括号/圆括号内部」，所以这里直接写成一遍扫描。
///
/// 与 Kotlin 的两点不同（都是刻意的）：
/// * 括号不平衡不报错，当成「一直到结尾」—— 垃圾规则不该让整个抓取崩掉；
/// * 引号只在括号内部起作用。Kotlin 的普通切分路径也确实不认引号。
pub fn split_rule(s: &str, seps: &[&str]) -> (Vec<String>, Option<String>) {
    let Some((kind, _)) = outside_occurrences(s, seps)
        .first()
        .copied()
        .map(|(_, _, k)| (k, ()))
    else {
        return (vec![s.to_string()], None);
    };
    let sep = seps[kind].to_string();
    let mut out = Vec::new();
    let mut last = 0usize;
    for (at, len, k) in outside_occurrences(s, seps) {
        if k != kind || at < last {
            continue;
        }
        out.push(s[last..at].to_string());
        last = at + len;
    }
    out.push(s[last..].to_string());
    (out, Some(sep))
}

/// 所有「不在括号里」的分隔符出现位置：`(字节下标, 长度, seps 里的序号)`。
fn outside_occurrences(s: &str, seps: &[&str]) -> Vec<(usize, usize, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if depth > 0 {
            match quote {
                // 引号里：只找配对的引号
                Some(q) => {
                    if c == q {
                        quote = None;
                    }
                }
                None => match c {
                    b'\'' | b'"' => quote = Some(c),
                    // 转义只在括号里、且不在引号里生效（照抄 chompRuleBalanced）
                    b'\\' => i += 1,
                    b'[' | b'(' => depth += 1,
                    b']' | b')' => depth -= 1,
                    _ => {}
                },
            }
            i += 1;
            continue;
        }
        if c == b'[' || c == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if let Some(k) = seps
            .iter()
            .position(|sep| b[i..].starts_with(sep.as_bytes()))
        {
            out.push((i, seps[k].len(), k));
            i += seps[k].len();
            continue;
        }
        i += 1;
    }
    out
}

/// 去掉规则头部的 `@`、空白和控制字符（对应 `RuleAnalyzer.trim`）。
fn trim_head(s: &str) -> &str {
    s.trim_start_matches(|c: char| c == '@' || c <= ' ')
}

// ── 规则本身 ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 默认：HTML + CSS 方言
    Default,
    /// `$.` / `$[` / `@json:`
    Json,
    /// `@xpath:` / `/` / `//`
    XPath,
}

/// 一条 `##` 替换规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replace {
    pub pattern: String,
    pub with: String,
    /// `rule##regex##repl###` —— 末尾还有一段时只替第一个匹配（legado 的约定）
    pub first: bool,
}

/// 求值 `{{ }}` 里那种「内嵌规则」的入口：给规则文本，还一个结果。
///
/// 单独起个名字是因为 `Option<Rc<dyn Fn(&str) -> Option<String>>>` 直接写在字段上
/// 会让 `Env` 的定义很难读。
pub type RuleEval = std::rc::Rc<dyn Fn(&str) -> Option<String>>;

/// 求值一条规则需要的外部输入。
#[derive(Clone, Default)]
pub struct Env {
    pub key: String,
    pub page: u32,
    /// 有它就能跑 JS 规则；没有就报 `Unsupported`。
    /// 这样纯离线的测试不需要起 QuickJS，而线上遇到 `@js:` 也不会静默取空。
    pub js: Option<std::rc::Rc<crate::js::Js>>,
    /// 求值 `{{@@class.x@text}}` 这类「方括号里是规则」的写法。
    ///
    /// legado 的 `isRule()` 规定：`{{ }}` 里的东西以 `@` / `$.` / `$[` / `//` 开头
    /// 就是**规则**（拿当前内容求值），否则才是 JS。真书源里这种写法有 700 多处。
    pub rule_eval: Option<RuleEval>,
}

impl Env {
    pub fn new(key: &str, page: u32) -> Self {
        Self {
            key: key.to_string(),
            page,
            js: None,
            rule_eval: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub mode: Mode,
    /// 规则主体（前缀和 `##` 之后的部分）
    pub text: String,
    /// 来自 `@css:` 前缀。为真时整段是一个 CSS 选择器，
    /// `@` 只在最后出现一次（分隔取值器），不参与逐级收窄。
    pub css: bool,
    /// 来自 `@@` 前缀：**强制**按 HTML 选择器解，即使正文是 JSON。
    ///
    /// legado 在 `SourceRule` 里把 `@@`（和 `@css:`）排在 `isJSON` 之前 ——
    /// 正文是 JSON 时所有规则默认都当 JSON 路径，作者要写选择器得用 `@@` 逃出来。
    pub escaped: bool,
    /// 尾部的 `@js:` / `<js>` 脚本。求值顺序是「先算 text，把结果当 `result`
    /// 喂给脚本，脚本的输出才是最终结果」—— 和 legado 的 `splitSourceRule` 一致。
    pub js: Option<String>,
    pub replace: Option<Replace>,
}

impl Rule {
    /// 解析一条规则。
    pub fn parse(raw: &str, key: &str, page: u32) -> R<Self> {
        Self::parse_env(raw, &Env::new(key, page))
    }

    /// 解析一条规则。`env` 提供 `{{ }}` 的 key/page，以及可选的 JS 环境。
    pub fn parse_env(raw: &str, env: &Env) -> R<Self> {
        let raw = raw.trim();
        // `@js:` 之后的**全部**内容都是 JS（legado 的正则就是 `@js:([\w\W]*)`，
        // 吃到字符串末尾），所以 `##` 出现在 `@js:` 后面时属于脚本，不是替换。
        let (raw, js) = match split_js(raw) {
            Some((before, script)) => (before, Some(script)),
            None => (raw, None),
        };
        let raw = raw.trim();
        let mut css = false;
        let mut escaped = false;
        let (mode, mut body) = match () {
            _ if starts_ci(raw, "@css:") => {
                css = true;
                (Mode::Default, &raw[5..])
            }
            _ if raw.starts_with("@@") => {
                escaped = true;
                (Mode::Default, &raw[2..])
            }
            _ if starts_ci(raw, "@json:") => (Mode::Json, &raw[6..]),
            _ if raw.starts_with("$.") || raw.starts_with("$[") => (Mode::Json, raw),
            _ if starts_ci(raw, "@xpath:") => (Mode::XPath, &raw[7..]),
            // `/` 开头就是 XPath（legado 的判据），`//` 是后代
            _ if raw.starts_with('/') => (Mode::XPath, raw),
            _ => (Mode::Default, raw),
        };
        // `@webjs:` 要真浏览器，只能拒
        if let Some(w) = find_ci(body, "@webjs:") {
            return Err(Error::Unsupported(body[w..].to_string()));
        }
        let owned;
        if body.contains("{{") {
            owned = expand_braces(body, env)?;
            body = &owned;
        }

        // `##` 分段。legado 用 Kotlin 的 split，尾部空段会被丢掉，
        // 所以 `a##b##` 是「替换所有」，`a##b###` 才是「只替第一个」。
        let segs: Vec<&str> = body.trim().split("##").collect();
        let text = trim_head(segs[0]).trim().to_string();
        let replace = (segs.len() > 1).then(|| Replace {
            pattern: segs[1].to_string(),
            with: segs.get(2).copied().unwrap_or("").to_string(),
            first: segs.len() > 3 && !segs[3].is_empty(),
        });
        Ok(Rule {
            mode,
            text,
            css,
            escaped,
            js,
            replace,
        })
    }

    /// 把 `##` 替换应用到一条结果上。
    ///
    /// `first` 的语义照抄 legado：只返回**第一个匹配所在的片段**（替换后），
    /// 没有匹配则返回空串。不是「整串只替一处」—— 这是个容易被想当然的地方。
    pub fn apply_replace(&self, s: &str) -> String {
        let Some(r) = &self.replace else {
            return s.to_string();
        };
        let Ok(re) = regex::Regex::new(&r.pattern) else {
            // 正则写坏了：legado 回落到字面量替换
            return s.replace(&r.pattern, &r.with);
        };
        if !r.first {
            return re.replace_all(s, r.with.as_str()).into_owned();
        }
        match re.find(s) {
            Some(m) => re.replace(m.as_str(), r.with.as_str()).into_owned(),
            None => String::new(),
        }
    }
}

/// 前缀匹配（忽略大小写）。
///
/// 必须用 `str::get` 而不是 `s[..n]` —— 规则里到处是中文（`text.下一页`），
/// 按字节切会在字符中间炸开。
/// legado 的 `isRule()`：`{{ }}` 里的东西以这些开头就当**规则**，不当 JS。
pub fn is_rule_pub(s: &str) -> bool {
    is_rule(s)
}

fn is_rule(s: &str) -> bool {
    s.starts_with('@') || s.starts_with("$.") || s.starts_with("$[") || s.starts_with("//")
}

fn starts_ci(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// 把规则拆成「普通规则 + 尾部 JS」。没有 JS 时返回 `None`。
///
/// `@js:` 之后一律是脚本（吃到末尾）；`<js>…</js>` 有闭合标签。
/// 只取**最后一个** JS 段 —— 一条规则里串多段 JS 的写法在真书源里没见到。
fn split_js(s: &str) -> Option<(&str, String)> {
    let at = find_ci(s, "@js:");
    let tag = s.find("<js>");
    match (at, tag) {
        (Some(a), t) if t.is_none() || t.unwrap() > a => Some((&s[..a], s[a + 4..].to_string())),
        (_, Some(t)) => {
            let rest = &s[t + 4..];
            let end = rest.find("</js>").unwrap_or(rest.len());
            Some((&s[..t], rest[..end].to_string()))
        }
        _ => None,
    }
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.to_ascii_lowercase();
    h.find(&needle.to_ascii_lowercase())
}

// ── `{{ }}` 表达式 ──────────────────────────────────────────────────────

/// 把字符串里所有 `{{ }}` 换成求值结果。
///
/// legado 这里一律当 JS 跑。真书源里 99% 的用法只是 `key` / `page` 的整数运算
/// （见 README 的统计），所以这里实现一个**只认算术和两个变量**的求值器；
/// 遇到 `java.ajax(...)` 这种就返回 Unsupported。
pub fn expand_braces(s: &str, env: &Env) -> R<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            // 没有配对的 }}，当成普通文本
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let expr = after[..end].trim();
        // 三档，从便宜到贵：
        // 1. 内联算术求值器 —— `{{page}}` 这类在真书源里出现 5 万多次，不该绕 JS；
        // 2. 里面是规则（`{{@@class.x@text}}` / `{{$.x}}`）→ 让调用方拿当前内容求值；
        // 3. 其余当 JS 跑。
        let value = match eval_expr(expr, &env.key, env.page) {
            Some(v) => Some(v),
            None if is_rule(expr) => match &env.rule_eval {
                Some(f) => f(expr),
                None => None,
            },
            None => match &env.js {
                Some(js) => match js.eval(expr, None) {
                    Ok(v) => Some(v.to_text()),
                    Err(e) => return Err(Error::Js(format!("{{{{{expr}}}}}：{e}"))),
                },
                None => None,
            },
        };
        match value {
            Some(v) => out.push_str(&v),
            None => return Err(Error::Unsupported(format!("{{{{{expr}}}}}"))),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Num(f64),
    Str(String),
}

impl Val {
    fn num(&self) -> Option<f64> {
        match self {
            Val::Num(n) => Some(*n),
            Val::Str(_) => None,
        }
    }
    fn into_text(self) -> String {
        match self {
            Val::Str(s) => s,
            Val::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{n:.0}"),
            Val::Num(n) => n.to_string(),
        }
    }
}

fn now_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// 给测试用：`{{ }}` 那套算术求值器。
pub fn eval_expr_pub(expr: &str, key: &str, page: u32) -> Option<String> {
    eval_expr(expr, key, page)
}

/// 求值一个 `{{ }}` 表达式。失败就返回 `None`，由调用方转成 Unsupported。
fn eval_expr(expr: &str, key: &str, page: u32) -> Option<String> {
    let expr = expr.trim();
    // 「报时」用的固定写法，真书源里就这么原样出现。
    // 特判一次比内置一个 JS 引擎便宜太多。
    if expr == "Date.now()" {
        return Some(format!("{:.0}", now_millis()));
    }
    let toks = lex(expr)?;
    let mut p = Parser {
        t: &toks,
        i: 0,
        key,
        page,
    };
    let v = p.expr(0)?;
    (p.i == p.t.len()).then(|| v.into_text())
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Op(char),
    LParen,
    RParen,
}

fn lex(s: &str) -> Option<Vec<Tok>> {
    let b: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '0'..='9' => {
                let mut j = i;
                while j < b.len() && (b[j].is_ascii_digit() || b[j] == '.') {
                    j += 1;
                }
                let text: String = b[i..j].iter().collect();
                out.push(Tok::Num(text.parse().ok()?));
                i = j;
            }
            '\'' | '"' => {
                let mut j = i + 1;
                let mut s = String::new();
                while j < b.len() && b[j] != c {
                    s.push(b[j]);
                    j += 1;
                }
                if j >= b.len() {
                    return None;
                }
                out.push(Tok::Str(s));
                i = j + 1;
            }
            c if c.is_alphabetic() || c == '_' || c == '$' => {
                // 从 i + 1 起扫，**不要**从 i 起：起始字符里有 `$` 和 `_`，
                // 而循环条件只认字母数字/`_`/`.`，从 i 起会原地打转，
                // 于是每轮都往 tokens 里塞一个 Ident，直到把内存吃光。
                let mut j = i + 1;
                while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_' || b[j] == '.') {
                    j += 1;
                }
                out.push(Tok::Ident(b[i..j].iter().collect()));
                i = j;
            }
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '+' | '-' | '*' | '/' | '%' => {
                out.push(Tok::Op(c));
                i += 1;
            }
            _ => return None,
        }
    }
    Some(out)
}

struct Parser<'a> {
    t: &'a [Tok],
    i: usize,
    key: &'a str,
    page: u32,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }

    /// 优先级爬升。`min` 是当前允许的最低优先级。
    fn expr(&mut self, min: u8) -> Option<Val> {
        let mut lhs = self.unary()?;
        while let Some(Tok::Op(op)) = self.peek() {
            let op = *op;
            let p = match op {
                '+' | '-' => 1,
                '*' | '/' | '%' => 2,
                _ => return None,
            };
            if p < min {
                break;
            }
            self.i += 1;
            let rhs = self.expr(p + 1)?;
            lhs = match (lhs, op, rhs) {
                // 数字 + 数字是加法；只要有一边是字符串，`+` 就是拼接（和 JS 一致）
                (Val::Num(x), '+', Val::Num(y)) => Val::Num(x + y),
                (a, '+', b) => Val::Str(a.into_text() + &b.into_text()),
                (a, op, b) => {
                    let (x, y) = (a.num()?, b.num()?);
                    Val::Num(match op {
                        '-' => x - y,
                        '*' => x * y,
                        '/' => {
                            if y == 0.0 {
                                return None;
                            }
                            x / y
                        }
                        '%' => {
                            if y == 0.0 {
                                return None;
                            }
                            x % y
                        }
                        _ => return None,
                    })
                }
            };
        }
        Some(lhs)
    }

    fn unary(&mut self) -> Option<Val> {
        match self.peek() {
            Some(Tok::Op('-')) => {
                self.i += 1;
                Some(Val::Num(-self.unary()?.num()?))
            }
            Some(Tok::Op('+')) => {
                self.i += 1;
                self.unary()
            }
            _ => self.atom(),
        }
    }

    fn atom(&mut self) -> Option<Val> {
        match self.peek()?.clone() {
            Tok::Num(n) => {
                self.i += 1;
                Some(Val::Num(n))
            }
            Tok::Str(s) => {
                self.i += 1;
                Some(Val::Str(s))
            }
            Tok::LParen => {
                self.i += 1;
                let v = self.expr(0)?;
                if *self.peek()? != Tok::RParen {
                    return None;
                }
                self.i += 1;
                Some(v)
            }
            Tok::Ident(name) => {
                self.i += 1;
                match name.as_str() {
                    "key" => Some(Val::Str(self.key.to_string())),
                    "page" => Some(Val::Num(self.page as f64)),
                    // Math.round(x) —— 真书源里只用来round一个时间戳
                    "Math.round" => {
                        if *self.peek()? != Tok::LParen {
                            return None;
                        }
                        self.i += 1;
                        let v = self.expr(0)?;
                        if *self.peek()? != Tok::RParen {
                            return None;
                        }
                        self.i += 1;
                        Some(Val::Num(v.num()?.round()))
                    }
                    // 词法会把 `new Date()` 切成三个 token，所以在这里拼回来
                    "new" => {
                        if !matches!(self.peek()?, Tok::Ident(n) if n == "Date") {
                            return None;
                        }
                        self.i += 1;
                        if *self.peek()? != Tok::LParen {
                            return None;
                        }
                        self.i += 1;
                        if *self.peek()? != Tok::RParen {
                            return None;
                        }
                        self.i += 1;
                        Some(Val::Num(now_millis()))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(s: &str) -> Vec<String> {
        split_rule(s, &["&&", "||", "%%"]).0
    }

    #[test]
    fn splits_on_the_first_separator_kind_only() {
        assert_eq!(sp("a&&b&&c"), ["a", "b", "c"]);
        assert_eq!(sp("a||b"), ["a", "b"]);
        assert_eq!(sp("a%%b"), ["a", "b"]);
        // 第一个出现的是 &&，后面的 || 就不算了
        assert_eq!(sp("a&&b||c"), ["a", "b||c"]);
        assert_eq!(sp("a"), ["a"]);
        assert_eq!(sp(""), [""]);
    }

    #[test]
    fn separators_inside_brackets_are_not_separators() {
        // 这是这个函数存在的唯一理由：`&&` 出现在选择器里
        assert_eq!(sp("div[a&&b]&&c"), ["div[a&&b]", "c"]);
        assert_eq!(sp("div:not(a&&b)||c"), ["div:not(a&&b)", "c"]);
        // 嵌套
        assert_eq!(sp("a[b[c&&d]]&&e"), ["a[b[c&&d]]", "e"]);
        // 括号里的引号还能再挡一层
        assert_eq!(sp("a['&&']&&b"), ["a['&&']", "b"]);
        // 不平衡：不报错，也不在括号里切
        assert_eq!(sp("a[b&&c"), ["a[b&&c"]);
    }

    #[test]
    fn at_split_keeps_accessor_last() {
        let (v, sep) = split_rule("class.tab@tag.p.0@text", &["@"]);
        assert_eq!(sep.as_deref(), Some("@"));
        assert_eq!(v, ["class.tab", "tag.p.0", "text"]);
        // @ 出现在方括号里不算
        let (v, _) = split_rule("a[href@x]@text", &["@"]);
        assert_eq!(v, ["a[href@x]", "text"]);
    }

    #[test]
    fn parses_prefixes() {
        let p = |s: &str| Rule::parse(s, "k", 1).unwrap();
        assert_eq!(p("a@text").mode, Mode::Default);
        assert_eq!(p("a@text").text, "a@text");
        assert_eq!(p("@css:a b@text").mode, Mode::Default);
        assert_eq!(p("@@a@text").text, "a@text");
        assert_eq!(p("@json:$.a").mode, Mode::Json);
        assert_eq!(p("$.a.b").mode, Mode::Json);
        assert_eq!(p("$[0]").mode, Mode::Json);
        // 不区分大小写
        assert_eq!(p("@JSON:$.a").mode, Mode::Json);
        assert_eq!(p("@CSS:a@text").mode, Mode::Default);
    }

    /// 中文规则曾经在这里把进程炸掉过：`starts_ci` 按字节切前缀，
    /// `text.下一页` 的第 6 个字节落在汉字中间。
    /// 把真书源集合里**全部** 15000 多条规则串过一遍。
    ///
    /// 这个测试的价值不在于断言什么，而在于**不 panic**。规则文本是这个项目里
    /// 最脏的输入：中文、方括号、引号、反斜杠、正则、嵌套 JSON、写到一半的语法。
    /// 前面两个崩溃（`starts_ci` 按字节切前缀、`s[i..]` 落在汉字中间）都是它抓出来的。
    ///
    /// 语料来自 3907 个真实书源（去重、截到 400 字）。抓一次新的：
    /// 见 README 里「重新生成测试语料」。
    ///
    /// 分隔符用的是回车符而不是换行符 —— 真规则里有换行（`@js:` 脚本几乎都有），
    /// 用换行符切会把一条规则切成好几条，JS 那边就全成了语法错误。
    #[test]
    fn every_real_world_rule_parses_without_panicking() {
        let corpus = include_str!("../tests/fixtures/rule-corpus.txt");
        let doc = crate::html::Html::parse(
            "<html><body><div class='a'><p id='b'>正文<span>子</span></p></div></body></html>",
        );
        let mut n = 0usize;
        for line in corpus.split('\r') {
            if line.trim().is_empty() {
                continue;
            }
            n += 1;
            // 切分
            let (parts, _) = split_rule(line, &["&&", "||", "%%"]);
            for p in &parts {
                let _ = split_rule(p, &["@"]);
            }
            // 解析
            let Ok(rule) = Rule::parse(line, "关键字", 2) else {
                continue;
            };
            // 求值：只用一份很小的文档，重点是别炸
            let (alts, _) = split_rule(&rule.text, &["&&", "||", "%%"]);
            for alt in &alts {
                let alt = alt.trim().trim_start_matches(['@', ' ']);
                if alt.is_empty() {
                    continue;
                }
                if rule.css {
                    let (sel, acc) = match alt.rfind('@') {
                        Some(i) => (&alt[..i], &alt[i + 1..]),
                        None => (alt, "text"),
                    };
                    if let Ok(els) = doc.select(None, sel) {
                        let ids: Vec<Option<usize>> = els.into_iter().map(Some).collect();
                        let _ = doc.accessor(&ids, acc);
                    }
                } else {
                    let (steps, _) = split_rule(alt, &["@"]);
                    let (last, rest) = steps.split_last().expect("至少一段");
                    let mut cur: Vec<Option<usize>> = vec![None];
                    for st in rest {
                        match doc.step(&cur, st) {
                            Ok(v) => cur = v,
                            Err(_) => break,
                        }
                    }
                    let _ = doc.accessor(&cur, last.trim());
                    let _ = rule.apply_replace("样例文本 123");
                }
            }
        }
        assert!(n > 10_000, "语料只有 {n} 行，是不是没生成好");
    }

    #[test]
    fn ascii_prefix_check_survives_chinese_rules() {
        for rule in [
            "text.下一页@href",
            "下一页",
            "中文规则@中文",
            "下一页@text@js:1",
            "@css:.下一页@text",
            "class.下一章@tag.a@href",
        ] {
            let _ = Rule::parse(rule, "键", 1);
        }
        assert_eq!(Rule::parse("下一页", "k", 1).unwrap().text, "下一页");
        // 中文前面加前缀也要认
        assert!(matches!(
            Rule::parse("@css:.下一页@text", "k", 1),
            Ok(r) if r.css
        ));
    }

    #[test]
    fn xpath_is_refused_rather_than_guessed() {
        // XPath 现在是解析期收下、求值期跑
        assert_eq!(Rule::parse("//div", "k", 1).unwrap().mode, Mode::XPath);
        assert_eq!(
            Rule::parse("@xpath://div", "k", 1).unwrap().mode,
            Mode::XPath
        );
        assert_eq!(
            Rule::parse("/html/body", "k", 1).unwrap().text,
            "/html/body"
        );
        assert_eq!(Rule::parse("//div", "k", 1).unwrap().text, "//div");
        // `@webjs:` 要真浏览器，永远拒
        assert!(matches!(
            Rule::parse("a@webjs:window.x", "k", 1),
            Err(Error::Unsupported(_))
        ));
    }

    /// `@js:` 现在是**解析期收下、求值期执行**，不再是解析期报错。
    #[test]
    fn js_tails_are_captured_for_evaluation() {
        // `@js:` 之后一律是脚本，连 `##` 也算脚本的一部分
        let r = Rule::parse("class.a@text@js:result.replace(/x/g,'y')", "k", 1).unwrap();
        assert_eq!(r.text, "class.a@text");
        assert_eq!(r.js.as_deref(), Some("result.replace(/x/g,'y')"));
        assert!(r.replace.is_none(), "`@js:` 之后的 ## 属于脚本，不是替换");

        let r = Rule::parse("<js>1+1</js>", "k", 1).unwrap();
        assert_eq!(r.js.as_deref(), Some("1+1"));

        // 没有 JS 段时不能凭空多出 js
        assert!(Rule::parse("class.a@text", "k", 1).unwrap().js.is_none());

        // `{{ }}` 里算不出来的表达式：没有 JS 环境时仍然要拒绝（不能静默取空）
        assert!(matches!(
            Rule::parse("a{{java.ajax('x')}}", "k", 1),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn replacement_segments_follow_legado() {
        let r = Rule::parse("a##\\s+", "k", 1).unwrap();
        assert_eq!(r.text, "a");
        let rep = r.replace.unwrap();
        assert_eq!(rep.pattern, "\\s+");
        assert_eq!(rep.with, "", "只有两段时替换成空串");
        assert!(!rep.first);

        // 三段：替换所有
        let r = Rule::parse("a##x##y", "k", 1).unwrap();
        assert_eq!(r.apply_replace("x1x2"), "y1y2");
        // 四段：只替第一个。注意 legado 返回的是**第一个匹配所在的片段**（替换后），
        // 不是整串只替一处 —— 这是个容易想当然的地方，所以照着它的行为写死。
        let r = Rule::parse("a##x##y###", "k", 1).unwrap();
        assert!(r.replace.as_ref().unwrap().first);
        assert_eq!(r.apply_replace("pre x1 mid x2"), "y");
        assert_eq!(r.apply_replace("没有匹配"), "");
        // 匹配整段时才看得出「只替第一个」
        let r = Rule::parse(r"a##x\d##y###", "k", 1).unwrap();
        assert_eq!(r.apply_replace("pre x1 mid x2"), "y");
        // 尾部空段被丢掉 → 还是「替换所有」
        let r = Rule::parse("a##x##y##", "k", 1).unwrap();
        assert!(!r.replace.as_ref().unwrap().first);
        assert_eq!(r.apply_replace("x1x2"), "y1y2");
        // 真书源「盗墓笔记」里就有这条 `text##(|)`。它**什么也不做**：
        // Java 把 `(|)` 当成「空或空」，替换空匹配等于原样返回。
        // 这里照抄这个行为（Rust 的 regex 不认 `(|)`，走字面量回落，结果一样）。
        let r = Rule::parse(r"text##(|)", "k", 1).unwrap();
        assert_eq!(r.apply_replace("盗墓笔记(全集)"), "盗墓笔记(全集)");
        // 正则写坏走字面量回落，不能 panic
        let r = Rule::parse("a##[##y", "k", 1).unwrap();
        assert_eq!(r.apply_replace("a[b"), "ayb");
        // 真书源「顶点小说」里那条：把站点广告语去掉
        let r = Rule::parse("id.content@all##请记住本书.*$", "k", 1).unwrap();
        assert_eq!(
            r.apply_replace("正文正文请记住本书首发域名：ddyueshu.com"),
            "正文正文"
        );
    }

    #[test]
    fn brace_expressions() {
        let e = |s: &str| eval_expr(s, "斗破", 3);
        assert_eq!(e("page").as_deref(), Some("3"));
        assert_eq!(e("key").as_deref(), Some("斗破"));
        assert_eq!(e("(page-1)*50").as_deref(), Some("100"));
        assert_eq!(e("page-1").as_deref(), Some("2"));
        assert_eq!(e("page -1").as_deref(), Some("2"));
        assert_eq!(e("page*-1+11").as_deref(), Some("8"));
        assert_eq!(e("42-page").as_deref(), Some("39"));
        assert_eq!(e("-page").as_deref(), Some("-3"));
        assert_eq!(e("page-0").as_deref(), Some("3"));
        // 除法和取模
        assert_eq!(e("page/2").as_deref(), Some("1.5"));
        assert_eq!(e("page%2").as_deref(), Some("1"));
        // 时间戳（真书源里的缓存破除写法）
        assert!(e("Math.round(new Date()/1000)").unwrap().len() > 9);
        assert!(e("Date.now()").unwrap().len() == 13);
        // 不认识的表达式 = 不认识，不能当 0
        assert_eq!(e("java.ajax('x')"), None);
        assert_eq!(e("source.getKey()"), None);
        assert_eq!(e("a"), None);
        assert_eq!(e("1/0"), None);
        // 曾经在这里死循环：`$` 开头的标识符扫不出字符，i 原地不动。
        // 真书源里 `{{$.chapter_id}}` 是常见写法（JSON 路径内嵌）。
        assert_eq!(e("$.chapter_id"), None);
        assert_eq!(e("$"), None);
        assert_eq!(e("_"), None);
        assert_eq!(e("$."), None);
        assert_eq!(e("page+"), None);
        assert_eq!(e("(page"), None);
    }

    #[test]
    fn braces_are_substituted_inside_rules() {
        let r = Rule::parse("http://x/s?q={{key}}&p={{page-1}}", "斗破", 2).unwrap();
        assert_eq!(r.text, "http://x/s?q=斗破&p=1");
        // 没有配对的 }} 就当普通文本
        let r = Rule::parse("a{{b", "k", 1).unwrap();
        assert_eq!(r.text, "a{{b");
    }
}
