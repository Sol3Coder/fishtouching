//! JSON 路径 —— Jayway JsonPath 的一个子集。
//!
//! legado 的 JSON 规则走的是 `JsonPath.read()`，所以语义要贴着它：
//!
//! * **定路径**（只有 `.key` / `[n]`）指向**一个**值；找不到就是没有（legado
//!   把 `PathNotFoundException` 吞掉当空串），**不做隐式数组展开**。
//!   要展开就明写 `[*]` —— 真书源里确实都这么写。
//! * **不定路径**（`..` / `[*]` / `?()` / 切片）永远返回**列表**。
//!
//! 支持的范围是量出来的，不是拍脑袋定的：拿 3907 个真书源统计，JSON 路径
//! 7756 条，形状分布是 `$.K` 4359、`$.K.K` 729、`$..K` 307、`$..K[*]` 248、
//! `$.K[*]` 200、`$.K.K[*]` 120、`$.[*]` 44、`$.K.*` 39、`$.K[N].K` 32；
//! 过滤器只有 59 条，全是 `[?(@.key)]` / `==` / `!=`；切片只有 19 条，
//! 全是 `[1:]` / `[:10]` 这种。所以：
//!
//! 支持 `$`、`.key`、`['key']`、`[n]`、`[*]`、`.*`、`..key`、`..*`、
//! `..[?()]`、`[a:b]`、`[?(key)]`、`[?(key==v)]`、`[?(key!=v)]`。
//!
//! 其余（多键并集 `[0,1]`、脚本 `[(@...)]`、`length()` 之类）**报错**，
//! 不猜 —— 猜错的表现是静默取到空内容或者别的东西，比报错难查得多。

use serde_json::Value;

use crate::{Error, R};

/// 路径里的一步。
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// `.key` / `['key']`
    Key(String),
    /// `[n]`。负数是「从末尾数」。
    Index(i64),
    /// `[*]` / `.*` —— 数组的每个元素，或对象的每个值。
    Wildcard,
    /// `..key` —— 递归找所有同名的键。
    DeepKey(String),
    /// `..*` / `..` —— 所有后代。
    DeepAll,
    /// `..[?(...)]` —— 递归 + 过滤（`$..[?(@.novelName)]` 这种写法有 30 条）。
    DeepFilter(Filter),
    /// `[a:b]` / `[a:]` / `[:b]`
    Slice(Option<i64>, Option<i64>),
    /// `[?(...)]`
    Filter(Filter),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub key: String,
    pub op: Op,
    /// `Op::Exists` 时是 `None`。
    pub value: Option<Lit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `[?(@.key)]` —— 这个键存在
    Exists,
    Eq,
    Ne,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
}

/// 一条解析好的路径。
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    pub steps: Vec<Step>,
}

impl Path {
    /// 是不是「不定路径」——含 `..` / `[*]` / `?()` / 切片就是。
    ///
    /// 这个区分是有意义的：不定路径永远返回列表，定路径指向一个值。
    /// legado 的元素提取（`getList`）就依赖它。
    pub fn is_indefinite(&self) -> bool {
        self.steps
            .iter()
            .any(|s| !matches!(s, Step::Key(_) | Step::Index(_)))
    }

    /// 求值。返回所有匹配到的值（定路径最多一个，找不到就是空）。
    pub fn eval(&self, root: &Value) -> Vec<Value> {
        let mut cur = vec![root.clone()];
        for step in &self.steps {
            let mut next = Vec::new();
            for v in &cur {
                apply(step, v, &mut next);
            }
            cur = next;
            if cur.is_empty() {
                break;
            }
        }
        cur
    }

    /// 求值，但要**元素列表**（`bookList` / `chapterList` 用）。
    ///
    /// legado 这里是 `read<ArrayList>`：
    /// * 不定路径 → 匹配结果就是列表；
    /// * 定路径指着**数组** → 那个数组本身就是列表（`$.data` 用了 49 次）；
    /// * 定路径指着标量 → 转 `ArrayList` 会失败 → **空**。
    pub fn eval_elements(&self, root: &Value) -> Vec<Value> {
        let vals = self.eval(root);
        if self.is_indefinite() {
            return vals;
        }
        match vals.into_iter().next() {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        }
    }
}

fn apply(step: &Step, v: &Value, out: &mut Vec<Value>) {
    match step {
        Step::Key(k) => {
            if let Value::Object(m) = v {
                if let Some(got) = m.get(k) {
                    out.push(got.clone());
                }
            }
            // 定路径遇到数组**不展开** —— Jayway 就是这样，真书源也都明写 `[*]`
        }
        Step::Index(i) => {
            let Value::Array(a) = v else {
                return;
            };
            let n = a.len() as i64;
            let idx = if *i < 0 { n + *i } else { *i };
            if idx >= 0 && idx < n {
                out.push(a[idx as usize].clone());
            }
        }
        Step::Wildcard => match v {
            Value::Array(a) => out.extend(a.iter().cloned()),
            // `.*` 在对象上是「所有值」
            Value::Object(m) => out.extend(m.values().cloned()),
            _ => {}
        },
        Step::Slice(a, b) => {
            let Value::Array(arr) = v else {
                return;
            };
            let n = arr.len() as i64;
            let norm = |x: i64| if x < 0 { (n + x).max(0) } else { x.min(n) };
            let start = a.map(norm).unwrap_or(0);
            let end = b.map(norm).unwrap_or(n);
            let (start, end) = (start.max(0), end.max(0));
            if start < end {
                out.extend(arr[start as usize..end as usize].iter().cloned());
            }
        }
        Step::DeepKey(k) => deep_key(v, k, out),
        Step::DeepAll => deep_all(v, out),
        Step::DeepFilter(f) => deep_filter(v, f, out),
        Step::Filter(f) => filter(v, f, out),
    }
}

/// `..key`：递归找所有这个名字的键。
///
/// 顺序是**深度优先、文档顺序** —— 第一个匹配很重要，很多规则靠「取第一个」
/// （`$..content` 45 次）。
fn deep_key(v: &Value, key: &str, out: &mut Vec<Value>) {
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                if k == key {
                    out.push(val.clone());
                }
                deep_key(val, key, out);
            }
        }
        Value::Array(a) => {
            for x in a {
                deep_key(x, key, out);
            }
        }
        _ => {}
    }
}

/// `..*`：所有后代（不含自己）。
fn deep_all(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Object(m) => {
            for val in m.values() {
                out.push(val.clone());
                deep_all(val, out);
            }
        }
        Value::Array(a) => {
            for x in a {
                out.push(x.clone());
                deep_all(x, out);
            }
        }
        _ => {}
    }
}

/// `..[?(...)]`：递归，留下**满足条件**的节点。
///
/// 实现成一步而不是「DeepAll 再 Filter」：后者会把数组和它的元素都过一遍，
/// 结果里出现重复。这一步直接按「这个对象/这个元素本身满不满足」判。
fn deep_filter(v: &Value, f: &Filter, out: &mut Vec<Value>) {
    match v {
        Value::Object(_) => {
            if matches(v, f) {
                out.push(v.clone());
            }
            if let Value::Object(m) = v {
                for val in m.values() {
                    deep_filter(val, f, out);
                }
            }
        }
        Value::Array(a) => {
            // 只往下走，**不在这里判** —— 判定交给 Object 分支。
            // 在这里也判一次的话，数组里的对象会被收两遍。
            for x in a {
                deep_filter(x, f, out);
            }
        }
        _ => {}
    }
}

/// `[?(...)]`：数组里满足条件的元素；对象上就是判它自己。
///
/// 对象那种是刻意的 —— `$.[?(@.alias_id)]._id` 这种写法在真书源里出现过，
/// 意思是「根对象有 alias_id 才要」。
fn filter(v: &Value, f: &Filter, out: &mut Vec<Value>) {
    match v {
        Value::Array(a) => {
            for x in a {
                if matches(x, f) {
                    out.push(x.clone());
                }
            }
        }
        Value::Object(_) => {
            if matches(v, f) {
                out.push(v.clone());
            }
        }
        _ => {}
    }
}

/// 一个值满不满足过滤条件。
fn matches(v: &Value, f: &Filter) -> bool {
    let got = match v {
        Value::Object(m) => m.get(&f.key),
        _ => None,
    };
    match f.op {
        // `[?(@.key)]`：键存在，且值不是 null
        Op::Exists => got.is_some_and(|x| !x.is_null()),
        Op::Eq | Op::Ne => {
            let eq = match (got, &f.value) {
                (None, _) => false,
                (Some(a), Some(b)) => lit_eq(a, b),
                (Some(_), None) => false,
            };
            if f.op == Op::Eq {
                eq
            } else {
                !eq
            }
        }
    }
}

/// 值相等。数字按数值比（`3` 和 `3.0` 是一回事），其余按文本比。
fn lit_eq(v: &Value, lit: &Lit) -> bool {
    match (v, lit) {
        (Value::String(s), Lit::Str(t)) => s == t,
        (Value::Bool(b), Lit::Bool(c)) => b == c,
        (Value::Null, Lit::Null) => true,
        (Value::Number(n), Lit::Num(x)) => n.as_f64() == Some(*x),
        // 类型不同时退一步按文本比：有些源把数字写成字符串
        (a, b) => value_to_string(a) == lit_to_string(b),
    }
}

pub fn lit_to_string(l: &Lit) -> String {
    match l {
        Lit::Str(s) => s.clone(),
        Lit::Num(n) => {
            if n.fract() == 0.0 && n.is_finite() && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                n.to_string()
            }
        }
        Lit::Bool(b) => b.to_string(),
        Lit::Null => String::new(),
    }
}

/// 一个 JSON 值变成字符串。
///
/// 照抄 legado 的 `toString()`：标量给字面量，对象/数组给 JSON 文本。
pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

// ── 解析 ──────────────────────────────────────────────────────────────

/// 解析一条 JSON 路径。不认识的东西一律报错。
pub fn parse(path: &str) -> R<Path> {
    let s = path.trim();
    let chars: Vec<char> = s.chars().collect();
    let mut i;

    // `$` 开头的标准写法。`@` 是过滤器里的当前节点（`@.key`）。
    //
    // **开头不写 `$` 也认**：正文是 JSON 时 legado 有个 `isJSON` 标志，
    // 所有规则都当 JSON 路径解，于是真书源里有 `chapterList: "data"`、
    // `chapterName: "chapterName"` 这种裸键名写法。
    if chars.first() == Some(&'$') || chars.first() == Some(&'@') {
        i = 1;
    } else {
        i = 0;
    }

    let mut steps = Vec::new();
    while i < chars.len() {
        match chars[i] {
            // `..key` / `..*` / `..[?(...)]`
            '.' if chars.get(i + 1) == Some(&'.') => {
                i += 2;
                match chars.get(i) {
                    Some('*') => {
                        i += 1;
                        // `..*[?()]` 少见，但 `..[*]` 要认
                        if chars.get(i) == Some(&'[') && chars.get(i + 1) == Some(&'*') {
                            i += 2;
                            if chars.get(i) == Some(&']') {
                                i += 1;
                            }
                        }
                        steps.push(Step::DeepAll);
                    }
                    Some('[') => {
                        // `..[?(...)]` 或 `..[0]`
                        let (step, ni) = parse_bracket(&chars, i)?;
                        i = ni;
                        steps.push(match step {
                            Step::Filter(f) => Step::DeepFilter(f),
                            // `..[n]` 是「所有后代里的第 n 个」——语义含混，拒掉
                            other => {
                                return Err(Error::Unsupported(format!(
                                    "JSON 路径里的 `..[{other:?}]`"
                                )))
                            }
                        });
                    }
                    Some('.') | None => {
                        // `..` 后面直接结束或又是点：当成「所有后代」
                        steps.push(Step::DeepAll);
                    }
                    _ => {
                        let name = read_name(&chars, &mut i);
                        if name.is_empty() {
                            return Err(Error::Unsupported(format!("JSON 路径里的 `..`：{path}")));
                        }
                        steps.push(Step::DeepKey(name));
                    }
                }
            }
            // `.[*]` —— Jayway 认这种写法（真书源里 `$.[*]` 44 次）
            '.' => {
                i += 1;
                if chars.get(i) == Some(&'[') {
                    continue;
                }
                if chars.get(i) == Some(&'*') && chars.get(i + 1) != Some(&'*') {
                    i += 1;
                    steps.push(Step::Wildcard);
                    continue;
                }
                let name = read_name(&chars, &mut i);
                if name.is_empty() {
                    return Err(Error::Unsupported(format!("JSON 路径里的空键名：{path}")));
                }
                steps.push(Step::Key(name));
            }
            '[' => {
                let (step, ni) = parse_bracket(&chars, i)?;
                i = ni;
                steps.push(step);
            }
            // 有些源写成 `$data`（没有点），宽容一点
            c if c.is_alphanumeric() || c == '_' => {
                let name = read_name(&chars, &mut i);
                steps.push(Step::Key(name));
            }
            c => {
                return Err(Error::Unsupported(format!(
                    "JSON 路径里的字符 `{c}`：{path}"
                )))
            }
        }
    }
    Ok(Path { steps })
}

/// 读一个键名：直到 `.` / `[` / 末尾。带引号的先去掉引号。
fn read_name(chars: &[char], i: &mut usize) -> String {
    if let Some(&q) = chars.get(*i) {
        if q == '\'' || q == '"' {
            *i += 1;
            let mut out = String::new();
            while let Some(&c) = chars.get(*i) {
                if c == q {
                    *i += 1;
                    break;
                }
                out.push(c);
                *i += 1;
            }
            return out;
        }
    }
    let mut out = String::new();
    while let Some(&c) = chars.get(*i) {
        if c == '.' || c == '[' || c == ']' {
            break;
        }
        out.push(c);
        *i += 1;
    }
    out.trim().to_string()
}

/// 解析一个 `[...]`。返回这一步和新的下标。
fn parse_bracket(chars: &[char], mut i: usize) -> R<(Step, usize)> {
    debug_assert_eq!(chars.get(i), Some(&'['));
    i += 1; // 跳过 '['
            // 先拿到括号内部（考虑引号）
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
            '[' | '(' => depth += 1,
            ')' => depth -= 1,
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
        return Err(Error::Unsupported("JSON 路径里的 `[` 没闭合".into()));
    }
    let inner: String = chars[start..i].iter().collect();
    let inner = inner.trim().to_string();
    let next = i + 1; // 跳过 ']'

    // `[*]`
    if inner == "*" {
        return Ok((Step::Wildcard, next));
    }
    // `[?(...)]`
    if let Some(rest) = inner.strip_prefix("?(") {
        let expr = rest.strip_suffix(')').unwrap_or(rest);
        return Ok((Step::Filter(parse_filter(expr)?), next));
    }
    // `[1:2]` / `[1:]` / `[:2]`
    if inner.contains(':') {
        let (a, b) = inner.split_once(':').expect("上面刚判过有冒号");
        let num = |s: &str| -> R<Option<i64>> {
            let s = s.trim();
            if s.is_empty() {
                return Ok(None);
            }
            s.parse::<i64>()
                .map(Some)
                .map_err(|_| Error::Unsupported(format!("JSON 切片里的 `{s}`")))
        };
        return Ok((Step::Slice(num(a)?, num(b)?), next));
    }
    // `['key']` —— 真书源里见过 2 次
    if inner.starts_with('\'') || inner.starts_with('"') {
        let k = inner.trim_matches(['\'', '"']).to_string();
        return Ok((Step::Key(k), next));
    }
    // `[n]`
    if let Ok(n) = inner.parse::<i64>() {
        return Ok((Step::Index(n), next));
    }
    // 多键并集 `[0,1]` / `['a','b']`：量出来真书源里没有，拒掉而不是猜
    Err(Error::Unsupported(format!("JSON 下标 `[{inner}]`")))
}

/// 解析 `@.key` / `@.key==v` / `@.key!=v`。
///
/// 量出来真书源只有这三种：存在、等于、不等于。
fn parse_filter(expr: &str) -> R<Filter> {
    let e = expr.trim();
    let Some(rest) = e.strip_prefix('@') else {
        return Err(Error::Unsupported(format!(
            "JSON 过滤器 `{e}` 应当以 @ 开头"
        )));
    };
    // `@.key` / `@['key']`
    let body = rest.strip_prefix('.').unwrap_or(rest);
    let (key, tail) = if let Some(b) = body.strip_prefix('[') {
        match b.find(']') {
            Some(end) => (
                b[..end].trim_matches(['\'', '"']).to_string(),
                b[end + 1..].trim().to_string(),
            ),
            None => return Err(Error::Unsupported(format!("JSON 过滤器 `{e}` 里缺 `]`"))),
        }
    } else {
        let end = body
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '$'))
            .unwrap_or(body.len());
        (body[..end].to_string(), body[end..].trim().to_string())
    };
    if key.is_empty() {
        return Err(Error::Unsupported(format!("JSON 过滤器 `{e}` 里没有键名")));
    }
    if tail.is_empty() {
        return Ok(Filter {
            key,
            op: Op::Exists,
            value: None,
        });
    }
    let (op, val) = if let Some(v) = tail.strip_prefix("==") {
        (Op::Eq, v)
    } else if let Some(v) = tail.strip_prefix("!=") {
        (Op::Ne, v)
    } else if let Some(v) = tail.strip_prefix("<>") {
        (Op::Ne, v)
    } else {
        return Err(Error::Unsupported(format!("JSON 过滤器运算符 `{tail}`")));
    };
    Ok(Filter {
        key,
        op,
        value: Some(parse_lit(val.trim())?),
    })
}

/// 解析一个过滤器的字面量。
///
/// **严格**：`'x' && @.c` 这种多出来的东西必须报错。宽松处理的下场是
/// 整段被当成一个字符串字面量，于是过滤器永远不匹配 —— 静默取空，
/// 比报错难查得多。
fn parse_lit(s: &str) -> R<Lit> {
    let t = s.trim();
    if t.is_empty() {
        return Err(Error::Unsupported("JSON 过滤器里少了比较值".into()));
    }
    let quote = t.chars().next().expect("上面刚判过非空");
    if quote == '\'' || quote == '"' {
        let rest = &t[quote.len_utf8()..];
        return match rest.find(quote) {
            Some(i) if i == rest.len() - 1 => Ok(Lit::Str(rest[..i].to_string())),
            _ => Err(Error::Unsupported(format!(
                "JSON 过滤器里的字符串字面量 `{t}`（引号没配好）"
            ))),
        };
    }
    match t {
        "true" => Ok(Lit::Bool(true)),
        "false" => Ok(Lit::Bool(false)),
        "null" => Ok(Lit::Null),
        _ => t.parse::<f64>().map(Lit::Num).map_err(|_| {
            Error::Unsupported(format!(
                "JSON 过滤器里的值 `{t}`（字符串要加引号，这里只认数字/true/false/null）"
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(path: &str, doc: &Value) -> Vec<String> {
        let p = parse(path).unwrap_or_else(|e| panic!("解析 {path} 失败：{e}"));
        p.eval(doc).iter().map(value_to_string).collect()
    }

    fn els(path: &str, doc: &Value) -> Vec<String> {
        let p = parse(path).unwrap();
        p.eval_elements(doc).iter().map(value_to_string).collect()
    }

    fn doc() -> Value {
        json!({
            "code": 200,
            "data": {
                "list": [
                    {"name": "斗破苍穹", "author": "天蚕土豆", "format": 3},
                    {"name": "武动乾坤", "author": "天蚕土豆", "format": 1}
                ]
            },
            "search": [{"novelName": "A"}, {"novelName": "B"}],
            "title": "顶层"
        })
    }

    #[test]
    fn plain_keys() {
        assert_eq!(ev("$.code", &doc()), ["200"]);
        assert_eq!(ev("$.title", &doc()), ["顶层"]);
        assert!(!ev("$.data.list", &doc())[0].is_empty(), "数组给 JSON 文本");
        // 不存在的键 = 没有结果（legado 把 PathNotFoundException 吞掉）
        assert!(ev("$.nope", &doc()).is_empty());
        assert!(ev("$.data.nope", &doc()).is_empty());
    }

    #[test]
    fn a_definite_path_does_not_implicitly_spread_over_an_array() {
        // Jayway 的定路径不展开数组；真书源也都明写 [*]
        assert!(
            ev("$.data.list.name", &doc()).is_empty(),
            "定路径遇到数组不该自己展开"
        );
        assert_eq!(ev("$.data.list[*].name", &doc()), ["斗破苍穹", "武动乾坤"]);
    }

    #[test]
    fn wildcards_and_indexes() {
        assert_eq!(ev("$.data.list[0].name", &doc()), ["斗破苍穹"]);
        assert_eq!(ev("$.data.list[-1].name", &doc()), ["武动乾坤"]);
        assert!(ev("$.data.list[9].name", &doc()).is_empty());
        assert_eq!(
            ev("$.data.list[*].author", &doc()),
            ["天蚕土豆", "天蚕土豆"]
        );
        // `.*` 在对象上是所有值
        assert_eq!(els("$.data.*", &doc()).len(), 1);
    }

    #[test]
    fn deep_descent() {
        assert_eq!(ev("$..title", &doc()), ["顶层"]);
        assert_eq!(ev("$..novelName", &doc()), ["A", "B"]);
        // `$..search[*]` 是最常见的 bookList 写法（101 次）
        assert_eq!(els("$..search[*]", &doc()).len(), 2);
        assert_eq!(ev("$..author", &doc()), ["天蚕土豆", "天蚕土豆"]);
        // 找不到就是空
        assert!(ev("$..nope", &doc()).is_empty());
    }

    #[test]
    fn slices() {
        assert_eq!(ev("$.data.list[0:1].name", &doc()), ["斗破苍穹"]);
        assert_eq!(ev("$.data.list[:1].name", &doc()), ["斗破苍穹"]);
        assert_eq!(ev("$.data.list[1:].name", &doc()), ["武动乾坤"]);
        assert_eq!(ev("$.data.list[0:9].name", &doc()).len(), 2, "越界要夹住");
        assert_eq!(ev("$.data.list[-1:].name", &doc()), ["武动乾坤"]);
    }

    #[test]
    fn filters() {
        // 等于（数字）
        assert_eq!(ev("$.data.list[?(@.format==3)].name", &doc()), ["斗破苍穹"]);
        // 不等于
        assert_eq!(ev("$.data.list[?(@.format!=3)].name", &doc()), ["武动乾坤"]);
        // 存在
        assert_eq!(els("$.search[?(@.novelName)]", &doc()).len(), 2);
        // 字符串相等
        let d = json!({"a": [{"s": "xbiquge", "id": 1}, {"s": "other", "id": 2}]});
        assert_eq!(ev("$.a[?(@.s=='xbiquge')].id", &d), ["1"]);
        assert_eq!(ev("$.a[?(@.s!='xbiquge')].id", &d), ["2"]);
    }

    /// `$..[?(@.novelName)]`：递归 + 过滤，真书源里 30 次。
    #[test]
    fn deep_filter_does_not_double_count() {
        let d = json!({"a": {"novelName": "X", "b": {"novelName": "Y"}}});
        let got = els("$..[?(@.novelName)]", &d);
        assert_eq!(got.len(), 2, "每个只算一次，实际 {got:?}");
        // 数组元素也认
        let d2 = json!({"list": [{"novelName": "P"}, {"other": 1}]});
        assert_eq!(els("$..[?(@.novelName)]", &d2).len(), 1);
    }

    #[test]
    fn the_filter_on_the_root_object_is_allowed() {
        // `$.[?(@.alias_id)]._id` —— 真书源里出现过，意思是「根对象有这个键才要」
        let d = json!({"_id": "abc", "alias_id": "x"});
        assert_eq!(ev("$.[?(@.alias_id)]._id", &d), ["abc"]);
        let d2 = json!({"_id": "abc"});
        assert!(ev("$.[?(@.alias_id)]._id", &d2).is_empty());
    }

    #[test]
    fn element_extraction_matches_legado() {
        // 不定路径 → 结果就是列表
        assert_eq!(els("$.data.list[*]", &doc()).len(), 2);
        // 定路径指着数组 → 数组本身就是列表（`$.data` 用了 49 次）
        assert_eq!(els("$.data.list", &doc()).len(), 2);
        // 定路径指着标量 → 转 List 会失败，legado 给空
        assert!(els("$.title", &doc()).is_empty());
    }

    /// 正文是 JSON 时不写 `$` 也认（legado 的 `isJSON` 标志）。
    #[test]
    fn a_bare_key_is_allowed() {
        let d = json!({"data": {"name": "X"}});
        assert_eq!(ev("data.name", &d), ["X"]);
        assert_eq!(ev("data", &d), ["{\"name\":\"X\"}"]);
    }

    #[test]
    fn quoted_keys_and_lenient_forms() {
        assert_eq!(ev("$['title']", &doc()), ["顶层"]);
        assert_eq!(ev("$.\"title\"", &doc()), ["顶层"]);
        // `$.[*]` —— 44 次
        let arr = json!({"a": 1, "b": 2});
        assert_eq!(parse("$.[*]").unwrap().eval(&arr).len(), 2);
        // `$data`（没有点）也认
        assert_eq!(ev("$code", &doc()), ["200"]);
    }

    #[test]
    fn indefinite_detection() {
        assert!(!parse("$.a.b[0]").unwrap().is_indefinite());
        assert!(parse("$.a[*]").unwrap().is_indefinite());
        assert!(parse("$..a").unwrap().is_indefinite());
        assert!(parse("$.a[0:1]").unwrap().is_indefinite());
        assert!(parse("$.a[?(@.b)]").unwrap().is_indefinite());
    }

    #[test]
    fn unsupported_syntax_errors_instead_of_guessing() {
        // 这些量出来真书源里没有 —— 报错，不要猜
        for bad in [
            "$.a[0,1]",          // 多下标并集
            "$.a[(@.length-1)]", // 脚本下标
            "$.a[?(@.b>1)]",     // 比较运算符
            "$.a[?(@.b=='x' && @.c)]",
        ] {
            assert!(
                parse(bad).is_err(),
                "{bad} 应当报错，而不是给出一个猜出来的结果"
            );
        }
    }

    #[test]
    fn scalar_types_round_trip() {
        let d = json!({"s": "文本", "n": 3, "f": 1.5, "b": true, "z": null, "a": [1]});
        assert_eq!(ev("$.s", &d), ["文本"]);
        assert_eq!(ev("$.n", &d), ["3"]);
        assert_eq!(ev("$.f", &d), ["1.5"]);
        assert_eq!(ev("$.b", &d), ["true"]);
        assert_eq!(ev("$.z", &d), [""], "null 给空串");
        assert_eq!(ev("$.a", &d), ["[1]"], "数组给 JSON 文本");
    }

    #[test]
    fn number_and_string_compare_equal_when_types_differ() {
        // 有些源把数字写成字符串，比较时退一步按文本比
        let d = json!([{"v": "3"}, {"v": 4}]);
        assert_eq!(els("$[?(@.v==3)]", &d).len(), 1);
        assert_eq!(els("$[?(@.v==4)]", &d).len(), 1);
    }
}
