//! 抓取链路：搜索 → 详情 → 目录 → 正文。
//!
//! 顺序和每一步用哪条规则，照抄 legado 的 `WebBook` / `BookList` /
//! `BookInfo` / `BookChapterList` / `BookContent`。
//!
//! 每个「列表 → 逐项取字段」的地方都有一个容易做错的细节：**子规则的求值起点
//! 是列表里的那一个元素**，不是整篇文档。所以 `bookList` 选出 `li` 之后，
//! `name: "tag.a.0@text"` 是「在这个 li 里找第 0 个 a」，而不是在全页里找。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::html::Html;
use crate::js::{Bindings, Host, Js, Mode as JsMode};
use crate::net;
use crate::rule::{self, Env, Mode, Rule};
use crate::source::BookSource;
use crate::{Error, R};

/// 一次抓取只会用到一个方法。抽成 trait 是为了让测试能喂录好的页面，
/// 而不是每次跑测试都去敲真实站点。
///
/// `Clone + 'static` 是 JS 宿主加的要求：`Js::new` 收 `Rc<dyn Host>`，
/// 宿主必须**拥有**自己那份抓取能力，不能借用 `Crawler`。
/// 抓一个 URL。
///
/// **不带 `Clone` 父约束** —— 带了就 `dyn` 不了（`Clone` 要 `Sized`），
/// 而 `Online` 需要 `Arc<dyn Fetch>` 才能让测试塞假抓取器进来。
/// 需要克隆的地方在 `Crawler` 的 impl 上单独要 `F: Clone`。
pub trait Fetch: Send + Sync + 'static {
    fn get(&self, spec: &str) -> R<net::Response>;
}

impl Fetch for net::Fetcher {
    fn get(&self, spec: &str) -> R<net::Response> {
        net::Fetcher::get(self, spec)
    }
}

/// 让 `Crawler<Arc<dyn Fetch>>` 能用，`Online` 就可以是「注入哪个抓取器都行」
/// 的 —— 否则搜索的翻页逻辑只能用真站点测，而真站点会挂、会变、会慢。
impl Fetch for std::sync::Arc<dyn Fetch> {
    fn get(&self, spec: &str) -> R<net::Response> {
        (**self).get(spec)
    }
}

/// 搜索结果里的一条。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hit {
    pub name: String,
    pub author: String,
    pub url: String,
    pub intro: String,
    pub kind: String,
    pub cover: String,
    pub last_chapter: String,
}

/// 书籍详情。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    pub name: String,
    pub author: String,
    pub intro: String,
    pub kind: String,
    pub cover: String,
    pub last_chapter: String,
    /// 详情页里指向目录页的链接；为空就用详情页自己当目录页
    pub toc_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chapter {
    pub title: String,
    pub url: String,
}

/// 分页上限。legado 只靠 URL 去重，站点返回递增 URL 就能无限抓下去；
/// 加个上限是刻意的改进。
pub const MAX_PAGES: usize = 50;

/// 规则求值的**起点**。
///
/// 一个书源要么是 HTML（选择器），要么是 JSON（路径）。子规则的起点就是这个：
/// `bookList` 给出的元素，`name` 再在它上面取值。
///
/// 之前这个位置是个裸的 `Option<usize>`（HTML 元素下标），JSON 源只好一律报
/// `Unsupported` —— 那是 545 个源。
#[derive(Debug, Clone)]
enum Node {
    /// HTML 文档里的一个元素下标
    El(usize),
    /// 一个 JSON 值
    Json(Box<serde_json::Value>),
}

pub struct Crawler<F> {
    pub source: BookSource,
    pub fetch: F,
    pub max_pages: usize,
    /// 给 JS 的 `book.name` / `book.author` 用（详情页拿到之后由上层填上）
    pub book_name: String,
    pub book_author: String,
    /// 书源变量（`java.put` / `@get:`）。跨规则共享，所以在 Crawler 这一层。
    pub vars: Rc<RefCell<HashMap<String, String>>>,
}

impl<F: Fetch + Clone> Crawler<F> {
    pub fn new(source: BookSource, fetch: F) -> Self {
        Self {
            source,
            fetch,
            max_pages: MAX_PAGES,
            book_name: String::new(),
            book_author: String::new(),
            vars: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// 造一个「页面上下文」的宿主：`java.ajax` 和 `java.getString` 都能用。
    fn host(&self, content: String, base: &str, key: &str, page: u32) -> Rc<RuleHost> {
        let fetcher = self.fetch.clone();
        Rc::new(RuleHost {
            ajax: Rc::new(move |url: &str| match fetcher.get(url) {
                Ok(r) => r.body,
                Err(_) => String::new(),
            }),
            source: self.source.clone(),
            key: key.to_string(),
            page,
            content: RefCell::new(content),
            base: RefCell::new(base.to_string()),
            vars: self.vars.clone(),
            book_name: self.book_name.clone(),
            book_author: self.book_author.clone(),
            chapter_title: RefCell::new(String::new()),
            chapter_url: RefCell::new(String::new()),
            chapter_index: RefCell::new(0),
        })
    }

    /// 搜索。`page` 从 1 开始（`searchUrl` 里的 `{{page}}` 直接用这个值）。
    pub fn search(&self, key: &str, page: u32) -> R<Vec<Hit>> {
        if !self.source.can_search() {
            return Err(Error::Missing("searchUrl（这个源不支持搜索）".into()));
        }
        let spec = self.source.search_url.clone().unwrap_or_default();
        let res = self.get(&spec, key, page)?;
        let html = Html::parse(&res.body);
        let ev = Eval::new(&html, &res.body, &res.url, key, page).with_host(self.host(
            res.body.clone(),
            &res.url,
            key,
            page,
        ));
        let rule = &self.source.search;
        let mut out = Vec::new();
        for it in ev.elements(rule.book_list.trim())? {
            let it = Some(&it);
            // 书名和链接是**必需**的（没有就没法用），其余是锦上添花。
            // 非必需字段取不到就留空，**不能让整次搜索跟着失败** ——
            // 一个写坏的简介规则把全部结果都搞没，是比少个简介严重得多的事。
            let hit = Hit {
                name: ev.string(it, &rule.name)?,
                url: ev.string_url(it, &rule.book_url)?,
                author: ev.string(it, &rule.author).unwrap_or_default(),
                intro: ev.string(it, &rule.intro).unwrap_or_default(),
                kind: ev.string(it, &rule.kind).unwrap_or_default(),
                cover: ev.string_url(it, &rule.cover_url).unwrap_or_default(),
                last_chapter: ev.string(it, &rule.last_chapter).unwrap_or_default(),
            };
            // 没有书名的行是噪声（页面上到处都有 li）
            if !hit.name.is_empty() {
                out.push(hit);
            }
        }
        Ok(out)
    }

    /// 书籍详情。顺带把 `tocUrl` 也解出来。
    pub fn info(&self, url: &str) -> R<Info> {
        let res = self.get(url, "", 1)?;
        let html = Html::parse(&res.body);
        let ev = Eval::new(&html, &res.body, &res.url, "", 1).with_host(self.host(
            res.body.clone(),
            &res.url,
            "",
            1,
        ));
        let rule = &self.source.book_info;
        Ok(Info {
            name: ev.string(None, &rule.name)?,
            author: ev.string(None, &rule.author).unwrap_or_default(),
            // 简介也常带标签，legado 同样过一遍 HtmlFormatter
            intro: crate::html::format_text(&ev.string(None, &rule.intro).unwrap_or_default()),
            kind: ev.string(None, &rule.kind).unwrap_or_default(),
            cover: ev.string_url(None, &rule.cover_url).unwrap_or_default(),
            last_chapter: ev.string(None, &rule.last_chapter).unwrap_or_default(),
            toc_url: ev.string_url(None, &rule.toc_url).unwrap_or_default(),
        })
    }

    /// 目录。会跟着 `nextTocUrl` 翻页，最多 [`MAX_PAGES`] 页。
    pub fn toc(&self, url: &str) -> R<Vec<Chapter>> {
        let mut out = Vec::new();
        let mut next = Some(url.to_string());
        let mut seen: Vec<String> = Vec::new();
        while let Some(u) = next.take() {
            if seen.contains(&u) || seen.len() >= self.max_pages {
                break;
            }
            seen.push(u.clone());
            let res = self.get(&u, "", 1)?;
            let html = Html::parse(&res.body);
            let ev = Eval::new(&html, &res.body, &res.url, "", 1).with_host(self.host(
                res.body.clone(),
                &res.url,
                "",
                1,
            ));
            let rule = &self.source.toc;
            for it in ev.elements(rule.chapter_list.trim())? {
                let it = Some(&it);
                let title = ev.string(it, &rule.chapter_name)?;
                // 算 URL 之前先把这一章的标题挂上（`chapterUrl` 的脚本会读它）
                ev.set_chapter(&title, "", out.len() + 1);
                // 标题必需（没标题的目录没法看），链接取不到就留空：
                // 有些站的链接要靠脚本拼，不该因为这种字段让整本书打不开
                let url = ev.string_url(it, &rule.chapter_url).unwrap_or_default();
                if !title.is_empty() {
                    out.push(Chapter { title, url });
                }
            }
            if !rule.next_toc_url.trim().is_empty() {
                next = ev
                    .strings(None, &rule.next_toc_url, true)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|u| u != &res.url && !seen.contains(u));
            }
        }
        // 同一章可能在多页里重复出现
        let mut dedup: Vec<Chapter> = Vec::new();
        for c in out {
            if !dedup.iter().any(|d| d.url == c.url) {
                dedup.push(c);
            }
        }
        Ok(dedup)
    }

    /// 正文。会跟着 `nextContentUrl` 往后接，最多 [`MAX_PAGES`] 段。
    pub fn content(&self, url: &str) -> R<String> {
        let mut parts = Vec::new();
        let mut next = Some(url.to_string());
        let mut seen: Vec<String> = Vec::new();
        while let Some(u) = next.take() {
            if seen.contains(&u) || seen.len() >= self.max_pages {
                break;
            }
            seen.push(u.clone());
            let res = self.get(&u, "", 1)?;
            let html = Html::parse(&res.body);
            let ev = Eval::new(&html, &res.body, &res.url, "", 1).with_host(self.host(
                res.body.clone(),
                &res.url,
                "",
                1,
            ));
            let rule = &self.source.content;
            // 正文要先按 HTML 洗一遍：content 规则常常取的是 `@html` / `@all`，
            // 拿到的是带标签的片段。洗完之后再反转义（顺序不能反）。
            let raw = ev.strings_ex(None, &rule.content, false, false)?.join(
                "
",
            );
            let text = unescape_entities(&crate::html::format_text(&raw));
            if !text.trim().is_empty() {
                parts.push(text);
            }
            if !rule.next_content_url.trim().is_empty() {
                next = ev
                    .strings(None, &rule.next_content_url, true)?
                    .into_iter()
                    .find(|n| !seen.contains(n));
            }
        }
        if parts.is_empty() {
            return Err(Error::Missing("正文（content 规则没匹配到东西）".into()));
        }
        Ok(parts.join("\n"))
    }

    /// URL 上下文里的 JS 环境。
    ///
    /// 宿主没有「当前内容」，所以 `java.getString` 一律给空串 —— legado 在 URL
    /// 上下文里绑的是 `AnalyzeUrl`，它本来就没有 `getString`。
    fn url_js(&self, key: &str, page: u32) -> Option<Rc<Js>> {
        let fetcher = self.fetch.clone();
        let host = Rc::new(RuleHost {
            ajax: Rc::new(move |url: &str| match fetcher.get(url) {
                Ok(r) => r.body,
                Err(_) => String::new(),
            }),
            source: self.source.clone(),
            key: key.to_string(),
            page,
            content: RefCell::new(String::new()),
            base: RefCell::new(self.source.url.clone()),
            vars: self.vars.clone(),
            book_name: self.book_name.clone(),
            book_author: self.book_author.clone(),
            chapter_title: RefCell::new(String::new()),
            chapter_url: RefCell::new(String::new()),
            chapter_index: RefCell::new(0),
        });
        let b = Bindings {
            page,
            key: key.to_string(),
            base_url: self.source.url.clone(),
            source_url: self.source.url.clone(),
            source_name: self.source.name.clone(),
            source_comment: self.source.comment.clone().unwrap_or_default(),
            book_name: self.book_name.clone(),
            book_author: self.book_author.clone(),
            ..Default::default()
        };
        Js::new(host, &b).ok().map(Rc::new)
    }

    fn get(&self, spec: &str, key: &str, page: u32) -> R<net::Response> {
        // URL 模板里的 `{{ }}`：先走算术，算不出来才起 JS
        //（真书源里会写 `{{java.ajax(...)}}` 或 `{{source.getKey()}}`）。
        let js = self.url_js(key, page);
        let expanded = rule::expand_braces(
            spec,
            &Env {
                key: key.to_string(),
                page,
                js,
                rule_eval: None,
            },
        )?;
        // 书源里的 URL 常写成相对路径（`/tag/?key={{key}}`），基准是 bookSourceUrl。
        // 选项要先切下来再拼绝对地址，否则 `,{json}` 会被当成路径的一部分。
        let (bare, opt) = net::split_url_option(&expanded);
        let mut abs = net::absolute(&self.source.url, &bare);
        if let Some(opt) = opt {
            abs.push(',');
            abs.push_str(&opt);
        }
        self.fetch.get(&abs)
    }
}

/// 规则求值的上下文：一份 DOM + 求值起点 + 相对链接的基准 + 可选的 JS 环境。
struct Eval<'a> {
    html: &'a Html<'a>,
    /// 抓回来的原始正文。
    ///
    /// 裸 `@js:` 脚本拿到的 `result` 是它（legado 的管线 `result = content` 起步）。
    /// 不能拿 `html.raw()` 顶替 —— 那是重新序列化过的 DOM，对 JSON 正文更是
    /// 会把它裹进 `<html><body>`。
    body: &'a str,
    /// 正文解析出来的 JSON（解析不了就是 `None`）。只有 JSON 源用得上。
    ///
    /// 持有所有权而不是借用：解析结果得比 `new()` 活得久，而 `Eval` 是
    /// 在这一层现造现用的。
    json: Option<serde_json::Value>,
    /// 相对链接的基准 —— 是**重定向之后**的 URL
    base: &'a str,
    key: &'a str,
    page: u32,
    /// 有宿主就能跑 JS。纯离线测试和「嵌套求值」都没有。
    host: Option<Rc<RuleHost>>,
    /// 懒建：一页可能用到十几条规则，但大部分页面一条 JS 都没有。
    js: RefCell<Option<Rc<Js>>>,
}

impl<'a> Eval<'a> {
    fn new(html: &'a Html<'a>, body: &'a str, base: &'a str, key: &'a str, page: u32) -> Self {
        Self {
            html,
            // 只有真的能当 JSON 解析才存下来。HTML 页面解析会失败，正好是 None
            json: serde_json::from_str::<serde_json::Value>(body).ok(),
            body,
            base,
            key,
            page,
            host: None,
            js: RefCell::new(None),
        }
    }

    fn with_host(mut self, host: Rc<RuleHost>) -> Self {
        self.host = Some(host);
        self
    }

    /// 告诉求值环境「现在在拼哪一章」。
    ///
    /// 真书源里 `chapterUrl` 的脚本经常引用 `chapter.title`（上一段刚算出来的），
    /// 所以算完标题就要在算 URL 之前把它设进来。
    /// 顺带把 JS 环境丢掉重建 —— 那些绑定是装配时快照进去的，不重建就是旧值。
    fn set_chapter(&self, title: &str, url: &str, index: usize) {
        if let Some(h) = &self.host {
            *h.chapter_title.borrow_mut() = title.to_string();
            *h.chapter_url.borrow_mut() = url.to_string();
            *h.chapter_index.borrow_mut() = index;
        }
        *self.js.borrow_mut() = None;
    }

    /// 组装求值环境。`rule_eval` 只在规则里真的有 `{{ }}` 时才建 ——
    /// 它要克隆整页 HTML，普通规则不该付这个代价。
    /// 求值环境。`cur` 是**当前起点** —— `{{$.id}}` 里的内嵌规则要对着它求值，
    /// 不是对着整篇文档。
    ///
    /// 这一点很要命：JSON API 源的 `bookUrl` 常写成
    /// `http://api.x/book/{{$.id}}/info`，而 `$.id` 是**这个搜索结果元素**的
    /// id。原来这里写死 `None`（= 整篇正文），于是永远取到空 ——
    /// 表现是「搜索有结果，点进去打不开」。
    ///
    /// 闭包要 `'static`（`RuleEval` 没带生命周期），所以 JSON 起点只能克隆一份
    /// 进去。HTML 起点记的是下标，配上原始正文重新解析，下标对得上。
    fn env(&self, raw: &str, cur: Option<&Node>) -> Env {
        let mut env = Env {
            key: self.key.to_string(),
            page: self.page,
            js: self.js(),
            rule_eval: None,
        };
        if raw.contains("{{") {
            // 用**原始正文**而不是 `html.raw()`：后者是重新序列化过的 DOM，
            // 拿它当 JS 的 result 和重新解析的基准都会偏
            let content = self.body.to_string();
            let base = self.base.to_string();
            let key = self.key.to_string();
            let page = self.page;
            let start: Option<serde_json::Value> = match cur {
                Some(Node::Json(v)) => Some((**v).clone()),
                _ => None,
            };
            let el: Option<usize> = match cur {
                Some(Node::El(i)) => Some(*i),
                _ => None,
            };
            env.rule_eval = Some(Rc::new(move |r: &str| {
                let html = Html::parse(&content);
                let ev = Eval::new(&html, &content, &base, &key, page);
                let node = start
                    .as_ref()
                    .map(|v| Node::Json(Box::new(v.clone())))
                    .or_else(|| el.map(Node::El));
                ev.strings(node.as_ref(), r, false).ok().map(|v| {
                    v.join(
                        "
",
                    )
                })
            }));
        }
        env
    }

    /// 拿到（必要时先建）JS 环境。没有宿主就返回 `None`，
    /// 于是 `@js:` 规则会报 `Unsupported` 而不是静默取空。
    fn js(&self) -> Option<Rc<Js>> {
        if let Some(j) = self.js.borrow().as_ref() {
            return Some(j.clone());
        }
        let host = self.host.clone()?;
        let b = Bindings {
            page: self.page,
            key: self.key.to_string(),
            base_url: self.base.to_string(),
            source_url: host.source.url.clone(),
            source_name: host.source.name.clone(),
            source_comment: host.source.comment.clone().unwrap_or_default(),
            book_name: host.book_name.clone(),
            book_author: host.book_author.clone(),
            src: self.html.raw(),
            result: String::new(),
            chapter_title: host.chapter_title.borrow().clone(),
            chapter_url: host.chapter_url.borrow().clone(),
            chapter_index: *host.chapter_index.borrow(),
        };
        let js = Rc::new(Js::new(host, &b).ok()?);
        *self.js.borrow_mut() = Some(js.clone());
        Some(js)
    }

    /// 这条规则实际该按哪种模式解。
    ///
    /// legado 的 `SourceRule` 里有个 `isJSON` 标志：**正文是 JSON 时，所有规则
    /// 默认都当 JSON 路径**（所以真书源里有 `chapterList: "data"` 这种裸键名，
    /// 不带 `$.`）。`@css:` 和 `@@` 是显式逃出去写选择器的两个口子，优先级更高。
    fn effective_mode(&self, r: &Rule) -> Mode {
        if r.css || r.escaped {
            Mode::Default
        } else if self.json.is_some() {
            Mode::Json
        } else {
            r.mode
        }
    }

    /// 取一串字符串。`is_url` 为真时按基准拼成绝对链接并去重（legado 也只在
    /// URL 场景去重）。
    fn strings(&self, cur: Option<&Node>, raw: &str, is_url: bool) -> R<Vec<String>> {
        self.strings_ex(cur, raw, is_url, true)
    }

    /// `unescape = false` 是正文那条路要的：legado 先不反转义、把 HTML 洗成文本，
    /// 洗完再反转义。反了的话 `&lt;div&gt;` 会先变成真标签再被当成标签删掉。
    fn strings_ex(
        &self,
        cur: Option<&Node>,
        raw: &str,
        is_url: bool,
        unescape: bool,
    ) -> R<Vec<String>> {
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        let r = Rule::parse_env(raw, &self.env(raw, cur))?;
        // 规则展开之后就是个绝对地址 —— 源作者写的就是这个地址本身，没有
        // 选择器/路径可套。`bookUrl: "http://api.x/book/{{$.id}}/info"` 这种
        // 写法很常见（223 条 bookUrl 里占相当一部分）。
        //
        // legado 会把它当 CSS 选择器查一遍、最后取到**空**（它自己的 bug）。
        // 我们按作者显然的意图返回地址本身。判据用「以 URL scheme 开头」而不是
        // 「像不像选择器」—— 没有哪个 CSS 选择器会以 `http://` 开头，不会误判。
        let raw_values = if let Some(url) = literal_url(&r.text) {
            // 尾巴上的 `@js:` 还是要跑，`result` 就是地址本身
            match &r.js {
                Some(script) => self
                    .js()
                    .ok_or_else(|| Error::Unsupported(format!("@js:{script}")))?
                    .eval(script, Some(&url))?
                    .to_list(),
                None => vec![url],
            }
        } else if self.effective_mode(&r) == Mode::Json {
            self.json_strings(cur, &r)?
        } else if self.effective_mode(&r) == Mode::XPath {
            self.xpath_strings(cur, &r)?
        } else {
            self.html_strings(cur, &r)?
        };
        let mut out: Vec<String> = Vec::new();
        for s in raw_values {
            let s = r.apply_replace(&s);
            if is_url {
                let u = net::absolute(self.base, &s);
                if !u.is_empty() && !out.contains(&u) {
                    out.push(u);
                }
            } else {
                out.push(s);
            }
        }
        if is_url || !unescape {
            Ok(out)
        } else {
            Ok(out.into_iter().map(|s| unescape_entities(&s)).collect())
        }
    }

    fn string(&self, cur: Option<&Node>, raw: &str) -> R<String> {
        Ok(self.strings(cur, raw, false)?.join("\n"))
    }

    fn string_url(&self, cur: Option<&Node>, raw: &str) -> R<String> {
        Ok(self
            .strings(cur, raw, true)?
            .into_iter()
            .next()
            .unwrap_or_default())
    }

    /// 取一堆元素（`bookList` / `chapterList` 用）。
    fn elements(&self, raw: &str) -> R<Vec<Node>> {
        if raw.trim().is_empty() {
            return Err(Error::Missing(
                "列表规则（bookList / chapterList 是空的）".into(),
            ));
        }
        // 列表规则总是从整篇文档起步
        let r = Rule::parse_env(raw, &self.env(raw, None))?;
        if self.effective_mode(&r) == Mode::Json {
            // legado 这里是 `read<ArrayList>`：不定路径给匹配列表，定路径指着
            // 数组就把它自己当列表，指着标量则转 List 失败 → 空
            let root = self.json_root(None)?;
            let (parts, sep) = rule::split_rule(&r.text, &["&&", "||", "%%"]);
            let mut all: Vec<Vec<Node>> = Vec::new();
            for part in &parts {
                let got: Vec<Node> = crate::jsonpath::parse(part)?
                    .eval_elements(root)
                    .into_iter()
                    .map(|v| Node::Json(Box::new(v)))
                    .collect();
                if !got.is_empty() {
                    all.push(got);
                    if sep.as_deref() == Some("||") {
                        break;
                    }
                }
            }
            return Ok(combine(all, sep.as_deref()));
        }
        if self.effective_mode(&r) == Mode::XPath {
            return self.xpath_elements(&r.text);
        }
        let (parts, sep) = rule::split_rule(&r.text, &["&&", "||", "%%"]);
        let mut results: Vec<Vec<Option<usize>>> = Vec::new();
        for part in &parts {
            let got = if r.css {
                self.html
                    .select(None, part.trim())?
                    .into_iter()
                    .map(Some)
                    .collect()
            } else {
                self.step_chain(None, part)?
            };
            if !got.is_empty() {
                results.push(got);
                if sep.as_deref() == Some("||") {
                    break;
                }
            }
        }
        Ok(combine(results, sep.as_deref())
            .into_iter()
            .flatten()
            .map(Node::El)
            .collect())
    }

    /// 取 JSON 求值的根：有起点就用起点，否则用整篇正文。
    fn json_root<'b>(&'b self, cur: Option<&'b Node>) -> R<&'b serde_json::Value> {
        match cur {
            Some(Node::Json(v)) => Ok(v.as_ref()),
            // JSON 规则套在 HTML 元素上：源写错了，明说
            Some(Node::El(_)) => Err(Error::Unsupported(
                "JSON 路径规则不能接在 HTML 选择器后面".into(),
            )),
            None => self.json.as_ref().ok_or_else(|| {
                Error::Unsupported("这条规则是 JSON 路径，但抓回来的正文不是 JSON".into())
            }),
        }
    }

    /// XPath 模式的取值。和 [`Self::html_strings`] 一一对应。
    ///
    /// 尾部取值器（`/@content`、`/text()`）拆出来交给 `Html::accessor` ——
    /// 和 CSS 那条路共用同一套取值逻辑，两边不会跑偏。
    fn xpath_strings(&self, cur: Option<&Node>, r: &Rule) -> R<Vec<String>> {
        let (parts, sep) = rule::split_rule(&r.text, &["&&", "||", "%%"]);
        let mut results: Vec<Vec<String>> = Vec::new();
        for part in &parts {
            let (expr, acc) = split_xpath_accessor(part.trim());
            let xp = crate::xpath::parse(&expr)?;
            // `/` 开头是**绝对路径**，起点永远是文档，不是当前元素。
            // （`//meta[@property='og:image']/@content` 写在 `bookUrl` 那种
            //  逐元素求值的字段里时，写死当前元素就一条都取不到。）
            let root = if expr.trim_start().starts_with('/') {
                None
            } else {
                el_of(cur, part)?
            };
            let ids = xp.select(self.html, root)?;
            let els: Vec<Option<usize>> = ids.into_iter().map(Some).collect();
            let got = self.html.accessor(&els, &acc);
            if !got.is_empty() {
                results.push(got);
                if sep.as_deref() == Some("||") {
                    break;
                }
            }
        }
        let mut out = combine(results, sep.as_deref());
        if let Some(script) = &r.js {
            let Some(js) = self.js() else {
                return Err(Error::Unsupported(format!("@js:{script}")));
            };
            let seed = if r.text.trim().is_empty() {
                self.body.to_string()
            } else {
                out.join(
                    "
",
                )
            };
            out = js.eval(script, Some(&seed))?.to_list();
        }
        Ok(out)
    }

    /// XPath 的**元素**（`bookList` / `chapterList` 用）。
    fn xpath_elements(&self, raw: &str) -> R<Vec<Node>> {
        let mut out: Vec<Node> = Vec::new();
        let (parts, _) = rule::split_rule(raw.trim(), &["&&", "||", "%%"]);
        for part in &parts {
            // 元素提取时**不拆取值器** —— `//div/dd/a` 整个是路径
            let xp = crate::xpath::parse(part.trim())?;
            for i in xp.select(self.html, None)? {
                out.push(Node::El(i));
            }
        }
        Ok(out)
    }

    /// JSON 模式的取值。和 [`Self::html_strings`] 一一对应。
    fn json_strings(&self, cur: Option<&Node>, r: &Rule) -> R<Vec<String>> {
        let root = self.json_root(cur)?;
        let (parts, sep) = rule::split_rule(&r.text, &["&&", "||", "%%"]);
        let mut results: Vec<Vec<String>> = Vec::new();
        let mut puts: Vec<(String, String)> = Vec::new();
        for part in &parts {
            let (path, opts) = split_json_options(part)?;
            let vals: Vec<String> = crate::jsonpath::parse(&path)?
                .eval(root)
                .iter()
                .map(crate::jsonpath::value_to_string)
                .collect();
            // `@put:{k:v}` —— 存进书源变量，后面的规则用 `{{k}}` / `@get:{k}` 取。
            // （17k 那种「搜索拿到 id、再拼目录 URL」的 API 源全靠它。）
            //
            // 取值部分**是规则，不是字面量** —— legado 是 `put(key, getString(value))`。
            // 所以 `@put:{book:id}` 是「把本元素的 id 存成 book」，而
            // `@put:{u:class.x@href}` 是「把那个链接存成 u」。
            for (k, v) in opts {
                let val = if v.trim().is_empty() {
                    vals.first().cloned().unwrap_or_default()
                } else {
                    self.strings(cur, &v, false)
                        .unwrap_or_default()
                        .into_iter()
                        .next()
                        .unwrap_or_default()
                };
                puts.push((k, val));
            }
            if !vals.is_empty() {
                results.push(vals);
                if sep.as_deref() == Some("||") {
                    break;
                }
            }
        }
        if let (Some(host), false) = (&self.host, puts.is_empty()) {
            let mut vars = host.vars.borrow_mut();
            for (k, v) in puts {
                vars.insert(k, v);
            }
        }
        let mut out = combine(results, sep.as_deref());
        // 尾巴上的 `@js:`：和 HTML 那条路同一套规矩
        if let Some(script) = &r.js {
            let Some(js) = self.js() else {
                return Err(Error::Unsupported(format!("@js:{script}")));
            };
            let seed = if r.text.trim().is_empty() {
                self.body.to_string()
            } else {
                out.join("\n")
            };
            out = js.eval(script, Some(&seed))?.to_list();
        }
        Ok(out)
    }

    fn html_strings(&self, cur: Option<&Node>, r: &Rule) -> R<Vec<String>> {
        let (parts, sep) = rule::split_rule(&r.text, &["&&", "||", "%%"]);
        let mut results: Vec<Vec<String>> = Vec::new();
        for part in &parts {
            let got = if r.css {
                self.css_result(cur, part)?
            } else {
                self.result_list(cur, part)?
            };
            if !got.is_empty() {
                results.push(got);
                if sep.as_deref() == Some("||") {
                    break;
                }
            }
        }
        let mut out = combine(results, sep.as_deref());
        // 尾巴上的 `@js:`：把前面算出来的结果当 `result` 喂给脚本，
        // 脚本的输出才是最终结果（legado 的 splitSourceRule 就是这个顺序）。
        if let Some(script) = &r.js {
            let Some(js) = self.js() else {
                return Err(Error::Unsupported(format!("@js:{script}")));
            };
            // legado 的管线是 `result = content` 起步的，所以**裸 `@js:` 脚本拿到的
            // `result` 是整页 HTML**（`@js:result.match(/id="(\d+)"/)[1]` 这种写法
            // 非常常见）。只有当 `@js:` 前面还有一段规则时，`result` 才是那段的输出。
            // legado 的管线是 `result = content` 起步的，所以**裸 `@js:` 脚本拿到的
            // `result` 是整页正文**（`@js:result.match(/id="(\d+)"/)[1]` 这种写法非常
            // 常见）。只有 `@js:` 前面还有一段规则时，`result` 才是那段的输出。
            let seed = if r.text.trim().is_empty() {
                self.body.to_string()
            } else {
                out.join("\n")
            };
            let v = js.eval(script, Some(&seed))?;
            out = v.to_list();
        }
        Ok(out)
    }

    /// `@css:` 规则：整段是选择器，最后一个 `@` 后面是取值器。
    fn css_result(&self, cur: Option<&Node>, piece: &str) -> R<Vec<String>> {
        let piece = piece.trim();
        if piece.is_empty() {
            return Ok(Vec::new());
        }
        let (sel, acc) = match piece.rfind('@') {
            Some(i) => (&piece[..i], &piece[i + 1..]),
            // legado 这里会抛异常；我们宽容一点，当成取文本
            None => (piece, "text"),
        };
        let els: Vec<Option<usize>> = self
            .html
            .select(el_of(cur, piece)?, sel)?
            .into_iter()
            .map(Some)
            .collect();
        Ok(self.html.accessor(&els, acc))
    }

    /// 默认方言：`a@b@c` 里最后一段是取值器，前面都是逐级收窄。
    fn result_list(&self, cur: Option<&Node>, piece: &str) -> R<Vec<String>> {
        let piece = piece.trim().trim_start_matches(['@', ' ', '\t']);
        if piece.is_empty() {
            return Ok(Vec::new());
        }
        let (steps, _) = rule::split_rule(piece, &["@"]);
        let (last, rest) = steps.split_last().expect("至少一段");
        let mut els: Vec<Option<usize>> = vec![el_of(cur, piece)?];
        for step in rest {
            els = self.step_one(&els, step)?;
        }
        Ok(self.html.accessor(&els, last.trim()))
    }

    /// `getElements` 的路径：**没有取值器**，每一段都是逐级收窄。
    fn step_chain(&self, cur: Option<&Node>, piece: &str) -> R<Vec<Option<usize>>> {
        let piece = piece.trim().trim_start_matches(['@', ' ', '\t']);
        if piece.is_empty() {
            return Ok(Vec::new());
        }
        let (steps, _) = rule::split_rule(piece, &["@"]);
        let mut els: Vec<Option<usize>> = vec![el_of(cur, piece)?];
        for step in &steps {
            els = self.step_one(&els, step)?;
        }
        Ok(els)
    }

    fn step_one(&self, els: &[Option<usize>], step: &str) -> R<Vec<Option<usize>>> {
        self.html.step(els, step)
    }
}

/// 把 XPath 尾部的取值器拆出来（`/@content`、`/text()`）。
///
/// 返回 `(路径, 取值器)`。没有取值器时按 `text` 算 —— 和 CSS 那条路一致。
fn split_xpath_accessor(s: &str) -> (String, String) {
    let t = s.trim();
    // 注意 XPath 里写 `text()`，而 `Html::accessor` 认的是 `text` ——
    // 直接把 `text()` 传过去会掉进「按属性名取值」那一支，结果是空
    for (xp, acc) in [
        ("text()", "text"),
        ("textNodes", "textNodes"),
        ("ownText", "ownText"),
        ("html", "html"),
        ("all", "all"),
    ] {
        if let Some(head) = t.strip_suffix(xp) {
            if let Some(path) = head.strip_suffix('/') {
                return (path.trim().to_string(), acc.to_string());
            }
        }
    }
    // `/@name`
    if let Some(i) = t.rfind("/@") {
        let name = &t[i + 2..];
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == ':')
        {
            return (t[..i].to_string(), name.to_string());
        }
    }
    (t.to_string(), "text".to_string())
}

/// 规则文本本身是不是一个绝对地址。
///
/// `//` 开头要**格外小心**：那既是协议相对地址（`//cdn.x.com/a.jpg`），
/// 也是最高频的 XPath 前缀（`//div`、`//*[@id="list"]`）。
/// 一开始两种都收，结果**每条 `//` 开头的 XPath 都被当成地址原样返回**了 ——
/// 表现是「规则看着生效了，值却是那串选择器本身」。所以这里要求
/// `//` 后面得像个主机名：没有 XPath 元字符、且带点。
fn literal_url(text: &str) -> Option<String> {
    let t = text.trim();
    if t.chars().any(char::is_whitespace) {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(t.to_string());
    }
    let looks_like_host = lower.starts_with("//")
        && !t.contains(['[', '@', '(', ')', '*', '!', '|'])
        && t[2..].contains('.');
    looks_like_host.then(|| t.to_string())
}

/// 把 JSON 规则拆成「路径 + `@` 选项」。
///
/// 真书源里写成 `$.bookName@put:{book:id}`。不分出来的话 `@put:{book:id}`
/// 会被当成键名的一部分 —— 于是路径永远匹配不上，**静默取空**。
/// （这个 bug 就是这么发现的：17k 小说的搜索规则一条都取不到值。）
///
/// 返回 `(路径, [(变量名, 取值规则)])`。
fn split_json_options(part: &str) -> R<(String, Vec<(String, String)>)> {
    let (segs, _) = rule::split_rule(part, &["@"]);
    let mut it = segs.into_iter();
    let path = it.next().unwrap_or_default().trim().to_string();
    let mut puts = Vec::new();
    for seg in it {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let Some(body) = seg
            .strip_prefix("put:")
            .or_else(|| seg.strip_prefix("PUT:"))
        else {
            // 不认识的选项宁可报错：静默忽略的下场是「规则看着生效了，
            // 实际少了东西」，比报错难查
            return Err(Error::Unsupported(format!(
                "JSON 规则 `{part}` 里的 `@{seg}` 选项"
            )));
        };
        for kv in body.trim().trim_matches(['{', '}']).split(',') {
            let kv = kv.trim();
            if kv.is_empty() {
                continue;
            }
            match kv.split_once(':') {
                Some((k, v)) => puts.push((k.trim().to_string(), v.trim().to_string())),
                None => {
                    return Err(Error::Unsupported(format!(
                        "`@put:` 里的 `{kv}`（要写成 名字:取值）"
                    )))
                }
            }
        }
    }
    Ok((path, puts))
}

/// 把一个求值起点当成 HTML 元素下标。
///
/// `None` 表示「整篇文档」。JSON 起点配 CSS 规则是源写错了 —— 明说，
/// 而不是当成「整篇文档」悄悄给出别的东西。
fn el_of(cur: Option<&Node>, rule: &str) -> R<Option<usize>> {
    match cur {
        None => Ok(None),
        Some(Node::El(i)) => Ok(Some(*i)),
        // 报错要说是**哪条**规则：不然几百条规则里根本找不着
        Some(Node::Json(_)) => Err(Error::Unsupported(format!(
            "HTML 选择器 `{rule}` 接在了 JSON 路径后面"
        ))),
    }
}

/// `&&` 是首尾相接，`||` 是取第一个非空的，`%%` 是交错轮取。
fn combine<T: Clone>(results: Vec<Vec<T>>, sep: Option<&str>) -> Vec<T> {
    match sep {
        Some("%%") => {
            let mut out = Vec::new();
            let Some(n) = results.first().map(Vec::len) else {
                return out;
            };
            for i in 0..n {
                for r in &results {
                    if let Some(v) = r.get(i) {
                        out.push(v.clone());
                    }
                }
            }
            out
        }
        _ => results.into_iter().flatten().collect(),
    }
}

/// `java.*` 背后的实现。
///
/// **必须拥有全部状态**（不能借用 `Crawler`）：`Js::new` 收的是
/// `Rc<dyn Host>`，要求 `'static`。
struct RuleHost {
    ajax: Rc<dyn Fn(&str) -> String>,
    source: BookSource,
    key: String,
    page: u32,
    /// 当前内容。`java.setContent` 会换掉它。
    content: RefCell<String>,
    base: RefCell<String>,
    vars: Rc<RefCell<HashMap<String, String>>>,
    book_name: String,
    book_author: String,
    /// 正在拼的那一章（`@js:` 里常写 `chapter.title`）
    chapter_title: RefCell<String>,
    chapter_url: RefCell<String>,
    chapter_index: RefCell<usize>,
}

impl RuleHost {
    /// 在当前内容上跑一条 legado 规则。
    ///
    /// 刻意**不带 JS 环境**：`java.getString("@@class.intro@text")` 这种是纯 HTML
    /// 规则，占绝大多数；嵌套规则里再套 `@js:` 会递归起 QuickJS 上下文，
    /// 收益很小而代价和复杂度都不小。那种情况返回空串。
    fn run(&self, rule: &str, mode: JsMode) -> String {
        let content = self.content.borrow().clone();
        let base = self.base.borrow().clone();
        let html = Html::parse(&content);
        let ev = Eval::new(&html, &content, &base, &self.key, self.page);
        let got = match mode {
            JsMode::Html => ev.step_chain(None, rule).map(|els| {
                els.iter()
                    .filter_map(|e| e.map(|i| html.outer_html(i)))
                    .collect::<Vec<_>>()
            }),
            _ => ev.strings(None, rule, false),
        };
        match got {
            Ok(v) => v.join(
                "
",
            ),
            Err(_) => String::new(),
        }
    }
}

impl Host for RuleHost {
    fn ajax(&self, url: &str) -> String {
        (self.ajax)(url)
    }

    fn set_content(&self, html: &str) {
        *self.content.borrow_mut() = html.to_string();
    }

    fn rule(&self, rule: &str, mode: JsMode) -> String {
        self.run(rule, mode)
    }

    fn put(&self, key: &str, value: &str) {
        self.vars
            .borrow_mut()
            .insert(key.to_string(), value.to_string());
    }

    fn get(&self, key: &str) -> String {
        self.vars.borrow().get(key).cloned().unwrap_or_default()
    }

    fn html_select(&self, html: &str, selector: &str) -> String {
        let doc = Html::parse(html);
        doc.select(None, selector)
            .map(|els| {
                els.iter()
                    .map(|&i| doc.outer_html(i))
                    .collect::<Vec<_>>()
                    .join(
                        "
",
                    )
            })
            .unwrap_or_default()
    }

    fn html_text(&self, html: &str) -> String {
        let doc = Html::parse(html);
        doc.subtree(None)
            .first()
            .map(|&i| doc.text(i))
            .unwrap_or_default()
    }

    fn html_attr(&self, html: &str, name: &str) -> String {
        let doc = Html::parse(html);
        match doc.subtree(None).first() {
            Some(&i) => doc.attr(i, name),
            None => String::new(),
        }
    }

    fn html_remove(&self, html: &str, selector: &str) -> String {
        // 简化：把匹配到的片段整段删掉。真书源里 `remove()` 用来去掉广告节点。
        let doc = Html::parse(html);
        let mut out = html.to_string();
        if let Ok(els) = doc.select(None, selector) {
            for i in els {
                out = out.replace(&doc.outer_html(i), "");
            }
        }
        out
    }
}

/// legado 每次 `getString` 都会做一遍 HTML 实体反转义（`&nbsp;` 在中文站里到处都是）。
fn unescape_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'&' {
            let step = s[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&s[i..i + step]);
            i += step;
            continue;
        }
        let Some(end) = s[i..].find(';').filter(|e| *e <= 10) else {
            out.push('&');
            i += 1;
            continue;
        };
        let body = &s[i + 1..i + end];
        let rep = match body {
            "nbsp" => Some('\u{a0}'),
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ldquo" => Some('“'),
            "rdquo" => Some('”'),
            _ => body
                .strip_prefix('#')
                .and_then(|n| {
                    if let Some(hex) = n.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        n.parse::<u32>().ok()
                    }
                })
                .and_then(char::from_u32),
        };
        match rep {
            Some(c) => {
                out.push(c);
                i += end + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const HOME: &str = include_str!("../tests/fixtures/daomubiji-home.html");
    const BOOK: &str = include_str!("../tests/fixtures/daomubiji-book.html");
    const CHAPTER: &str = include_str!("../tests/fixtures/daomubiji-chapter.html");
    const SOURCES: &str = include_str!("../tests/fixtures/sources.json");

    /// 录好的页面。找不到的 URL 会失败 —— 那是测试没录全，不是「返回空」。
    #[derive(Clone)]
    struct Recorded(HashMap<String, String>);

    impl Fetch for Recorded {
        fn get(&self, spec: &str) -> R<net::Response> {
            let (url, _) = net::split_url_option(spec);
            match self.0.get(&url) {
                Some(body) => Ok(net::Response {
                    url,
                    body: body.clone(),
                }),
                None => Err(Error::Net(format!("测试里没录这个页面：{url}"))),
            }
        }
    }

    fn daomubiji() -> BookSource {
        BookSource::parse_file(SOURCES)
            .unwrap()
            .into_iter()
            .find(|s| s.name == "盗墓笔记" && s.url == "http://www.daomubiji.com/")
            .expect("夹具里应当有这个源")
    }

    fn recorded() -> Recorded {
        let mut m = HashMap::new();
        m.insert("http://www.daomubiji.com/".to_string(), HOME.to_string());
        m.insert(
            "http://www.daomubiji.com/dao-mu-bi-ji-1".to_string(),
            BOOK.to_string(),
        );
        m.insert(
            "http://www.daomubiji.com/qi-xing-lu-wang-01.html".to_string(),
            CHAPTER.to_string(),
        );
        Recorded(m)
    }

    fn crawler() -> Crawler<Recorded> {
        Crawler::new(daomubiji(), recorded())
    }

    #[test]
    fn the_fixture_has_the_rules_we_think_it_has() {
        let s = daomubiji();
        assert_eq!(s.toc.chapter_list, "class.excerpts-wrapper@tag.a");
        assert_eq!(s.content.content, "class.article-content@tag.p@textNodes");
        assert_eq!(s.search.book_list, "class.container@tag.li!0");
        assert!(!s.can_search());
    }

    #[test]
    fn toc_from_the_book_page() {
        let c = crawler();
        let toc = c.toc("http://www.daomubiji.com/dao-mu-bi-ji-1").unwrap();
        assert_eq!(toc.len(), 75, "这个页面把全书的章节都列出来了");
        assert_eq!(toc[0].title, "七星鲁王 第一章 血尸");
        assert_eq!(
            toc[0].url,
            "http://www.daomubiji.com/qi-xing-lu-wang-01.html"
        );
        assert_eq!(toc[1].title, "七星鲁王 第二章 五十年后");
        // 每一章的链接都应当是绝对 URL
        assert!(toc.iter().all(|c| c.url.starts_with("http://")));
    }

    #[test]
    fn content_from_a_chapter_page() {
        let c = crawler();
        let text = c
            .content("http://www.daomubiji.com/qi-xing-lu-wang-01.html")
            .unwrap();
        assert!(text.starts_with("50年前，长沙镖子岭。"), "{text:.60}");
        assert!(text.contains("血尸"));
        assert!(text.contains('\n'), "多个 <p> 应当用换行接起来");
        // 不能把页面上的导航/广告一起吃进来
        assert!(!text.contains("<script"), "{text:.200}");
        assert!(!text.contains("adsbygoogle"));
        // 正文的每一行都不应该带标签
        assert!(text.lines().all(|l| !l.contains('<')), "{text:.200}");
    }

    #[test]
    fn search_reads_the_nav_as_books() {
        // 这个源的 searchUrl 是 `-`，但它把站点导航当成「书架」。
        // 我们按 legado 的语义拒绝搜索，但 bookList 规则本身应当能解出东西。
        let c = crawler();
        let res = net::Response {
            url: "http://www.daomubiji.com/".into(),
            body: HOME.into(),
        };
        let html = Html::parse(&res.body);
        let ev = Eval::new(&html, &res.body, &res.url, "", 1);
        let items = ev.elements("class.container@tag.li!0").unwrap();
        assert!(items.len() > 5, "应当解出若干个 li");
        let names = ev
            .strings(None, "class.menu-item@tag.a.0@text", false)
            .unwrap();
        assert!(names.iter().any(|n| n.contains("盗墓笔记")), "{names:?}");
        // 但搜索本身要明确失败，不能悄悄返回空
        assert!(matches!(c.search("盗墓", 1), Err(Error::Missing(_))));
    }

    #[test]
    fn info_reads_name_and_intro() {
        let c = crawler();
        let info = c.info("http://www.daomubiji.com/dao-mu-bi-ji-1").unwrap();
        assert!(info.name.contains("七星鲁王"), "{:?}", info.name);
        assert!(info.intro.contains("盗墓笔记"), "{:?}", info.intro);
    }

    /// JSON 路径还没实现，要明确报错而不是静默取空。
    #[test]
    fn json_rules_are_refused() {
        let mut s = daomubiji();
        s.toc.chapter_list = "$.data.list".into();
        let c = Crawler::new(s, recorded());
        assert!(matches!(
            c.toc("http://www.daomubiji.com/dao-mu-bi-ji-1"),
            Err(Error::Unsupported(_))
        ));
    }

    /// `@js:` 真的接通了：脚本能拿到 `result`，输出会变成最终结果。
    #[test]
    fn js_rules_actually_run_on_real_pages() {
        // `@js:` 前面还有规则时，`result` 是那段的输出
        let mut s = daomubiji();
        s.toc.chapter_name = "tag.a@text@js:result.slice(0,4)+'!'".into();
        let c = Crawler::new(s, recorded());
        let toc = c.toc("http://www.daomubiji.com/dao-mu-bi-ji-1").unwrap();
        assert!(!toc.is_empty());
        assert_eq!(
            toc[0].title, "七星鲁王!",
            "脚本应当把标题截成前 4 字加感叹号"
        );
        assert!(toc.iter().all(|ch| ch.title.ends_with('!')));

        // 裸 `@js:`（前面没有规则）拿到的 `result` 是**整页 HTML** ——
        // 这是 legado 的管线决定的，`@js:result.match(...)` 全靠它
        let mut s = daomubiji();
        s.toc.chapter_name = "@js:String(result.length)".into();
        let c = Crawler::new(s, recorded());
        let toc = c.toc("http://www.daomubiji.com/dao-mu-bi-ji-1").unwrap();
        let used: usize = toc[0].title.parse().unwrap();
        assert!(
            used > 10_000,
            "裸 @js: 应当拿到整页 HTML，实测只有 {used} 字节"
        );

        // `chapter.title` 在算 chapterUrl 时可用（真书源里很常见）
        let mut s = daomubiji();
        s.toc.chapter_url = "tag.a@href@js:'http://cdn/'+encodeURIComponent(chapter.title)".into();
        let c = Crawler::new(s, recorded());
        let toc = c.toc("http://www.daomubiji.com/dao-mu-bi-ji-1").unwrap();
        assert!(
            toc[0].url.starts_with("http://cdn/%E4%B8%83%E6%98%9F"),
            "{}",
            toc[0].url
        );
        assert_eq!(toc[0].url, "http://cdn/%E4%B8%83%E6%98%9F%E9%B2%81%E7%8E%8B%20%E7%AC%AC%E4%B8%80%E7%AB%A0%20%E8%A1%80%E5%B0%B8");

        // `java.*` 在规则里也能用
        let mut s = daomubiji();
        s.toc.chapter_name = "tag.a@text@js:java.md5Encode(result).slice(0,8)".into();
        let c = Crawler::new(s, recorded());
        let toc = c.toc("http://www.daomubiji.com/dao-mu-bi-ji-1").unwrap();
        assert_eq!(toc[0].title.len(), 8);
        assert!(toc[0].title.chars().all(|c| c.is_ascii_hexdigit()));

        // `{{ }}` 里算不出来的表达式现在也走 JS。真实用武之地是 `searchUrl` 这种
        // **整串就是 URL** 的地方 —— 规则里不行：没有 `@` 取值器的规则在 legado 里
        // 也返回空（`getResultLast` 把最后一段当属性名），我们保持一致。
        let mut s = daomubiji();
        s.search_url = Some("http://x/{{java.md5Encode('a').slice(0,3)}}?k={{key}}".into());
        let c = Crawler::new(s, recorded());
        // 记录里没有这个地址，报错信息里会带上拼好的 URL —— 正好用来断言拼对了
        let e = c.search("斗破", 1).unwrap_err();
        assert!(format!("{e}").contains("http://x/0cc?k="), "{e}");
    }

    /// 脚本抛异常时要报出来，不能把整个抓取吞掉。
    #[test]
    fn a_broken_js_rule_surfaces_as_an_error() {
        let mut s = daomubiji();
        s.toc.chapter_name = "@js:this is not javascript ((".into();
        let c = Crawler::new(s, recorded());
        let e = c
            .toc("http://www.daomubiji.com/dao-mu-bi-ji-1")
            .unwrap_err();
        assert!(matches!(e, Error::Js(_)), "{e:?}");
        assert!(format!("{e}").contains("JS 出错"), "{e}");
    }

    #[test]
    fn missing_pages_fail_loudly() {
        let c = crawler();
        assert!(matches!(c.toc("http://nope/"), Err(Error::Net(_))));
        assert!(matches!(c.content("http://nope/"), Err(Error::Net(_))));
    }

    /// 真打站点。默认不跑（`cargo test -- --ignored` 才跑），因为：
    /// * 需要网络；
    /// * 站点会改版、会挂、会被墙 —— 一个会随机变红的测试比没有测试更糟。
    ///
    /// 上面那些夹具测试盯的是**我们的实现**，这个盯的是**书源和站点是否还活着**。
    #[test]
    #[ignore = "需要网络"]
    fn live_daomubiji() {
        let c = Crawler::new(daomubiji(), net::Fetcher::new(daomubiji().headers()));
        let toc = c
            .toc("http://www.daomubiji.com/dao-mu-bi-ji-1")
            .expect("拉目录");
        assert!(toc.len() > 10, "{} 章", toc.len());
        assert!(toc[0].title.contains("第一章"), "{:?}", toc[0].title);

        let text = c.content(&toc[0].url).expect("拉正文");
        assert!(text.len() > 500, "正文只有 {} 字", text.len());
        assert!(text.contains("血尸"));
        println!("=== 第一章 {} ===", toc[0].title);
        println!(
            "{}",
            text.lines().take(3).collect::<Vec<_>>().join(
                "
"
            )
        );
    }

    #[test]
    fn unescape_entities_covers_the_common_entities() {
        assert_eq!(unescape_entities("a&nbsp;b"), "a\u{a0}b");
        assert_eq!(unescape_entities("&lt;p&gt;"), "<p>");
        assert_eq!(unescape_entities("&amp;amp"), "&amp");
        assert_eq!(unescape_entities("&#20320;&#22909;"), "你好");
        assert_eq!(unescape_entities("&#x4f60;"), "你");
        assert_eq!(unescape_entities("100% & 200%"), "100% & 200%");
        // 认不出来的实体保持原样，不能吃掉字符
        assert_eq!(unescape_entities("&nosuchthing;x"), "&nosuchthing;x");
        assert_eq!(unescape_entities("&"), "&");
        assert_eq!(unescape_entities("中文&more"), "中文&more");
    }

    #[test]
    fn combine_follows_the_three_separators() {
        let v = |x: Vec<Vec<i32>>| x;
        assert_eq!(combine(v(vec![vec![1, 2], vec![3]]), Some("&&")), [1, 2, 3]);
        assert_eq!(combine(v(vec![vec![1, 2], vec![3]]), None), [1, 2, 3]);
        // %% 交错：先各取第 0 个，再各取第 1 个
        assert_eq!(
            combine(v(vec![vec![1, 2], vec![3, 4]]), Some("%%")),
            [1, 3, 2, 4]
        );
        assert_eq!(combine(v(vec![vec![1], vec![2, 3]]), Some("%%")), [1, 2]);
        assert!(combine::<i32>(v(vec![]), Some("%%")).is_empty());
    }
}
