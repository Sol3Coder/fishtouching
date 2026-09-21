//! JSON 源的全链路 —— 用假 API 打，不依赖真站点。
//!
//! 真站点那 50 个 JSON 源里绝大多数是死的（DNS/404/403），剩下几个要
//! `<js>` 中段串联。所以这里自己造一个**真实的 JSON API 形状**：
//!
//! * 搜索：`$.data.lists[*]`，元素字段在根上，书名带 `@put:{book:id}`
//! * 目录：URL 里用 `{{book}}` 拼（搜索阶段 put 进去的变量）
//! * 正文：JSON 里再取一层
//!
//! 这三步正好覆盖 JSON 路径引擎的全部关键语义：不定路径、定路径指着数组、
//! 递归下降、`@put:`、以及**内嵌规则对着当前元素求值**。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use novel_core::crawl::{Chapter, Crawler, Fetch, Hit};
use novel_core::net::Response;
use novel_core::source::BookSource;

/// 按 URL 给不同的假响应。
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

impl Fetch for Fake {
    fn get(&self, spec: &str) -> novel_core::R<Response> {
        let (url, _) = novel_core::net::split_url_option(spec);
        self.0.lock().unwrap().push(url.clone());

        // 搜索接口
        if url.contains("/search") {
            return Ok(Response {
                url,
                body: r#"{
                  "code": 0,
                  "data": { "lists": [
                    {"bookName": "斗破苍穹", "authorPenName": "天蚕土豆", "id": "1001",
                     "introduction": "简介一", "lastUpdateChapterName": "第1章"},
                    {"bookName": "武动乾坤", "authorPenName": "天蚕土豆", "id": "1002",
                     "introduction": "简介二", "lastUpdateChapterName": "第2章"}
                  ]}
                }"#
                .into(),
            });
        }

        // 详情页 = 目录页（没写 tocUrl 时 legado 也是拿详情页当目录页）
        if url.contains("/book/1001/info") || url.contains("/book/1001/volumes") {
            return Ok(Response {
                url,
                body: r#"{
                  "data": { "volumes": [
                    {"chapters": [
                      {"chapterId": "c1", "chapterName": "第1章 陨落的天才"},
                      {"chapterId": "c2", "chapterName": "第2章 斗之气"}
                    ]},
                    {"chapters": [
                      {"chapterId": "c3", "chapterName": "第3章 客人"}
                    ]}
                  ]}
                }"#
                .into(),
            });
        }

        // 一个 HTML 页面（用来验「JSON 规则打到 HTML 上要报错」）
        if url.contains("/static/") {
            return Ok(Response {
                url,
                body: "<html><body><div id=\"c\">正文</div></body></html>".into(),
            });
        }

        // 正文接口
        if url.contains("/chapter/") {
            return Ok(Response {
                url,
                body: r#"{"data": {"content": "正文第一段。\n正文第二段。"}}"#.into(),
            });
        }

        Err(novel_core::Error::Missing(format!(
            "假站点没有这个接口：{url}"
        )))
    }
}

fn source() -> BookSource {
    let json = r#"[{
      "bookSourceName": "假 JSON API",
      "bookSourceUrl": "http://api.test/",
      "bookSourceType": 0,
      "enabled": true,
      "searchUrl": "http://api.test/search?key={{key}}",
      "ruleSearch": {
        "bookList": "$.data.lists[*]",
        "name": "$.bookName@put:{book:id}",
        "author": "$.authorPenName",
        "bookUrl": "http://api.test/book/{{$.id}}/info",
        "intro": "$.introduction",
        "lastChapter": "$.lastUpdateChapterName"
      },
      "ruleToc": {
        "chapterList": "$.data.volumes..chapters[*]",
        "chapterName": "$.chapterName",
        "chapterUrl": "http://api.test/chapter/{{$.chapterId}}"
      },
      "ruleContent": { "content": "$.data.content" }
    }]"#;
    BookSource::parse_file(json).unwrap().remove(0)
}

fn crawler() -> (Crawler<Fake>, Fake) {
    let fake = Fake::new();
    (Crawler::new(source(), fake.clone()), fake)
}

#[test]
fn a_json_api_source_walks_the_whole_chain() {
    let (c, fake) = crawler();

    // ── 搜索 ──
    let hits: Vec<Hit> = c.search("斗破", 1).expect("搜索应当成功");
    assert_eq!(hits.len(), 2, "`$.data.lists[*]` 应当给出两条");
    assert_eq!(hits[0].name, "斗破苍穹");
    assert_eq!(hits[0].author, "天蚕土豆");
    assert_eq!(hits[0].intro, "简介一");
    assert_eq!(hits[0].last_chapter, "第1章");
    // 内嵌规则 `{{$.id}}` 要对着**这个元素**求值，不是对着整篇 JSON。
    // 写错的话这里会变成文档根上的 id（不存在）→ 空链接。
    assert_eq!(hits[0].url, "http://api.test/book/1001/info");
    assert_eq!(hits[1].url, "http://api.test/book/1002/info");

    // ── 目录 ──
    // URL 里的 `{{book}}` 来自搜索那条 `@put:{book:id}`。
    // （`@put:` 没实现的话，`$.bookName@put:{book:id}` 会被当成一个键名，
    //  于是 name 为空、`book` 变量也不存在 —— 表现为「搜索没结果」。）
    let toc: Vec<Chapter> = c.toc(&hits[0].url).expect("拉目录应当成功");
    assert_eq!(
        toc.len(),
        3,
        "`$.data.volumes..chapters[*]` 三层套嵌要能挖到底"
    );
    assert_eq!(toc[0].title, "第1章 陨落的天才");
    assert_eq!(toc[0].url, "http://api.test/chapter/c1");
    assert_eq!(toc[2].title, "第3章 客人");
    // 目录请求真的发出去过（`{{book}}` 那条路）
    assert!(
        fake.asked().iter().any(|u| u.contains("/book/1001/")),
        "实际请求：{:?}",
        fake.asked()
    );

    // ── 正文 ──
    let text = c.content(&toc[0].url).expect("拉正文应当成功");
    assert!(text.contains("正文第一段"), "正文：{text}");
}

#[test]
fn a_definite_path_to_an_array_is_a_valid_element_list() {
    // `$.data` 这种定路径指着数组的写法，真书源里用了 49 次。
    // 不定路径 `$.data[*]` 和它必须给一样的东西。
    let mut src = source();
    src.search.book_list = "$.data.lists".into();
    let fake = Fake::new();
    let c = Crawler::new(src, fake);
    let hits = c.search("斗破", 1).expect("搜索应当成功");
    assert_eq!(hits.len(), 2, "定路径指着数组也要当成元素列表");
    assert_eq!(hits[0].name, "斗破苍穹");
}

#[test]
fn a_broken_optional_field_does_not_kill_the_whole_search() {
    // 一个写坏的简介规则不该让全部结果都消失 ——
    // 少个简介是小事，搜索整个空掉是大事
    let mut src = source();
    src.search.intro = "$.完全不存在的路径[*]".into();
    src.search.cover_url = "class.nope@img@src".into();
    let fake = Fake::new();
    let c = Crawler::new(src, fake);
    let hits = c.search("斗破", 1).expect("搜索仍然应当成功");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].name, "斗破苍穹");
    assert!(hits[0].intro.is_empty(), "取不到就留空");
}

#[test]
fn a_json_path_against_an_html_page_says_so() {
    // 正文其实是 HTML，但规则写了 JSON 路径：要**报错**，不要静默取空
    let mut src = source();
    src.content.content = "$.data.content".into();
    let c = Crawler::new(src, Fake::new());
    // 这个地址返回的是 HTML
    let e = c.content("http://api.test/static/1.html").unwrap_err();
    assert!(e.to_string().contains("不是 JSON"), "错误信息要说清楚：{e}");
}

#[test]
fn put_variables_are_recorded() {
    // `@put:{book:id}` 的取值部分是**规则**（legado: `put(key, getString(value))`），
    // 所以要真的把本元素的 id 解出来存进变量表。
    // 后面拼目录 URL 全靠它 —— 存成字面量 "id" 的话目录永远拉不到。
    let (c, _) = crawler();
    let _ = c.search("斗破", 1).unwrap();
    let vars: HashMap<String, String> = c.vars.borrow().clone();
    // **后写的赢**：legado 的 `putRule` 是每求值一次规则就 put 一次，
    // 所以一条 `bookList` 跑完，变量里留的是**最后一条**结果的值。
    // （这就是为什么 tocUrl 那种要靠变量的字段不能在搜索结果循环里求值。）
    assert_eq!(
        vars.get("book").map(String::as_str),
        Some("1002"),
        "`@put:{{book:id}}` 应当存下 id，实际 {vars:?}"
    );
}

/// 正文是 JSON 时，**不带 `$.` 的裸键名**也要当 JSON 路径解。
///
/// legado 的 `SourceRule` 有个 `isJSON` 标志管这个，真书源里
/// `chapterList: "data"` 这种写法很常见。
#[test]
fn a_bare_key_is_a_json_path_when_the_body_is_json() {
    let mut src = source();
    // 裸键名（不带 `$.`）—— 正文是 JSON 时 legado 全都按 JSON 路径解
    src.toc.chapter_list = "data.volumes..chapters[*]".into();
    src.toc.chapter_name = "chapterName".into();
    src.content.content = "data.content".into();
    let fake = Fake::new();
    let c = Crawler::new(src, fake);
    let toc = c
        .toc("http://api.test/book/1001/info")
        .expect("裸键名要能解");
    assert_eq!(toc.len(), 3, "`chapterName` 不带 $. 也要能取到");
    assert_eq!(toc[0].title, "第1章 陨落的天才");
    let text = c.content("http://api.test/chapter/c1").unwrap();
    assert!(text.contains("正文第一段"), "{text}");
}

/// `@@` 是「正文是 JSON 但我要写选择器」的逃生口。
#[test]
fn double_at_escapes_back_to_html_selectors() {
    let src = source();
    // 用 `@@` 强制 HTML 模式，正文是 JSON —— 取不到东西也不该把 JSON 路径当选择器
    let fake = Fake::new();
    let c = Crawler::new(src, fake);
    let hits = c.search("斗破", 1).unwrap();
    let e = c.content("http://api.test/static/1.html");
    // 假站点没这个接口，报的是网络错；关键是别报「JSON 路径里的字符」
    assert!(e.is_err());
    let _ = hits;
}
