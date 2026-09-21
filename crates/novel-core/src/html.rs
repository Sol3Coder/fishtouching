//! HTML 选择器引擎 —— 对齐 JSoup，而不是 CSS 标准。
//!
//! 为什么不用 `tl` 自带的 `query_selector`：
//! * 它**不支持组合符**（`div a`、`a > b` 都返回 0 个），而 legado 的默认方言和
//!   `@css:` 规则全是组合符；
//! * 真书源大量使用 JSoup 的私有扩展（`:eq(n)`、`:contains(t)`、`:matches(re)`、
//!   `:has(sel)`、`:lt(n)`），任何标准 CSS 库都不会有；
//! * legado 的默认方言（`class.x@tag.p.0@text`）本身就是「一步步 select」的语义，
//!   不是一条 CSS 选择器，必须有逐级求值的能力。
//!
//! 所以只借 `tl` 的**解析树**，匹配逻辑自己写。

use std::collections::HashMap;

use tl::{Node, NodeHandle, ParserOptions, VDom};

use crate::{Error, R};

/// 解析好的 HTML。借用源字符串，所以调用方要保证 `src` 活得比它久。
pub struct Html<'a> {
    dom: VDom<'a>,
    /// 文档序的所有元素（只有元素，没有文本/注释）
    ids: Vec<NodeHandle>,
    /// 每个元素在 `ids` 里的父元素；顶层是 `None`
    parent: Vec<Option<usize>>,
    /// 在父元素的**元素**孩子里排第几（0 开始）
    nth: Vec<usize>,
    /// 在父元素的**同名**孩子里排第几（0 开始）
    nth_of_type: Vec<usize>,
}

impl<'a> Html<'a> {
    pub fn parse(src: &'a str) -> Self {
        // tl 只在流完全为空时报错，HTML 里基本碰不到；真碰上了就当空文档。
        let dom = tl::parse(src, ParserOptions::default())
            .or_else(|_| tl::parse("<html></html>", ParserOptions::default()))
            .expect("空文档总能解析");
        let mut me = Self {
            dom,
            ids: Vec::new(),
            parent: Vec::new(),
            nth: Vec::new(),
            nth_of_type: Vec::new(),
        };
        // 文档序深度优先。`tl` 没有父指针，所以父/序号关系只能自己走一遍建出来。
        //
        // `tl` 会在最外面套一个**名字为空**的节点代表整个文档。JSoup 的 Document
        // 不是 Element，不会出现在 select 结果里，所以这里把它拆掉，
        // 让它的孩子当顶层元素。
        let mut roots = Vec::new();
        for r in me.dom.children().to_vec() {
            // 顶层可能混着文本/注释节点（`<html>` 前面那个换行就是），它们不是元素
            match me.node_of(r) {
                Some(Node::Tag(_)) if me.name_of(r).as_deref() == Some("") => {
                    roots.extend(me.tag_children(r))
                }
                Some(Node::Tag(_)) => roots.push(r),
                _ => {}
            }
        }
        for (n, r) in roots.into_iter().enumerate() {
            me.walk(r, None, n, 0);
        }
        me
    }

    fn walk(&mut self, h: NodeHandle, parent: Option<usize>, nth: usize, nth_of_type: usize) {
        let me = self.ids.len();
        self.ids.push(h);
        self.parent.push(parent);
        self.nth.push(nth);
        self.nth_of_type.push(nth_of_type);
        let mut same: HashMap<String, usize> = HashMap::new();
        for (elem, k) in self.tag_children(h).into_iter().enumerate() {
            let name = self
                .name_of(k)
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let t = same.entry(name).or_insert(0);
            let nth_of_type = *t;
            *t += 1;
            self.walk(k, Some(me), elem, nth_of_type);
        }
    }

    fn parser(&self) -> &tl::Parser<'a> {
        self.dom.parser()
    }

    fn node(&self, i: usize) -> Option<&Node<'a>> {
        self.ids.get(i)?.get(self.parser())
    }

    fn node_of(&self, h: NodeHandle) -> Option<&Node<'a>> {
        h.get(self.parser())
    }

    fn name_of(&self, h: NodeHandle) -> Option<String> {
        let b = h.get(self.parser())?.as_tag()?.name();
        b.try_as_utf8_str().map(str::to_string)
    }

    /// `h` 的直接孩子里的**元素**（JSoup 的 `Element.children()` 不含文本节点）。
    fn tag_children(&self, h: NodeHandle) -> Vec<NodeHandle> {
        let Some(tag) = h.get(self.parser()).and_then(|n| n.as_tag()) else {
            return Vec::new();
        };
        tag.children()
            .top()
            .iter()
            .copied()
            .filter(|c| {
                c.get(self.parser())
                    .is_some_and(|n| matches!(n, Node::Tag(_)))
            })
            .collect()
    }

    /// 顶层元素。
    /// 父元素。顶层元素是 `None`。
    pub fn parent_of(&self, i: usize) -> Option<usize> {
        self.parent.get(i).copied().flatten()
    }

    /// 属性存在吗。
    ///
    /// 和 `attr(i, name).is_empty()` 不是一回事 —— 属性可以合法地是空串
    /// （`<input value="">`），XPath 的 `[@value]` 问的是「有没有这个属性」。
    pub fn has_attr(&self, i: usize, name: &str) -> bool {
        self.css_has_attr(i, name)
    }

    /// 同一个父节点下、在 `i` **之后**的兄弟（元素）。
    pub fn siblings_after(&self, i: usize) -> Vec<usize> {
        let Some(p) = self.parent_of(i) else {
            return Vec::new();
        };
        let all = self.children(Some(p));
        match all.iter().position(|x| *x == i) {
            Some(k) => all[k + 1..].to_vec(),
            None => Vec::new(),
        }
    }

    /// 同一个父节点下、在 `i` **之前**的兄弟（元素）。
    pub fn siblings_before(&self, i: usize) -> Vec<usize> {
        let Some(p) = self.parent_of(i) else {
            return Vec::new();
        };
        let all = self.children(Some(p));
        match all.iter().position(|x| *x == i) {
            Some(k) => all[..k].to_vec(),
            None => Vec::new(),
        }
    }

    pub fn top_level(&self) -> Vec<usize> {
        (0..self.ids.len())
            .filter(|&i| self.parent[i].is_none())
            .collect()
    }

    /// 某个元素的孩子；`None` = 文档顶层。
    pub fn children(&self, cur: Option<usize>) -> Vec<usize> {
        match cur {
            None => self.top_level(),
            Some(i) => (0..self.ids.len())
                .filter(|&j| self.parent[j] == Some(i))
                .collect(),
        }
    }

    /// `cur` 子树里的所有元素，文档序，**含 `cur` 自己**（对应 JSoup 的 `select` 语义）。
    pub fn subtree(&self, cur: Option<usize>) -> Vec<usize> {
        match cur {
            None => (0..self.ids.len()).collect(),
            Some(i) => {
                let mut out = Vec::new();
                let mut stack = vec![i];
                while let Some(x) = stack.pop() {
                    out.push(x);
                    // 按文档序：孩子反着压栈
                    let kids = self.children(Some(x));
                    for &k in kids.iter().rev() {
                        stack.push(k);
                    }
                }
                // 上面是前序，但要保证是从 cur 开始的文档序
                out.sort_unstable();
                out
            }
        }
    }

    // ── 取内容 ──────────────────────────────────────────────────────────

    /// JSoup 的 `Element.text()`：所有后代文本，空白归一化后 trim。
    pub fn text(&self, i: usize) -> String {
        let raw = self
            .node(i)
            .map(|n| n.inner_text(self.parser()).into_owned())
            .unwrap_or_default();
        normalize(&raw)
    }

    /// JSoup 的 `Element.ownText()`：只算直接文本节点。
    pub fn own_text(&self, i: usize) -> String {
        let Some(tag) = self.node(i).and_then(|n| n.as_tag()) else {
            return String::new();
        };
        let mut s = String::new();
        for c in tag.children().top().iter() {
            if let Some(Node::Raw(r)) = c.get(self.parser()) {
                s.push_str(&r.as_utf8_str());
            }
        }
        normalize(&s)
    }

    /// JSoup 的 `Element.textNodes()`：直接文本节点，各自 trim，非空的用 `\n` 接起来。
    pub fn text_nodes(&self, i: usize) -> String {
        let Some(tag) = self.node(i).and_then(|n| n.as_tag()) else {
            return String::new();
        };
        let parts: Vec<String> = tag
            .children()
            .top()
            .iter()
            .filter_map(|c| match c.get(self.parser()) {
                Some(Node::Raw(r)) => Some(r.as_utf8_str().trim().to_string()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect();
        parts.join("\n")
    }

    pub fn outer_html(&self, i: usize) -> String {
        self.node(i)
            .map(|n| n.outer_html(self.parser()).into_owned())
            .unwrap_or_default()
    }

    /// 元素的 `outer_html`，但先去掉 `<script>` / `<style>`（对应 `@html`）。
    pub fn html_without_scripts(&self, els: &[usize]) -> String {
        els.iter()
            .map(|&i| strip_elements(&self.outer_html(i), &["script", "style"]))
            .collect()
    }

    /// 属性值。缺属性返回空串（JSoup 的 `attr` 也是空串）。
    pub fn attr(&self, i: usize, name: &str) -> String {
        let Some(tag) = self.node(i).and_then(|n| n.as_tag()) else {
            return String::new();
        };
        tag.attributes()
            .get(name)
            .flatten()
            .map(|b| b.as_utf8_str().into_owned())
            .unwrap_or_default()
    }

    pub fn tag_name(&self, i: usize) -> String {
        self.name_of(self.ids[i]).unwrap_or_default()
    }

    pub fn id_attr(&self, i: usize) -> String {
        self.attr(i, "id")
    }

    pub fn classes(&self, i: usize) -> Vec<String> {
        self.attr(i, "class")
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    // ── 选择器 ──────────────────────────────────────────────────────────

    /// 做一次 JSoup 风格的 `select`。
    ///
    /// `cur` 是求值起点：`None` = 整个文档，`Some(i)` = 以 `i` 为根（**含** `i` 自己）。
    pub fn select(&self, cur: Option<usize>, selector: &str) -> R<Vec<usize>> {
        let groups = parse_selector_list(selector)?;
        let cand = self.subtree(cur);
        let mut out = Vec::new();
        for i in cand {
            if groups.iter().any(|g| self.matches_complex(i, cur, g)) {
                out.push(i);
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// legado 默认方言里的一「步」：`class.foo.0`、`tag.a`、`id.x`、`text.下一页`、
    /// `children.0`，或者任何其它 CSS 选择器。
    pub fn step(&self, cur: &[Option<usize>], step: &str) -> R<Vec<Option<usize>>> {
        let step = step.trim();
        let (before, filter) = split_index(step);
        let mut out = Vec::new();
        for &c in cur {
            let mut els = match before.as_str() {
                "" => self.children(c),
                _ => {
                    let parts: Vec<&str> = before.split('.').collect();
                    match parts[0] {
                        "children" => self.children(c),
                        "class" => self.select(c, &format!(".{}", parts.get(1).unwrap_or(&"")))?,
                        "tag" => self.select(c, parts.get(1).unwrap_or(&""))?,
                        "id" => self.select(c, &format!("#{}", parts.get(1).unwrap_or(&"")))?,
                        "text" => {
                            let want = parts.get(1).unwrap_or(&"").to_lowercase();
                            self.scope(c)
                                .into_iter()
                                .filter(|&i| self.own_text(i).to_lowercase().contains(&want))
                                .collect()
                        }
                        _ => self.select(c, &before)?,
                    }
                }
            }
            .into_iter()
            .map(Some)
            .collect::<Vec<_>>();
            filter.apply(&mut els);
            out.extend(els);
        }
        out.dedup();
        Ok(out)
    }

    /// 求值范围：`None` 是整篇文档，`Some(i)` 是以 `i` 为根的子树（含 `i`）。
    fn scope(&self, cur: Option<usize>) -> Vec<usize> {
        match cur {
            None => self.subtree(None),
            Some(i) => self.subtree(Some(i)),
        }
    }

    /// legado 的取值器（`@text` / `@href` …）。
    pub fn accessor(&self, els: &[Option<usize>], last: &str) -> Vec<String> {
        let mut out = Vec::new();
        match last {
            "text" => {
                for e in els {
                    let t = match e {
                        Some(i) => self.text(*i),
                        // 文档节点整体取文本（legado 里 Document 也是 Element）
                        None => normalize(&self.whole_text()),
                    };
                    if !t.is_empty() {
                        out.push(t);
                    }
                }
            }
            "textNodes" => {
                for i in els.iter().flatten() {
                    let t = self.text_nodes(*i);
                    if !t.is_empty() {
                        out.push(t);
                    }
                }
            }
            "ownText" => {
                for i in els.iter().flatten() {
                    let t = self.own_text(*i);
                    if !t.is_empty() {
                        out.push(t);
                    }
                }
            }
            "html" => {
                let ids: Vec<usize> = els.iter().flatten().copied().collect();
                let h = self.html_without_scripts(&ids);
                if !h.is_empty() {
                    out.push(h);
                }
            }
            "all" => {
                let h: String = els
                    .iter()
                    .map(|e| match e {
                        Some(i) => self.outer_html(*i),
                        None => self.raw(),
                    })
                    .collect();
                if !h.is_empty() {
                    out.push(h);
                }
            }
            // 属性名
            _ => {
                for e in els {
                    let Some(i) = e else { continue };
                    let v = self.attr(*i, last);
                    // legado 在这里会跳过重复值
                    if v.trim().is_empty() || out.contains(&v) {
                        continue;
                    }
                    out.push(v);
                }
            }
        }
        out
    }

    /// 整篇文档的文本（不含标签）。
    fn whole_text(&self) -> String {
        self.subtree(None)
            .into_iter()
            .map(|i| self.own_text(i))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// 原始 HTML 全文。
    pub fn raw(&self) -> String {
        self.dom.outer_html()
    }

    fn matches_complex(&self, i: usize, root: Option<usize>, c: &Complex) -> bool {
        let Some((_, last)) = c.parts.last() else {
            return false;
        };
        if !self.matches_compound(i, root, last) {
            return false;
        }
        // 从右往左回溯。组合符存在**右边**那个复合选择器上，
        // 所以从 parts[k] 走到 parts[k+1] 要用 parts[k+1] 的组合符。
        let mut idx = i;
        for k in (0..c.parts.len().saturating_sub(1)).rev() {
            let comb = &c.parts[k + 1].0;
            let comp = &c.parts[k].1;
            match comb {
                Comb::Child => match self.parent[idx] {
                    Some(p) if self.in_root(p, root) && self.matches_compound(p, root, comp) => {
                        idx = p
                    }
                    _ => return false,
                },
                Comb::Adjacent => match self.prev_sibling(idx, root) {
                    Some(s) if self.matches_compound(s, root, comp) => idx = s,
                    _ => return false,
                },
                Comb::Sibling => {
                    let mut cur = self.prev_sibling(idx, root);
                    let mut hit = None;
                    while let Some(s) = cur {
                        if self.matches_compound(s, root, comp) {
                            hit = Some(s);
                            break;
                        }
                        cur = self.prev_sibling(s, root);
                    }
                    match hit {
                        Some(s) => idx = s,
                        None => return false,
                    }
                }
                Comb::Descendant => {
                    let mut cur = self.parent[idx];
                    let mut hit = None;
                    while let Some(p) = cur {
                        if !self.in_root(p, root) {
                            break;
                        }
                        if self.matches_compound(p, root, comp) {
                            hit = Some(p);
                            break;
                        }
                        cur = self.parent[p];
                    }
                    match hit {
                        Some(p) => idx = p,
                        None => return false,
                    }
                }
            }
        }
        true
    }

    /// `p` 是否在求值范围内（JSoup 的组合符走到 `root` 就停）。
    fn in_root(&self, p: usize, root: Option<usize>) -> bool {
        match root {
            None => true,
            Some(r) => p == r || self.is_descendant(p, r),
        }
    }

    fn is_descendant(&self, p: usize, r: usize) -> bool {
        let mut cur = self.parent[p];
        while let Some(x) = cur {
            if x == r {
                return true;
            }
            cur = self.parent[x];
        }
        false
    }

    fn prev_sibling(&self, i: usize, root: Option<usize>) -> Option<usize> {
        let p = self.parent[i]?;
        self.in_root(p, root).then_some(())?;
        if self.nth[i] == 0 {
            return None;
        }
        self.children(Some(p)).get(self.nth[i] - 1).copied()
    }

    fn matches_compound(&self, i: usize, root: Option<usize>, c: &Compound) -> bool {
        if let Some(t) = &c.tag {
            if !self.tag_name(i).eq_ignore_ascii_case(t) {
                return false;
            }
        }
        if let Some(id) = &c.id {
            if self.id_attr(i) != *id {
                return false;
            }
        }
        for class in &c.classes {
            if !self.classes(i).iter().any(|c| c == class) {
                return false;
            }
        }
        for (name, want) in &c.attrs {
            let got = self.attr(i, name);
            let ok = match want {
                AttrMatch::Exists => !got.is_empty() || self.has_attr(i, name),
                AttrMatch::Eq(v) => got == *v,
                AttrMatch::Prefix(v) => got.starts_with(v.as_str()),
                AttrMatch::Suffix(v) => got.ends_with(v.as_str()),
                AttrMatch::Contains(v) => got.contains(v.as_str()),
                AttrMatch::Word(v) => got.split_whitespace().any(|w| w == v),
            };
            if !ok {
                return false;
            }
        }
        c.pseudos.iter().all(|p| self.matches_pseudo(i, root, p))
    }

    /// CSS 方言里的属性存在判断（沿用 tl 的 `contains`）。
    fn css_has_attr(&self, i: usize, name: &str) -> bool {
        self.node(i)
            .and_then(|n| n.as_tag())
            .is_some_and(|t| t.attributes().contains(name))
    }

    fn matches_pseudo(&self, i: usize, root: Option<usize>, p: &Pseudo) -> bool {
        match p {
            Pseudo::Eq(n) => self.rank(i) == resolve_index(*n, self.siblings(i).len()) as i64,
            Pseudo::Lt(n) => self.rank(i) < *n,
            Pseudo::Gt(n) => self.rank(i) > *n,
            Pseudo::Contains(s) => self
                .node(i)
                .map(|n| {
                    n.inner_text(self.parser())
                        .to_lowercase()
                        .contains(&s.to_lowercase())
                })
                .unwrap_or(false),
            Pseudo::Matches(re) => regex::Regex::new(re)
                .map(|r| r.is_match(&self.text(i)))
                .unwrap_or(false),
            Pseudo::Not(group) => !group.iter().any(|c| self.matches_compound(i, root, c)),
            Pseudo::Has(sel) => sel.iter().any(|c| {
                self.subtree(Some(i))
                    .into_iter()
                    .any(|j| j != i && self.matches_complex(j, Some(i), c))
            }),
            Pseudo::NthChild(nth) => nth.matches(self.nth[i] as i64),
            Pseudo::NthLastChild(nth) => {
                let n = self.siblings(i).len() as i64;
                nth.matches(n - 1 - self.nth[i] as i64)
            }
            Pseudo::NthOfType(nth) => nth.matches(self.nth_of_type[i] as i64),
            Pseudo::NthLastOfType(nth) => {
                let n = self.same_type_count(i) as i64;
                nth.matches(n - 1 - self.nth_of_type[i] as i64)
            }
            Pseudo::FirstChild => self.nth[i] == 0,
            Pseudo::LastChild => self.nth[i] + 1 == self.siblings(i).len(),
            Pseudo::FirstOfType => self.nth_of_type[i] == 0,
            Pseudo::LastOfType => self.nth_of_type[i] + 1 == self.same_type_count(i),
            Pseudo::OnlyChild => self.siblings(i).len() == 1,
            Pseudo::Empty => self.children(Some(i)).is_empty() && self.text(i).is_empty(),
            Pseudo::Root => self.parent[i].is_none(),
        }
    }

    /// 在父元素的孩子里排第几；顶层元素按文档序排。
    fn rank(&self, i: usize) -> i64 {
        self.nth[i] as i64
    }

    fn siblings(&self, i: usize) -> Vec<usize> {
        match self.parent[i] {
            Some(p) => self.children(Some(p)),
            None => self.top_level(),
        }
    }

    fn same_type_count(&self, i: usize) -> usize {
        match self.parent[i] {
            Some(p) => {
                let name = self.tag_name(i);
                self.children(Some(p))
                    .into_iter()
                    .filter(|&j| self.tag_name(j) == name)
                    .count()
            }
            None => 1,
        }
    }
}

/// JSoup/legado 的空白归一化：连续空白变一个空格、首尾去掉。
/// `&nbsp;`（U+00A0）也算空白 —— JSoup 就是这么干的，中文站点里很常见。
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() || c == '\u{a0}' || c == '\u{3000}' {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

/// 把 `<script>…</script>` / `<style>…</style>` 整段删掉（对应 `@html` 的预处理）。
///
/// 没引 html5ever 的树，所以这里是对着字符串做一遍朴素扫描。
/// 真书源里 `<script>` 不会嵌 `<script>`，够用。
fn strip_elements(html: &str, names: &[&str]) -> String {
    let mut out = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'<' {
            if let Some(name) = names.iter().find(|n| {
                b[i + 1..].starts_with(n.as_bytes()) && is_name_end(b.get(i + 1 + n.len()))
            }) {
                let close = format!("</{name}");
                match lower[i..].find(&close) {
                    Some(j) => {
                        let after = i + j + close.len();
                        let end = lower[after..].find('>').map(|k| after + k + 1);
                        i = end.unwrap_or(b.len());
                        continue;
                    }
                    None => break,
                }
            }
        }
        let step = html[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&html[i..i + step]);
        i += step;
    }
    out
}

fn is_name_end(c: Option<&u8>) -> bool {
    matches!(
        c,
        None | Some(b'>') | Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
    )
}

// ── 选择器语法 ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Comb {
    Descendant,
    Child,
    Adjacent,
    Sibling,
}

#[derive(Debug, Clone, PartialEq)]
struct Complex {
    /// 第一项的 `Comb` 无意义
    parts: Vec<(Comb, Compound)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<(String, AttrMatch)>,
    pseudos: Vec<Pseudo>,
}

#[derive(Debug, Clone, PartialEq)]
enum AttrMatch {
    Exists,
    Eq(String),
    Prefix(String),
    Suffix(String),
    Contains(String),
    Word(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Pseudo {
    Eq(i64),
    Lt(i64),
    Gt(i64),
    Contains(String),
    Matches(String),
    Not(Vec<Compound>),
    Has(Vec<Complex>),
    NthChild(Nth),
    NthLastChild(Nth),
    NthOfType(Nth),
    NthLastOfType(Nth),
    FirstChild,
    LastChild,
    FirstOfType,
    LastOfType,
    OnlyChild,
    Empty,
    Root,
}

/// `an+b`
#[derive(Debug, Clone, Copy, PartialEq)]
struct Nth {
    a: i64,
    b: i64,
}

impl Nth {
    /// `idx` 是 0 开始的序号。
    fn matches(&self, idx: i64) -> bool {
        if self.a == 0 {
            return idx == self.b;
        }
        let d = idx - self.b;
        d % self.a == 0 && d / self.a >= 0
    }
}

fn parse_selector_list(s: &str) -> R<Vec<Complex>> {
    let groups = split_top(s, ',');
    if groups.iter().all(|g| g.trim().is_empty()) {
        return Err(Error::BadSelector(s.to_string()));
    }
    groups
        .iter()
        .map(|g| parse_complex(g.trim()))
        .collect::<R<Vec<_>>>()
}

/// 按顶层分隔符切（跳过 `[]`、`()` 和引号里的）。
fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
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
                c if c == sep && depth == 0 => {
                    out.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            },
        }
    }
    out.push(cur);
    out
}

fn parse_complex(s: &str) -> R<Complex> {
    let mut parts: Vec<(Comb, Compound)> = Vec::new();
    let mut comb = Comb::Descendant;
    let mut cur = String::new();
    let mut depth = 0;
    let mut quote: Option<char> = None;
    let flush = |cur: &mut String, comb: &mut Comb, parts: &mut Vec<(Comb, Compound)>| -> R<()> {
        let t = cur.trim();
        if !t.is_empty() {
            parts.push((comb.clone(), parse_compound(t)?));
        }
        cur.clear();
        Ok(())
    };
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if quote.is_some() {
            cur.push(c);
            if Some(c) == quote {
                quote = None;
            }
            i += 1;
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
            '>' | '+' | '~' if depth == 0 => {
                flush(&mut cur, &mut comb, &mut parts)?;
                comb = match c {
                    '>' => Comb::Child,
                    '+' => Comb::Adjacent,
                    _ => Comb::Sibling,
                };
            }
            c if c.is_whitespace() && depth == 0 => {
                // 空白只在「刚读完一个复合选择器，后面还跟着另一个」时才算后代组合符。
                // 否则它只是排版：`a > b` 里 `>` 两边的空格都不能把 Child 冲掉。
                let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                if !cur.trim().is_empty() && !matches!(next, Some('>' | '+' | '~')) {
                    flush(&mut cur, &mut comb, &mut parts)?;
                    comb = Comb::Descendant;
                }
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    flush(&mut cur, &mut comb, &mut parts)?;
    if parts.is_empty() {
        return Err(Error::BadSelector(s.to_string()));
    }
    Ok(Complex { parts })
}

fn parse_compound(s: &str) -> R<Compound> {
    let chars: Vec<char> = s.chars().collect();
    let mut c = Compound::default();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => i += 1,
            '#' => {
                let (v, n) = read_name(&chars, i + 1);
                if v.is_empty() {
                    return Err(Error::BadSelector(s.to_string()));
                }
                c.id = Some(v);
                i = n;
            }
            '.' => {
                let (v, n) = read_name(&chars, i + 1);
                if v.is_empty() {
                    return Err(Error::BadSelector(s.to_string()));
                }
                c.classes.push(v);
                i = n;
            }
            '[' => {
                let end = matching(&chars, i, '[', ']')?;
                let inner: String = chars[i + 1..end].iter().collect();
                c.attrs.push(parse_attr(inner.trim())?);
                i = end + 1;
            }
            ':' => {
                let (pseudo, n) = parse_pseudo(&chars, i + 1, s)?;
                c.pseudos.push(pseudo);
                i = n;
            }
            ch if ch.is_alphanumeric() || ch == '-' || ch == '_' => {
                let (v, n) = read_name(&chars, i);
                c.tag = Some(v);
                i = n;
            }
            _ => return Err(Error::BadSelector(s.to_string())),
        }
    }
    Ok(c)
}

fn read_name(chars: &[char], mut i: usize) -> (String, usize) {
    let start = i;
    while i < chars.len()
        && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_' || chars[i] == '\\')
    {
        i += 1;
    }
    (chars[start..i].iter().collect(), i)
}

fn matching(chars: &[char], open: usize, o: char, cl: char) -> R<usize> {
    let mut depth = 0;
    let mut quote: Option<char> = None;
    for (i, &c) in chars.iter().enumerate().skip(open) {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c == o {
            depth += 1;
        } else if c == cl {
            depth -= 1;
            if depth == 0 {
                return Ok(i);
            }
        }
    }
    Err(Error::BadSelector(chars.iter().collect()))
}

fn parse_attr(s: &str) -> R<(String, AttrMatch)> {
    for (op, kind) in [
        ("^=", 1),
        ("$=", 2),
        ("*=", 3),
        ("~=", 4),
        ("|=", 5),
        ("=", 0),
    ] {
        if let Some(at) = s.find(op) {
            let name = s[..at].trim().to_string();
            let val = unquote(s[at + op.len()..].trim());
            return Ok((
                name,
                match kind {
                    1 => AttrMatch::Prefix(val),
                    2 => AttrMatch::Suffix(val),
                    3 => AttrMatch::Contains(val),
                    4 => AttrMatch::Word(val),
                    // JSoup 的 `|=` 很少见，按前缀加连字符近似
                    5 => AttrMatch::Prefix(val),
                    _ => AttrMatch::Eq(val),
                },
            ));
        }
    }
    Ok((s.trim().to_string(), AttrMatch::Exists))
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    let b = t.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

fn parse_pseudo(chars: &[char], mut i: usize, whole: &str) -> R<(Pseudo, usize)> {
    let (name, n) = read_name(chars, i);
    i = n;
    let name = name.to_lowercase();
    let mut arg: Option<String> = None;
    if chars.get(i) == Some(&'(') {
        let end = matching(chars, i, '(', ')')?;
        arg = Some(chars[i + 1..end].iter().collect());
        i = end + 1;
    }
    let need = |a: &Option<String>| -> R<String> {
        a.clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| Error::BadSelector(whole.to_string()))
    };
    let p = match name.as_str() {
        "eq" => Pseudo::Eq(need(&arg)?.trim().parse().unwrap_or(-1)),
        "lt" => Pseudo::Lt(need(&arg)?.trim().parse().unwrap_or(0)),
        "gt" => Pseudo::Gt(need(&arg)?.trim().parse().unwrap_or(0)),
        "contains" => Pseudo::Contains(unquote(&need(&arg)?)),
        "matches" | "containsown" => Pseudo::Matches(need(&arg)?),
        "not" => {
            let inner = need(&arg)?;
            let mut cs = Vec::new();
            for g in split_top(&inner, ',') {
                cs.push(parse_compound(g.trim())?);
            }
            Pseudo::Not(cs)
        }
        "has" => Pseudo::Has(parse_selector_list(&need(&arg)?)?),
        "nth-child" => Pseudo::NthChild(parse_nth(&need(&arg)?)?),
        "nth-last-child" => Pseudo::NthLastChild(parse_nth(&need(&arg)?)?),
        "nth-of-type" => Pseudo::NthOfType(parse_nth(&need(&arg)?)?),
        "nth-last-of-type" => Pseudo::NthLastOfType(parse_nth(&need(&arg)?)?),
        "first-child" => Pseudo::FirstChild,
        "last-child" => Pseudo::LastChild,
        "first-of-type" => Pseudo::FirstOfType,
        "last-of-type" => Pseudo::LastOfType,
        "only-child" => Pseudo::OnlyChild,
        "empty" => Pseudo::Empty,
        "root" => Pseudo::Root,
        _ => return Err(Error::BadSelector(format!("{whole}（:{name} 不认识）"))),
    };
    Ok((p, i))
}

fn parse_nth(s: &str) -> R<Nth> {
    let t = s.trim().to_lowercase();
    match t.as_str() {
        // 注意是 0 基序号：odd 是第 1、3、5 个 → 下标 0、2、4
        "odd" => return Ok(Nth { a: 2, b: 0 }),
        "even" => return Ok(Nth { a: 2, b: 1 }),
        _ => {}
    }
    if let Ok(n) = t.parse::<i64>() {
        // 1 开始 → 0 开始
        return Ok(Nth { a: 0, b: n - 1 });
    }
    let (a, b) = match t.split_once('n') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => return Err(Error::BadSelector(s.to_string())),
    };
    let a = match a {
        "" | "+" => 1,
        "-" => -1,
        _ => a.parse().map_err(|_| Error::BadSelector(s.to_string()))?,
    };
    let b = if b.is_empty() {
        0
    } else {
        b.replace(' ', "")
            .parse::<i64>()
            .map_err(|_| Error::BadSelector(s.to_string()))?
    };
    Ok(Nth { a, b: b - 1 })
}

// ── legado 的索引后缀 ───────────────────────────────────────────────────

/// `class.foo.0` / `tag.li!0:1:2` / `tag.div[0, -1, 3:5]` 里的索引部分。
#[derive(Debug, Default, Clone)]
struct IndexFilter {
    exclude: bool,
    items: Vec<Idx>,
}

#[derive(Debug, Clone)]
enum Idx {
    One(i64),
    /// `start:end:step`，两端都可以省（`None` = legado 的默认值：0 / 末位）
    Range(Option<i64>, Option<i64>, i64),
}

impl IndexFilter {
    fn apply(&self, els: &mut Vec<Option<usize>>) {
        if self.items.is_empty() {
            return;
        }
        let len = els.len() as i64;
        // keep/drop 里装的是**位置**，不是元素序号
        let mut keep: Vec<usize> = Vec::new();
        let mut drop: Vec<usize> = Vec::new();
        for it in &self.items {
            match it {
                Idx::One(n) => {
                    if let Some(i) = resolve_one(*n, len) {
                        (if self.exclude { &mut drop } else { &mut keep }).push(i);
                    }
                }
                Idx::Range(start, end, step) => {
                    for i in expand_range(*start, *end, *step, len) {
                        (if self.exclude { &mut drop } else { &mut keep }).push(i);
                    }
                }
            }
        }
        if self.exclude {
            let victims: Vec<Option<usize>> =
                drop.iter().filter_map(|&p| els.get(p)).copied().collect();
            els.retain(|i| !victims.contains(i));
        } else {
            let mut picked: Vec<Option<usize>> = Vec::new();
            for i in keep {
                if i < els.len() && !picked.contains(&els[i]) {
                    picked.push(els[i]);
                }
            }
            *els = picked;
        }
    }
}

fn resolve_one(n: i64, len: i64) -> Option<usize> {
    if n >= 0 && n < len {
        Some(n as usize)
    } else if n < 0 && -n <= len {
        Some((n + len) as usize)
    } else {
        None
    }
}

/// 展开区间。负数索引和「反向」都照抄 legado 的 `getElementsSingle`。
fn expand_range(start: Option<i64>, end: Option<i64>, step_raw: i64, len: i64) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    // 省左端 = 0，省右端 = 末位；负数从尾部数
    let mut start = start.unwrap_or(0);
    if start < 0 {
        start += len;
    }
    let mut end = end.unwrap_or(len - 1);
    if end < 0 {
        end += len;
    }
    if (start < 0 && end < 0) || (start >= len && end >= len) {
        return Vec::new();
    }
    start = start.clamp(0, len - 1);
    end = end.clamp(0, len - 1);
    if start == end || step_raw >= len {
        return vec![start as usize];
    }
    let step = if step_raw > 0 {
        step_raw
    } else if -step_raw < len {
        step_raw + len
    } else {
        1
    };
    let step = step.max(1);
    let mut out = Vec::new();
    if end > start {
        let mut i = start;
        while i <= end {
            out.push(i as usize);
            i += step;
        }
    } else {
        let mut i = start;
        while i >= end {
            out.push(i as usize);
            i -= step;
        }
    }
    out
}

fn resolve_index(n: i64, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as i64;
    if n < 0 {
        (n + len).max(0) as usize
    } else {
        n.min(len - 1).max(0) as usize
    }
}

/// 把 `class.foo.0` 拆成 (`class.foo`, 索引过滤器)。
fn split_index(rule: &str) -> (String, IndexFilter) {
    let rule = rule.trim();
    if rule.ends_with(']') {
        let chars: Vec<char> = rule.chars().collect();
        if let Some(open) = last_unmatched_open(&chars) {
            let inner: String = chars[open + 1..chars.len() - 1].iter().collect();
            let before: String = chars[..open].iter().collect();
            if let Some(f) = parse_bracket_index(&inner) {
                return (before, f);
            }
        }
    }
    // 老写法：从末尾往回吃 `[0-9\-:]`
    let b = rule.as_bytes();
    let mut j = b.len();
    while j > 0 && (b[j - 1].is_ascii_digit() || b[j - 1] == b'-' || b[j - 1] == b':') {
        j -= 1;
    }
    if j == b.len() {
        return (rule.to_string(), IndexFilter::default());
    }
    if j == 0 || (b[j - 1] != b'.' && b[j - 1] != b'!') {
        return (rule.to_string(), IndexFilter::default());
    }
    let nums: Vec<i64> = rule[j..]
        .split(':')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if nums.is_empty() {
        return (rule.to_string(), IndexFilter::default());
    }
    (
        rule[..j - 1].to_string(),
        IndexFilter {
            exclude: b[j - 1] == b'!',
            items: nums.into_iter().map(Idx::One).collect(),
        },
    )
}

/// 找最后一个「能一路平衡到结尾」的 `[`。
fn last_unmatched_open(chars: &[char]) -> Option<usize> {
    let mut depth = 0i32;
    let mut open = None;
    for (i, &c) in chars.iter().enumerate().rev() {
        if c == ']' {
            depth += 1;
        } else if c == '[' {
            depth -= 1;
            if depth == 0 {
                open = Some(i);
            }
        }
    }
    open
}

fn parse_bracket_index(inner: &str) -> Option<IndexFilter> {
    let inner = inner.trim();
    let (exclude, body) = match inner.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, inner),
    };
    let mut items = Vec::new();
    for part in body.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part.contains(':') {
            let mut seg = part.split(':').map(str::trim);
            let a = seg
                .next()
                .and_then(|s| if s.is_empty() { None } else { s.parse().ok() });
            let b = seg
                .next()
                .and_then(|s| if s.is_empty() { None } else { s.parse().ok() });
            let c = seg
                .next()
                .and_then(|s| if s.is_empty() { None } else { s.parse().ok() });
            if seg.next().is_some() {
                return None;
            }
            items.push(Idx::Range(a, b, c.unwrap_or(1)));
        } else {
            items.push(Idx::One(part.parse().ok()?));
        }
    }
    Some(IndexFilter { exclude, items })
}

// ── HTML → 纯文本 ───────────────────────────────────────────────────────

/// 把一段 HTML 洗成纯文本。移植自 legado 的 `HtmlFormatter.format`。
///
/// 顺序是有讲究的，不能改：
/// 1. `&nbsp;` 系先变成空格（**在**去标签之前，否则 `&lt;div&gt;` 会变成真标签）；
/// 2. 块级标签的边界变成换行；
/// 3. 注释和其余标签去掉；
/// 4. 最后把换行周围收拾干净，并用全角空格做首行缩进（中文小说就这么排）。
///
/// 空白类刻意写成 ASCII 显式集合而不是 `\s`：Rust 的 `\s` 默认是 Unicode 空白，
/// 会把中文排版用的全角空格（U+3000）也当空白吃掉，Java 不会。
pub fn format_text(html: &str) -> String {
    let steps: Vec<(regex::Regex, &str)> = vec![
        (re(r"(&nbsp;)+"), " "),
        (re(r"(&ensp;|&emsp;)"), " "),
        (re("(&thinsp;|&zwnj;|&zwj;|\u{2009}|\u{200C}|\u{200D})"), ""),
        (re(r"</?(?:div|p|br|hr|h\d|article|dd|dl)[^>]*>"), "\n"),
        (re(r"<!--[^>]*-->"), ""),
        // legado 这里是 `</?(?!img)[a-zA-Z]+(?=[ >])[^<>]*>`，靠前瞻把 img 留下来。
        // Rust 的正则没有前瞻，而 TUI 也画不出图，所以干脆把所有标签都去掉。
        (re(r"</?[a-zA-Z]+[^<>]*>"), ""),
        (
            re(r"[\x20\t\n\x0B\x0C\r]*\n+[\x20\t\n\x0B\x0C\r]*"),
            "\n\u{3000}\u{3000}",
        ),
        (re(r"^[\n\x20\t\x0B\x0C\r]+"), "\u{3000}\u{3000}"),
        (re(r"[\n\x20\t\x0B\x0C\r]+$"), ""),
    ];
    let mut out = html.to_string();
    for (pat, to) in steps {
        out = pat.replace_all(&out, to).into_owned();
    }
    out
}

fn re(p: &str) -> regex::Regex {
    // 模式都是写死的常量，编不出来就是代码写错了
    regex::Regex::new(p).expect("内置的 HTML 清洗正则")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const PAGE: &str = r#"
<html><body>
<div class="excerpts-wrapper"><div class="excerpts">
  <article class="excerpt excerpt-c3"><a href="/qi-xing-lu-wang-01.html">七星鲁王 第一章 血尸</a></article>
  <article class="excerpt excerpt-c3"><a href="/qi-xing-lu-wang-02.html">七星鲁王 第二章 五十年后</a></article>
  <article class="excerpt excerpt-c3"><a href="/qi-xing-lu-wang-03.html">七星鲁王 第三章 瓜子庙</a></article>
</div></div>
<article class="article-content">
  <p>　　50年前，长沙镖子岭。</p>
  <p>　　“这下子麻烦大喽。”</p>
  <p>广告<span>（此处是广告）</span></p>
</article>
<ul class="list"><li id="a">甲</li><li id="b" class="x">乙</li><li id="c">丙</li><li id="d">丁</li></ul>
<div class="box"><p class="inner">嵌套</p></div>
<table><tr><td>1</td><td>2</td></tr><tr><td>3</td><td>4</td></tr></table>
</body></html>"#;

    fn h() -> Html<'static> {
        Html::parse(PAGE)
    }

    fn texts(html: &Html<'_>, ids: &[usize]) -> Vec<String> {
        ids.iter().map(|&i| html.text(i)).collect()
    }

    #[test]
    fn parse_tree_has_parents_and_sibling_order() {
        let html = h();
        let all = html.subtree(None);
        assert!(all.len() > 10);
        // parent 指向的元素确实排在前面（文档序性质）
        for i in 0..all.len() {
            if let Some(p) = html.parent[i] {
                assert!(p < i, "父元素应当排在孩子前面");
            }
        }
        assert_eq!(html.parent[0], None, "顶层元素没有父");
    }

    #[test]
    fn simple_selectors_and_combinators() {
        let html = h();
        let n = |sel: &str| html.select(None, sel).unwrap().len();
        assert_eq!(n("a"), 3);
        assert_eq!(n("article"), 4);
        assert_eq!(n("article.excerpt"), 3);
        assert_eq!(n(".excerpts-wrapper"), 1);
        // 这两个正是 tl 自带 query_selector 做不对的
        assert_eq!(n(".excerpts-wrapper a"), 3, "后代组合符");
        assert_eq!(n("article a"), 3);
        assert_eq!(n("body > div"), 2, "子组合符");
        assert_eq!(n("li + li"), 3, "相邻兄弟");
        assert_eq!(n("li ~ li"), 3, "一般兄弟");
        assert_eq!(n("a, p"), 3 + 4);
        assert_eq!(n("#b"), 1);
        assert_eq!(n("li.x"), 1);
        assert_eq!(n("*"), html.subtree(None).len());
    }

    #[test]
    fn jsoup_extensions() {
        let html = h();
        let t = |sel: &str| texts(&html, &html.select(None, sel).unwrap());
        assert_eq!(t("li:eq(1)"), ["乙"]);
        assert_eq!(t("li:lt(2)"), ["甲", "乙"]);
        assert_eq!(t("li:gt(1)"), ["丙", "丁"]);
        assert_eq!(t(".list li:eq(0)"), ["甲"]);
        assert_eq!(t("li:contains(丙)"), ["丙"]);
        assert_eq!(t("li:matches(^[甲乙]$)"), ["甲", "乙"]);
        assert_eq!(t("li:not(.x):not(#a)"), ["丙", "丁"]);
        assert_eq!(t("li:nth-child(2)"), ["乙"]);
        assert_eq!(t("li:nth-child(odd)"), ["甲", "丙"]);
        assert_eq!(t("li:last-child"), ["丁"]);
        assert_eq!(t("li:first-child"), ["甲"]);
        // :has 在真书源里用来「找到含某类子节点的行」
        assert_eq!(t("article:has(a)").len(), 3);
        assert_eq!(t("ul:has(li.x)").len(), 1);
        assert_eq!(t("li:not(:has(span))").len(), 4);
    }

    #[test]
    fn attribute_selectors() {
        let html = h();
        let n = |sel: &str| html.select(None, sel).unwrap().len();
        assert_eq!(n("[href]"), 3);
        assert_eq!(n("a[href='/qi-xing-lu-wang-01.html']"), 1);
        assert_eq!(n("a[href^='/qi-xing']"), 3);
        assert_eq!(n("a[href$='01.html']"), 1);
        assert_eq!(n("a[href*='lu-wang']"), 3);
        assert_eq!(n("li[class~=x]"), 1);
        assert_eq!(n("article[class~=excerpt]"), 3, "多类名里逐词匹配");
        // 属性值可以带引号也可以不带
        assert_eq!(n("a[href=/qi-xing-lu-wang-01.html]"), 1);
    }

    #[test]
    fn unknown_pseudo_and_bad_syntax_are_errors_not_silent_empty() {
        let html = h();
        assert!(matches!(
            html.select(None, "li:whatever(1)"),
            Err(Error::BadSelector(_))
        ));
        assert!(matches!(
            html.select(None, "li["),
            Err(Error::BadSelector(_))
        ));
        assert!(matches!(html.select(None, ""), Err(Error::BadSelector(_))));
    }

    #[test]
    fn subtree_scoping_stops_at_the_root() {
        let html = h();
        let wrapper = html.select(None, ".excerpts-wrapper").unwrap()[0];
        // 在 wrapper 里找到 3 个 a
        assert_eq!(html.select(Some(wrapper), "a").unwrap().len(), 3);
        // 但 `div a` 不能靠 wrapper 外面的祖先来满足
        assert_eq!(html.select(Some(wrapper), "body a").unwrap().len(), 0);
        assert_eq!(html.select(Some(wrapper), "div a").unwrap().len(), 3);
        // 根自己也算候选
        assert_eq!(
            html.select(Some(wrapper), ".excerpts-wrapper")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn text_accessors_match_jsoup() {
        let html = h();
        let p = |sel: &str| html.select(None, sel).unwrap();

        // text：所有后代文本，空白归一化
        assert_eq!(
            html.text(p("article.article-content")[0]),
            "50年前，长沙镖子岭。 “这下子麻烦大喽。” 广告（此处是广告）"
        );
        // ownText：只有直接文本节点，不含 <span>
        assert_eq!(html.own_text(p(".article-content p")[2]), "广告");
        assert_eq!(html.own_text(p("article.article-content")[0]), "");
        // textNodes：直接文本节点，各自 trim
        assert_eq!(html.text_nodes(p(".list li")[0]), "甲");
    }

    #[test]
    fn attribute_results_are_deduped_like_legado() {
        // getResultLast 的 else 分支会跳过重复值，这一条容易漏
        let f = IndexFilter::default();
        let mut els: Vec<Option<usize>> = vec![Some(0), Some(1), Some(2)];
        f.apply(&mut els);
        assert_eq!(els, [Some(0), Some(1), Some(2)], "没有索引时不过滤");
    }

    #[test]
    fn old_style_index_suffix() {
        let (before, f) = split_index("tag.li!0");
        assert_eq!(before, "tag.li");
        assert!(f.exclude);
        assert_eq!(f.items.len(), 1);

        // 注意尾巴上那个 `.`：legado 也是这么切的（它从 `!` 处往回 take）。
        // 无害 —— 后面只用 `parts[1]`，多出来的空段没人看。
        let (before, f) = split_index("tag.dl.!0:1:2:3:4:5");
        assert_eq!(before, "tag.dl.");
        assert!(f.exclude);
        assert_eq!(f.items.len(), 6);

        let (before, f) = split_index("tag.a.0");
        assert_eq!(before, "tag.a");
        assert!(!f.exclude);
        assert_eq!(f.items.len(), 1);

        // 不是数字后缀 → 没有索引
        assert!(split_index("text.下一页").1.items.is_empty());
        assert_eq!(split_index("text.下一页").0, "text.下一页");
        assert!(split_index("div.x").1.items.is_empty());
        assert!(split_index("a").1.items.is_empty());
    }

    #[test]
    fn bracket_index_suffix() {
        let (before, f) = split_index("tag.div[-1, 3:5, 2]");
        assert_eq!(before, "tag.div");
        assert!(!f.exclude);
        assert_eq!(f.items.len(), 3);
        let (_, f) = split_index("tag.div[!0:3]");
        assert!(f.exclude);
        assert_eq!(split_index("tag.div[-1:0]").0, "tag.div");
    }

    #[test]
    fn index_filters_on_real_shapes() {
        let html = h();
        let run = |rule: &str| {
            let cur: Vec<Option<usize>> = html.top_level().into_iter().map(Some).collect();
            let got = html.step(&cur, rule).unwrap();
            html.accessor(&got, "text")
        };
        // 真书源「顶点小说」的写法：排除前 10 个 li
        assert_eq!(run("tag.li!0").len(), 3);
        // 取第 2 个
        assert_eq!(run("tag.li.1"), ["乙"]);
        // 负数从尾部数
        assert_eq!(run("tag.li.-1"), ["丁"]);
        // 越界不 panic，只是空
        assert!(run("tag.li.99").is_empty());
        assert!(run("tag.li.-99").is_empty());
        // 反向
        assert_eq!(run("tag.li[-1:0]"), ["丁", "丙", "乙", "甲"]);
        // 区间
        assert_eq!(run("tag.li[1:3]"), ["乙", "丙", "丁"]);
        // 排除区间
        assert_eq!(run("tag.li[!0:2]"), ["丁"]);
    }

    #[test]
    fn steps_follow_the_legado_dialect() {
        let html = h();
        let run = |rule: &str| {
            let mut cur: Vec<Option<usize>> = html.top_level().into_iter().map(Some).collect();
            for step in rule.split('@') {
                cur = html.step(&cur, step).unwrap();
            }
            html.accessor(&cur, "text")
        };
        // 真书源「盗墓笔记」的三条规则，逐字对着线上页面写下来的
        assert_eq!(run("class.excerpts-wrapper@tag.a").len(), 3);
        assert_eq!(
            run("class.excerpts-wrapper@tag.a@text"),
            [
                "七星鲁王 第一章 血尸",
                "七星鲁王 第二章 五十年后",
                "七星鲁王 第三章 瓜子庙"
            ]
        );
        // class. 是「后代里找」，不是「孩子里找」
        assert_eq!(run("class.excerpts-wrapper@tag.a").len(), 3);
        // 逐级收窄
        assert_eq!(run("class.excerpts-wrapper@tag.article@tag.a").len(), 3);
        // id / text / children 三种步骤
        assert_eq!(run("id.b"), ["乙"]);
        assert_eq!(run("text.丙"), ["丙"]);
        assert_eq!(run("class.list@children").len(), 4);
        // 一步都写不下去时是空，不是 panic
        assert!(run("class.不存在").is_empty());
    }

    #[test]
    fn end_to_end_accessor_results() {
        // 模拟 crawl 层做的事：一堆 step，然后一个取值器
        let html = h();
        let top: Vec<Option<usize>> = html.top_level().into_iter().map(Some).collect();
        let els = html.step(&top, "class.excerpts-wrapper").unwrap();
        let els = html.step(&els, "tag.a").unwrap();
        let hrefs = html.accessor(&els, "href");
        assert_eq!(hrefs[0], "/qi-xing-lu-wang-01.html");
        assert_eq!(hrefs.len(), 3);
    }

    #[test]
    fn malformed_html_does_not_panic() {
        for src in [
            "",
            "<",
            "<div",
            "<div class=",
            "<p>未闭合",
            "纯文本没有标签",
            "<a href='x'>1</a><a href='x'>2</a>",
        ] {
            let html = Html::parse(src);
            let _ = html.select(None, "a");
            let _ = html.select(None, "div p:eq(0)");
            let top: Vec<Option<usize>> = html.top_level().into_iter().map(Some).collect();
            let _ = html.step(&top, "tag.a.0");
            let _ = html.accessor(&[], "text");
            let _ = html.text(0);
            let _ = html.outer_html(0);
            let _ = html.attr(0, "href");
        }
    }

    #[test]
    fn html_accessor_drops_script_and_style() {
        let src = r#"<div id="c">正文<script>var a=1;</script>尾<style>p{color:red}</style></div>"#;
        let html = Html::parse(src);
        let els = html.select(None, "#c").unwrap();
        let out = html.html_without_scripts(&els);
        assert!(!out.contains("var a=1"), "{out}");
        assert!(!out.contains("color:red"), "{out}");
        assert!(out.contains("正文"), "{out}");
    }

    #[test]
    fn normalize_collapses_whitespace_including_nbsp() {
        assert_eq!(normalize("  a\u{a0}\u{a0}b  "), "a b");
        assert_eq!(normalize("\n\t a \n b \n"), "a b");
        assert_eq!(normalize("\u{3000}全角空格"), "全角空格");
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("   "), "");
    }

    #[test]
    fn nth_expressions() {
        let m = |s: &str, i: i64| parse_nth(s).unwrap().matches(i);
        assert!(m("1", 0) && !m("1", 1));
        assert!(m("odd", 0) && !m("odd", 1) && m("odd", 2));
        assert!(m("even", 1) && !m("even", 0));
        // `2n+1` = 第 1、3、5 个 → 下标 0、2、4
        assert!(m("2n+1", 0) && m("2n+1", 2) && !m("2n+1", 1));
        // `3n` = 第 3、6、9 个 → 下标 2、5、8（注意不是 0、3、6）
        assert!(m("3n", 2) && m("3n", 5) && !m("3n", 0) && !m("3n", 1));
        assert!(!m("0n+3", 0) && m("0n+3", 2));
        // 负系数：`-n+3` = 第 3、2、1 个 → 下标 2、1、0；不能死循环
        assert!(m("-1n+3", 0) && m("-1n+3", 2) && !m("-1n+3", 3));
    }

    #[test]
    fn range_expansion_is_bounded() {
        // 步长为 0/负数/超大都不能死循环
        for (a, b, s) in [
            (0, 10, 0),
            (0, 10, -1),
            (0, 10, 1000),
            (10, 0, -3),
            (0, 0, 1),
        ] {
            let v = expand_range(Some(a), Some(b), s, 20);
            assert!(v.len() <= 20, "{a}:{b}:{s} 展开成了 {} 个", v.len());
        }
        assert_eq!(expand_range(Some(0), Some(4), 1, 10), [0, 1, 2, 3, 4]);
        assert_eq!(expand_range(Some(4), Some(0), 1, 10), [4, 3, 2, 1, 0]);
        // 两端省略
        assert_eq!(expand_range(None, None, 1, 3), [0, 1, 2]);
        assert_eq!(expand_range(None, Some(1), 1, 10), [0, 1]);
        assert_eq!(expand_range(Some(8), None, 1, 10), [8, 9]);
        // 步长比长度还大就只取首位（legado 的行为）
        assert_eq!(expand_range(Some(0), Some(9), 100, 10), [0]);
    }
}
