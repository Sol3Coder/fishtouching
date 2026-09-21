//! XPath 源的全链路 —— 用假站点 + 真实 HTML 形状打，不依赖真站点。
//!
//! 真 XPath 源里绝大多数站都是死的（33 个纯 XPath 源一个都跑不通，
//! 全是「页面上没找到搜索结果」/证书过期/DNS 失败）。所以这里造一份
//! **形状真实**的 HTML：`<dl><dt>作者</dt><dd><a>…` 那种列表页、
//! `og:novel:*` 那套 meta、以及三层嵌套的目录。
//!
//! 用到的表达式全是量出来的最高频那几条（见 `survey_xpath.py`）：
//! `//*[@id="list"]//dt[2]/following-sibling::dd/a`（26 次）、
//! `//*[text()="作者"]//text()`（22 次）、
//! `//meta[@property='og:novel:author']/@content`（19 次）。

use std::sync::{Arc, Mutex};

use novel_core::crawl::{Chapter, Crawler, Fetch, Hit};
use novel_core::net::Response;
use novel_core::source::BookSource;

#[derive(Clone)]
struct Fake(Arc<Mutex<Vec<String>>>);

impl Fake {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    fn asked(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

const SEARCH: &str = r#"<html><head>
  <meta property="og:novel:book_name" content="斗破苍穹">
  <meta property="og:novel:author" content="天蚕土豆">
  <meta property="og:novel:category" content="玄幻">
  <meta property="og:image" content="/img/dp.jpg">
</head><body>
  <div id="list"><dl>
    <dt>书名</dt><dd><a href="/book/1/">斗破苍穹</a></dd>
    <dt>最新</dt><dd><a href="/book/1/last.html">第100章</a></dd>
  </dl></div>
  <div id="label">作者</div>
  <div id="authorName">天蚕土豆</div>
</body></html>"#;

const TOC: &str = r#"<html><body>
  <div class="bookinfo"><p><i>斗破苍穹</i></p></div>
  <ul id="chapters">
    <li><a href="/c/1.html">第1章 陨落的天才</a></li>
    <li><a href="/c/2.html">第2章 斗之气</a></li>
    <li><a href="/c/3.html">第3章 客人</a></li>
  </ul>
</body></html>"#;

const CHAPTER: &str = r#"<html><body>
  <div id="content">正文第一段。<br>正文第二段。</div>
</body></html>"#;

impl Fetch for Fake {
    fn get(&self, spec: &str) -> novel_core::R<Response> {
        let (url, _) = novel_core::net::split_url_option(spec);
        self.0.lock().unwrap().push(url.clone());
        let body = if url.contains("/search") {
            SEARCH
        } else if url.contains("/book/1/") {
            TOC
        } else if url.contains("/c/") {
            CHAPTER
        } else {
            return Err(novel_core::Error::Missing(format!("假站点没有：{url}")));
        };
        Ok(Response {
            url,
            body: body.into(),
        })
    }
}

fn source() -> BookSource {
    let json = r#"[{
      "bookSourceName": "假 XPath 站",
      "bookSourceUrl": "http://www.x.test/",
      "bookSourceType": 0,
      "enabled": true,
      "searchUrl": "http://www.x.test/search?q={{key}}",
      "ruleSearch": {
        "bookList": "//*[@id=\"list\"]//dd/a",
        "name": "text",
        "bookUrl": "href",
        "author": "//*[text()=\"作者\"]/following-sibling::div[@id='authorName']/text()",
        "intro": "//meta[@property='og:novel:category']/@content",
        "coverUrl": "//meta[@property='og:image']/@content",
        "lastChapter": "//meta[@property='og:novel:book_name']/@content"
      },
      "ruleBookInfo": {
        "name": "//*[@class='bookinfo']/p[1]/i/text()",
        "author": "//meta[@property='og:novel:author']/@content"
      },
      "ruleToc": {
        "chapterList": "//ul[@id='chapters']/li/a",
        "chapterName": "text",
        "chapterUrl": "href"
      },
      "ruleContent": { "content": "//div[@id='content']/text()" }
    }]"#;
    BookSource::parse_file(json).unwrap().remove(0)
}

fn crawler() -> (Crawler<Fake>, Fake) {
    let fake = Fake::new();
    (Crawler::new(source(), fake.clone()), fake)
}

#[test]
fn an_xpath_source_walks_the_whole_chain() {
    let (c, fake) = crawler();

    let hits: Vec<Hit> = c.search("斗破", 1).expect("搜索应当成功");
    assert_eq!(hits.len(), 2, "`//*[@id=\"list\"]//dd/a` 应当给出 2 条");
    assert_eq!(hits[0].name, "斗破苍穹");
    assert_eq!(hits[0].url, "http://www.x.test/book/1/");
    assert_eq!(hits[1].name, "第100章");
    // `//*[text()="作者"]/...` —— 「找文本正好是『作者』的那个标签，
    // 再取它旁边那个值」。真书源里 22 次就是这个用法。
    assert_eq!(hits[0].author, "天蚕土豆");
    // `/@content` 取值器
    assert_eq!(hits[0].intro, "玄幻");
    assert_eq!(hits[0].cover, "http://www.x.test/img/dp.jpg");

    let toc: Vec<Chapter> = c
        .toc("http://www.x.test/book/1/index.html")
        .expect("拉目录应当成功");
    assert_eq!(toc.len(), 3);
    assert_eq!(toc[0].title, "第1章 陨落的天才");
    assert_eq!(toc[0].url, "http://www.x.test/c/1.html");
    assert!(fake.asked().iter().any(|u| u.contains("/book/1/")));

    let text = c.content(&toc[0].url).expect("拉正文应当成功");
    assert!(text.contains("正文第一段"), "正文：{text}");
}

/// `following-sibling::` 是真书源里唯一常用的轴
/// （`//*[@id="list"]//dt[2]/following-sibling::dd/a`，26 次）。
#[test]
fn following_sibling_axis_works_on_a_real_dl_shape() {
    let mut src = source();
    // 只要「最新」那一行：dt[2] 后面的 dd 里的 a
    src.search.book_list = r#"//*[@id="list"]//dt[2]/following-sibling::dd/a"#.into();
    let fake = Fake::new();
    let c = Crawler::new(src, fake);
    let hits = c.search("斗破", 1).unwrap();
    assert_eq!(hits.len(), 1, "dt[2] 后面只有一个紧跟的 dd");
    assert_eq!(hits[0].name, "第100章");
    assert_eq!(hits[0].url, "http://www.x.test/book/1/last.html");
}

#[test]
fn a_path_and_a_position_predicate() {
    let mut src = source();
    // `//div[5]/div/.../li` 那种纯位置路径（真书源里 9 次）
    src.toc.chapter_list = "//ul[@id='chapters']/li[2]/a".into();
    let fake = Fake::new();
    let c = Crawler::new(src, fake);
    let toc = c.toc("http://www.x.test/book/1/").unwrap();
    assert_eq!(toc.len(), 1);
    assert_eq!(toc[0].title, "第2章 斗之气");
}

/// 并集 `|`
#[test]
fn union_selects_both_sides() {
    let mut src = source();
    src.search.book_list =
        r#"//*[@id="list"]//dt[1]/following-sibling::dd/a | //div[@id="authorName"]"#.into();
    let fake = Fake::new();
    let c = Crawler::new(src, fake);
    let hits = c.search("斗破", 1).unwrap();
    // dt[1]（书名）后面有两个 dd（XPath 的 following-sibling 不截断），
    // 加上并集另一边的 div，一共 3 个
    assert_eq!(
        hits.len(),
        3,
        "实际：{:?}",
        hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );
}

/// `@xpath:` 前缀和裸 `//` 等价。
#[test]
fn the_at_xpath_prefix_is_the_same_thing() {
    let mut src = source();
    src.toc.chapter_list = "@xpath://ul[@id='chapters']/li/a".into();
    let fake = Fake::new();
    let c = Crawler::new(src, fake);
    assert_eq!(c.toc("http://www.x.test/book/1/").unwrap().len(), 3);
}

/// XPath 解析不了的东西要报错，不要静默给空。
#[test]
fn unsupported_xpath_errors_instead_of_returning_nothing() {
    let mut src = source();
    // 用不到的轴要报错（真书源里只出现 following/preceding-sibling）
    src.search.book_list = "//div/ancestor::x".into();
    let fake = Fake::new();
    let c = Crawler::new(src, fake);
    let e = c.search("斗破", 1).unwrap_err();
    assert!(
        e.to_string().contains("ancestor::"),
        "错误信息要说清楚是什么不支持：{e}"
    );
}

/// 单个坏掉的**可选**字段不该干掉整次搜索。
#[test]
fn a_broken_optional_xpath_field_does_not_kill_the_search() {
    let mut src = source();
    src.search.intro = "//nope[@a>1]".into();
    let fake = Fake::new();
    let c = Crawler::new(src, fake);
    let hits = c.search("斗破", 1).expect("搜索仍然应当成功");
    assert_eq!(hits.len(), 2);
    assert!(hits[0].intro.is_empty());
}
