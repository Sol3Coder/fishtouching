//! 书源模型 —— 字段名照抄 legado 的 `BookSource`，这样现成的书源 JSON 不用转换。
//!
//! 只解析真正会被用到的字段；其余（`loginUi`、`coverDecodeJs`、`variableComment` …）
//! 留在 JSON 里被忽略 —— 我们只读书源，不写回。

use serde::{Deserialize, Deserializer, Serialize};

/// 子规则既可能是对象，也可能是**被转义的 JSON 字符串**（legado 的导入器两种都收）。
/// 只用在结构体字段上，别用在 `String` 上 —— 那会把普通规则文本当 JSON 去解析。
fn object_or_json<'de, D, T>(de: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let v = serde_json::Value::deserialize(de)?;
    match v {
        serde_json::Value::Null => Ok(T::default()),
        serde_json::Value::String(s) if s.trim().is_empty() => Ok(T::default()),
        serde_json::Value::String(s) => serde_json::from_str(&s).map_err(serde::de::Error::custom),
        other => serde_json::from_value(other).map_err(serde::de::Error::custom),
    }
}

macro_rules! rule_struct {
    ($(#[$m:meta])* $vis:vis struct $name:ident { $($(#[$fm:meta])* $field:ident : $ty:ty),* $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Default, Serialize, Deserialize)]
        $vis struct $name {
            $(
                $(#[$fm])*
                #[serde(default)]
                pub $field: $ty,
            )*
        }
    };
}

/// 一个书源。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BookSource {
    #[serde(rename = "bookSourceName", default)]
    pub name: String,
    /// 源自己的主页，也是所有相对 URL 的基准。
    #[serde(rename = "bookSourceUrl", default)]
    pub url: String,
    #[serde(rename = "bookSourceGroup", default)]
    pub group: Option<String>,
    #[serde(rename = "bookSourceComment", default)]
    pub comment: Option<String>,
    /// 0=文本 1=音频 2=图片 3=文件
    #[serde(rename = "bookSourceType", default)]
    pub kind: i32,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(rename = "enabledExplore", default)]
    pub explore_enabled: bool,
    /// **三态**：legado 里 `null` 和 `false` 都表示不启用 cookie jar。
    #[serde(rename = "enabledCookieJar", default)]
    pub cookie_jar: Option<bool>,
    /// 请求头，JSON 对象字符串。
    #[serde(default)]
    pub header: Option<String>,
    #[serde(rename = "searchUrl", default)]
    pub search_url: Option<String>,
    #[serde(rename = "exploreUrl", default)]
    pub explore_url: Option<String>,

    #[serde(rename = "ruleSearch", default, deserialize_with = "object_or_json")]
    pub search: SearchRule,
    #[serde(rename = "ruleBookInfo", default, deserialize_with = "object_or_json")]
    pub book_info: BookInfoRule,
    #[serde(rename = "ruleToc", default, deserialize_with = "object_or_json")]
    pub toc: TocRule,
    #[serde(rename = "ruleContent", default, deserialize_with = "object_or_json")]
    pub content: ContentRule,
    #[serde(rename = "ruleExplore", default, deserialize_with = "object_or_json")]
    pub explore: ExploreRule,
}

fn yes() -> bool {
    true
}

impl BookSource {
    /// 解析 `header` 字段（JSON 对象字符串）。坏 JSON 当成没有头，不 panic。
    pub fn headers(&self) -> Vec<(String, String)> {
        let Some(raw) = self
            .header
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Vec::new();
        };
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(raw)
        else {
            return Vec::new();
        };
        map.into_iter()
            .filter_map(|(k, v)| Some((k, v.as_str()?.to_string())))
            .collect()
    }

    /// 能否搜索。`searchUrl` 是 `-`、空、或缺，都表示这个源不做搜索。
    pub fn can_search(&self) -> bool {
        self.search_url
            .as_deref()
            .map(str::trim)
            .is_some_and(|u| !u.is_empty() && u != "-")
    }

    /// 从 legado 导出的书源文件里读。单对象和数组（legado 的导出格式）都收。
    pub fn parse_file(json: &str) -> Result<Vec<Self>, serde_json::Error> {
        let v: serde_json::Value = serde_json::from_str(json)?;
        match v {
            serde_json::Value::Array(_) => serde_json::from_value(v),
            serde_json::Value::Object(_) => Ok(vec![serde_json::from_value(v)?]),
            _ => Err(serde::de::Error::custom("书源文件既不是对象也不是数组")),
        }
    }
}

rule_struct! {
    /// 搜索结果列表里的一条对应什么。
    pub struct SearchRule {
        #[serde(rename = "bookList")] book_list: String,
        name: String,
        author: String,
        intro: String,
        kind: String,
        #[serde(rename = "lastChapter")] last_chapter: String,
        #[serde(rename = "updateTime")] update_time: String,
        #[serde(rename = "bookUrl")] book_url: String,
        #[serde(rename = "coverUrl")] cover_url: String,
        #[serde(rename = "wordCount")] word_count: String,
        #[serde(rename = "checkKeyWord")] check_key_word: String,
    }
}

rule_struct! {
    /// 书籍详情页。
    pub struct BookInfoRule {
        init: String,
        name: String,
        author: String,
        intro: String,
        kind: String,
        #[serde(rename = "lastChapter")] last_chapter: String,
        #[serde(rename = "updateTime")] update_time: String,
        #[serde(rename = "coverUrl")] cover_url: String,
        #[serde(rename = "tocUrl")] toc_url: String,
        #[serde(rename = "wordCount")] word_count: String,
        #[serde(rename = "canReName")] can_re_name: String,
    }
}

rule_struct! {
    /// 目录页。
    pub struct TocRule {
        #[serde(rename = "chapterList")] chapter_list: String,
        #[serde(rename = "chapterName")] chapter_name: String,
        #[serde(rename = "chapterUrl")] chapter_url: String,
        #[serde(rename = "isVolume")] is_volume: String,
        #[serde(rename = "isVip")] is_vip: String,
        #[serde(rename = "isPay")] is_pay: String,
        #[serde(rename = "updateTime")] update_time: String,
        #[serde(rename = "nextTocUrl")] next_toc_url: String,
    }
}

rule_struct! {
    /// 正文页。
    pub struct ContentRule {
        content: String,
        #[serde(rename = "subContent")] sub_content: String,
        title: String,
        #[serde(rename = "nextContentUrl")] next_content_url: String,
        #[serde(rename = "webJs")] web_js: String,
        #[serde(rename = "sourceRegex")] source_regex: String,
        #[serde(rename = "replaceRegex")] replace_regex: String,
    }
}

rule_struct! {
    /// 发现页。
    pub struct ExploreRule {
        #[serde(rename = "bookList")] book_list: String,
        name: String,
        author: String,
        intro: String,
        kind: String,
        #[serde(rename = "lastChapter")] last_chapter: String,
        #[serde(rename = "updateTime")] update_time: String,
        #[serde(rename = "bookUrl")] book_url: String,
        #[serde(rename = "coverUrl")] cover_url: String,
        #[serde(rename = "wordCount")] word_count: String,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "bookSourceName": "盗墓笔记",
        "bookSourceUrl": "http://www.daomubiji.com/",
        "bookSourceType": 0,
        "enabled": true,
        "enabledExplore": true,
        "enabledCookieJar": false,
        "header": "{\"User-Agent\":\"Mozilla/5.0\"}",
        "searchUrl": "-",
        "ruleSearch": {
            "bookList": "class.container@tag.li!0",
            "name": "class.menu-item@tag.a.0@text",
            "bookUrl": "tag.a@href",
            "author": "text##(|)"
        },
        "ruleBookInfo": { "name": "h1@text", "intro": "class.focusbox-text@text" },
        "ruleToc": {
            "chapterList": "class.excerpts-wrapper@tag.a",
            "chapterName": "tag.a@text",
            "chapterUrl": "tag.a@href"
        },
        "ruleContent": { "content": "class.article-content@tag.p@textNodes" }
    }"#;

    #[test]
    fn parses_a_real_shaped_source() {
        let s = BookSource::parse_file(SAMPLE).unwrap().remove(0);
        assert_eq!(s.name, "盗墓笔记");
        assert_eq!(s.url, "http://www.daomubiji.com/");
        assert!(!s.can_search(), "searchUrl 是 - ，应当算不能搜索");
        assert_eq!(s.search.book_list, "class.container@tag.li!0");
        assert_eq!(s.toc.chapter_name, "tag.a@text");
        assert_eq!(s.content.content, "class.article-content@tag.p@textNodes");
        assert_eq!(s.toc.next_toc_url, "", "缺的字段应当是空串，不是 panic");
        assert_eq!(
            s.headers(),
            vec![("User-Agent".to_string(), "Mozilla/5.0".to_string())]
        );
    }

    /// 字段名打错是最容易发生、又最难发现的一类 bug：不报错，只是永远读到空串。
    /// 所以拿原始 JSON 的键逐个核对（靠序列化回来的键集合）。
    #[test]
    fn field_names_match_legado_exactly() {
        let raw: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        let s = BookSource::parse_file(SAMPLE).unwrap().remove(0);

        let check = |json: &serde_json::Value, ours: serde_json::Value, what: &str| {
            let want: std::collections::BTreeSet<&String> =
                json.as_object().unwrap().keys().collect();
            let got: std::collections::BTreeSet<String> = ours
                .as_object()
                .unwrap()
                .keys()
                .filter(|k| !ours[k].is_null())
                .cloned()
                .collect();
            let got_empty: std::collections::BTreeSet<String> = ours
                .as_object()
                .unwrap()
                .iter()
                .filter(|(_, v)| v.as_str() == Some(""))
                .map(|(k, _)| k.clone())
                .collect();
            for k in &want {
                assert!(
                    got.contains(*k) || got_empty.contains(*k),
                    "{what} 丢了字段 {k:?}；我们序列化出来的是 {got:?}"
                );
            }
            // 我们多出来的字段只能是「原始 JSON 里没有、于是取了默认值」的，
            // 非空就说明名字写错了。
            for k in got.difference(&got_empty) {
                assert!(want.contains(k), "{what} 多出字段 {k:?}");
            }
        };

        check(
            &raw["ruleSearch"],
            serde_json::to_value(&s.search).unwrap(),
            "ruleSearch",
        );
        check(
            &raw["ruleBookInfo"],
            serde_json::to_value(&s.book_info).unwrap(),
            "ruleBookInfo",
        );
        check(
            &raw["ruleToc"],
            serde_json::to_value(&s.toc).unwrap(),
            "ruleToc",
        );
        check(
            &raw["ruleContent"],
            serde_json::to_value(&s.content).unwrap(),
            "ruleContent",
        );
    }

    #[test]
    fn accepts_a_bare_object_and_a_weird_header() {
        assert_eq!(BookSource::parse_file(SAMPLE).unwrap().len(), 1);
        assert!(BookSource::parse_file("[]").unwrap().is_empty());
        assert!(BookSource::parse_file("42").is_err());

        let mut s = BookSource::parse_file(SAMPLE).unwrap().remove(0);
        s.header = Some("不是 JSON".into());
        assert!(s.headers().is_empty(), "坏 header 不能 panic");
        s.header = None;
        assert!(s.headers().is_empty());
    }

    #[test]
    fn sub_rules_may_be_json_strings_too() {
        // legado 的导入器两种形态都收，别人的书源集合里确实有这种
        let json = r#"{"bookSourceName":"x","bookSourceUrl":"http://x/",
            "ruleToc":"{\"chapterList\":\"a\",\"chapterName\":\"b@text\"}"}"#;
        let s = BookSource::parse_file(json).unwrap().remove(0);
        assert_eq!(s.toc.chapter_list, "a");
        assert_eq!(s.toc.chapter_name, "b@text");
    }

    #[test]
    fn cookie_jar_is_three_state() {
        let with = |raw: &str| {
            let j = format!(r#"{{"bookSourceName":"x","bookSourceUrl":"http://x/",{raw}}}"#);
            BookSource::parse_file(&j).unwrap().remove(0).cookie_jar
        };
        assert_eq!(with("\"enabledCookieJar\":true"), Some(true));
        assert_eq!(with("\"enabledCookieJar\":false"), Some(false));
        assert_eq!(with("\"enabled\":true"), None, "缺字段是 None，不是 false");
    }

    #[test]
    fn defaults_are_permissive() {
        // 只有名字和 url 的源也要能解析（真书源里有这种）
        let s = BookSource::parse_file(r#"{"bookSourceName":"n","bookSourceUrl":"u"}"#)
            .unwrap()
            .remove(0);
        assert!(s.enabled, "enabled 默认是 true");
        assert!(!s.explore_enabled);
        assert!(s.search.book_list.is_empty());
        assert!(!s.can_search());
    }
}
