//! 拉页面、猜编码、拼相对链接。
//!
//! 三件在这个场景里必须做对的事：
//! 1. **重定向后的 URL 才是基准** —— 书源里写的 `bookSourceUrl` 常常是旧域名
//!    （`daomubiji.com` 没事，但 `ddyueshu.com` 已经 301 到 `ddyueshv.cc` 了），
//!    相对链接必须对着最终 URL 拼。
//! 2. **编码要猜** —— 中文站点有相当一部分是 GBK/GB2312，直接按 UTF-8 解码会得到
//!    一屏乱码，然后所有选择器都匹配不上。
//! 3. **URL 里的中文要转义** —— 搜索词直接塞进 query 是发不出去的。

use std::time::Duration;

use serde::Deserialize;
use ureq::ResponseExt;

use crate::{Error, R};

/// 默认 UA。很多站点对没有 UA 的请求直接 403。
pub const UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/120.0.0.0 Safari/537.36";

/// 底层的 `ureq::Agent` 内部是 Arc，所以 clone 很便宜 —— 每个任务复制一份即可。
#[derive(Clone)]
pub struct Fetcher {
    agent: ureq::Agent,
    /// 书源 `header` 里给的头，每个请求都带。
    headers: Vec<(String, String)>,
}

/// 一次成功的抓取。
pub struct Response {
    /// 跟随重定向之后的 URL —— 相对链接要对着它解析。
    pub url: String,
    /// 已经按猜出来的编码解码好的正文。
    pub body: String,
}

impl Fetcher {
    /// 抓普通页面用的超时：够一个死站点暴露出来，又不至于让人干等。
    pub const PAGE_TIMEOUT: u64 = 60;

    pub fn new(headers: Vec<(String, String)>) -> Self {
        Self::with_timeout(headers, Self::PAGE_TIMEOUT)
    }

    /// 换一个总超时。书源集合动辄几十 MB（实测 21 MB / 75 秒），
    /// 拿抓页面的 60 秒去下它必然超时 —— 而且报出来的是「网络错误」，
    /// 看起来像站点挂了，其实是自己的预算太紧。
    pub fn with_timeout(headers: Vec<(String, String)>, secs: u64) -> Self {
        let cfg = ureq::Agent::config_builder()
            // legado 是 connect 15s / read 60s，这里合成一个总超时
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_global(Some(Duration::from_secs(secs)))
            .user_agent(UA)
            // 4xx/5xx 不当异常抛，我们要自己看状态码再决定
            .http_status_as_error(false)
            .build();
        Self {
            agent: cfg.into(),
            headers,
        }
    }

    /// 抓一个 URL。URL 后面可以跟 `,{json 选项}`（legado 的写法）。
    pub fn get(&self, url: &str) -> R<Response> {
        self.fetch(url, &[])
    }

    /// 例：`http://x/s.php,{"method":"POST","body":"key={{key}}","charset":"gbk"}`
    pub fn fetch(&self, spec: &str, extra_headers: &[(String, String)]) -> R<Response> {
        let (url, raw_opt) = split_url_option(spec);
        let opt = UrlOption::parse(raw_opt.as_deref());
        // URL 选项里带 JS 的（改 URL / 改响应 / WebView）一律拒绝，不猜
        if let Some(js) = [&opt.js, &opt.body_js, &opt.web_js]
            .into_iter()
            .flatten()
            .next()
        {
            return Err(Error::Unsupported(format!("URL 选项里的 JS：{js}")));
        }
        if opt.web_view.is_some() {
            return Err(Error::Unsupported("URL 选项要求 WebView".into()));
        }
        if let Some(c) = opt.charset.as_deref() {
            if c.eq_ignore_ascii_case("escape") {
                return Err(Error::Unsupported(
                    "charset: escape（JS 的 escape()）".into(),
                ));
            }
        }
        let url = encode_non_ascii(&url);
        let post = opt
            .method
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case("POST"));
        let all: Vec<(String, String)> = self
            .headers
            .iter()
            .cloned()
            .chain(opt.headers())
            .chain(extra_headers.iter().cloned())
            .collect();

        // POST 和 GET 的 builder 是不同类型，所以两条分支各写一遍
        let mut res = if post {
            let mut r = self.agent.post(&url);
            for (k, v) in &all {
                r = r.header(k, v);
            }
            r.header("content-type", "application/x-www-form-urlencoded")
                .send(form_bytes(
                    opt.body.as_deref().unwrap_or(""),
                    opt.charset.as_deref(),
                ))
        } else {
            let mut r = self.agent.get(&url);
            for (k, v) in &all {
                r = r.header(k, v);
            }
            r.call()
        }
        .map_err(|e| Error::Net(e.to_string()))?;

        let status = res.status().as_u16();
        let final_url = res.get_uri().to_string();
        if status >= 300 {
            return Err(Error::Http {
                status,
                url: final_url,
            });
        }
        let ctype = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = res
            .body_mut()
            .read_to_vec()
            .map_err(|e| Error::Net(e.to_string()))?;
        Ok(Response {
            url: final_url,
            body: decode(&bytes, ctype.as_deref()),
        })
    }
}

// ── 编码 ────────────────────────────────────────────────────────────────

/// 把字节解成字符串。顺序：BOM → HTTP 头 → `<meta>` → UTF-8 能解就用 UTF-8 → 否则 GBK。
///
/// 「先试 UTF-8 再退 GBK」是刻意的：UTF-8 是自校验的，误判概率极低，
/// 而 GBK 几乎能接受任何字节，先试它会把 UTF-8 页面解成乱码。
pub fn decode(bytes: &[u8], content_type: Option<&str>) -> String {
    if let Some((enc, off)) = bom(bytes) {
        let (s, _, _) = enc.decode(&bytes[off..]);
        return s.into_owned();
    }
    if let Some(label) = content_type.and_then(charset_of) {
        if let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes()) {
            let (s, _, _) = enc.decode(bytes);
            return s.into_owned();
        }
    }
    if let Some(label) = meta_charset(bytes) {
        if let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes()) {
            let (s, _, _) = enc.decode(bytes);
            return s.into_owned();
        }
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let (s, _, _) = encoding_rs::GBK.decode(bytes);
    s.into_owned()
}

fn bom(bytes: &[u8]) -> Option<(&'static encoding_rs::Encoding, usize)> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Some((encoding_rs::UTF_8, 3))
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        Some((encoding_rs::UTF_16LE, 2))
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        Some((encoding_rs::UTF_16BE, 2))
    } else {
        None
    }
}

/// 从 `text/html; charset=gbk` 里取 `gbk`。
fn charset_of(content_type: &str) -> Option<String> {
    let at = content_type.to_ascii_lowercase().find("charset=")? + "charset=".len();
    let rest = &content_type[at..];
    let end = rest
        .find(|c: char| c == ';' || c == ',' || c.is_whitespace())
        .unwrap_or(rest.len());
    let v = rest[..end].trim().trim_matches(['"', '\'']);
    (!v.is_empty()).then(|| v.to_string())
}

/// 扫前 4KB 找 `<meta charset=...>` 或 `<meta http-equiv=... content="...charset=...">`。
fn meta_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(4096)];
    let ascii: String = head
        .iter()
        .map(|&b| if b.is_ascii() { b as char } else { ' ' })
        .collect();
    let lower = ascii.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("<meta") {
        let start = from + i;
        let end = ascii[start..]
            .find('>')
            .map(|j| start + j)
            .unwrap_or(ascii.len());
        let tag = &ascii[start..end];
        let lower_tag = &lower[start..end];
        if let Some(at) = lower_tag.find("charset") {
            let rest = tag[at + "charset".len()..]
                .trim_start()
                .trim_start_matches('=')
                .trim_start()
                .trim_start_matches(['"', '\'']);
            let v: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
                .collect();
            if !v.is_empty() {
                return Some(v);
            }
        }
        from = end.max(start + 1);
    }
    None
}

// ── URL ─────────────────────────────────────────────────────────────────

/// 相对链接拼成绝对链接。对应 `NetworkUtils.getAbsoluteURL`。
pub fn absolute(base: &str, rel: &str) -> String {
    let rel = rel.trim();
    if rel.is_empty() {
        return String::new();
    }
    let low = rel.to_ascii_lowercase();
    // legado 对 javascript: 直接丢弃（否则会当成相对路径拼出一堆垃圾 URL）
    if low.starts_with("javascript:") {
        return String::new();
    }
    if low.starts_with("data:") || low.starts_with("mailto:") {
        return rel.to_string();
    }
    let (scheme, rest) = match split_scheme(base) {
        Some(v) => v,
        None => return rel.to_string(),
    };
    if low.starts_with("//") {
        return format!("{scheme}:{rel}");
    }
    if rel.contains("://") {
        return rel.to_string();
    }
    let (host, base_path) = split_host(rest);
    if rel.starts_with('/') {
        return format!("{scheme}://{host}{}", strip_dots(rel));
    }
    if rel.starts_with('?') || rel.starts_with('#') {
        let path = base_path.split(['?', '#']).next().unwrap_or("");
        return format!("{scheme}://{host}{path}{rel}");
    }
    let dir = match base_path.rfind('/') {
        Some(i) => &base_path[..=i],
        None => "/",
    };
    format!("{scheme}://{host}{}", strip_dots(&format!("{dir}{rel}")))
}

fn split_scheme(u: &str) -> Option<(&str, &str)> {
    let i = u.find("://")?;
    Some((&u[..i], &u[i + 3..]))
}

fn split_host(rest: &str) -> (&str, &str) {
    match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    }
}

/// 消掉 `/./` 和 `/../`。手写而不是引 `url` crate —— 书源里的相对链接就这么几种，
/// 而且我们只处理路径，不碰 query。
fn strip_dots(path: &str) -> String {
    let (path, tail) = match path.find(['?', '#']) {
        Some(i) => (&path[..i], &path[i..]),
        None => (path, ""),
    };
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("{}{tail}", out.join("/"))
}

/// 把 URL 里非 ASCII 的字节做 percent-encoding。
///
/// legado 在 GET 上**不**编码（它靠 OkHttp 兜底）。我们自己发请求，得转义，
/// 否则带中文的搜索 URL 直接构造失败。只动非 ASCII，所以 `&`、`=`、`%` 之类的
/// 模板字符原样保留，也不会二次编码。
pub fn encode_non_ascii(url: &str) -> String {
    if url.is_ascii() {
        return url.to_string();
    }
    let mut out = String::with_capacity(url.len() * 2);
    for b in url.bytes() {
        if b.is_ascii() {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charset_from_http_header() {
        assert_eq!(
            charset_of("text/html; charset=utf-8").as_deref(),
            Some("utf-8")
        );
        assert_eq!(charset_of("text/html;charset=GBK").as_deref(), Some("GBK"));
        assert_eq!(
            charset_of("text/html; charset=\"gb2312\"").as_deref(),
            Some("gb2312")
        );
        assert_eq!(charset_of("text/html").as_deref(), None);
    }

    #[test]
    fn charset_from_meta() {
        let m = |s: &str| meta_charset(s.as_bytes());
        assert_eq!(
            m(r#"<html><head><meta charset="gbk"></head>"#).as_deref(),
            Some("gbk")
        );
        assert_eq!(
            m(r#"<meta http-equiv="Content-Type" content="text/html; charset=gb2312">"#).as_deref(),
            Some("gb2312")
        );
        assert_eq!(m("<html><head></head>"), None);
    }

    #[test]
    fn decode_gbk_page() {
        // 「顶点小说」线上就是 gb2312
        let (gbk, _, _) = encoding_rs::GBK.encode("第一章 血尸");
        let s = decode(&gbk, Some("text/html; charset=gb2312"));
        assert_eq!(s, "第一章 血尸");
        // 没有 charset 头，靠 meta 或「不是 UTF-8」退到 GBK
        let html: Vec<u8> = [&b"<meta charset=gbk><p>"[..], &gbk, &b"</p>"[..]].concat();
        assert!(decode(&html, None).contains("第一章 血尸"));
        // 连 meta 都没有，就只能靠「GBK 解得出、UTF-8 解不出」这个判据
        assert!(decode(&gbk, None).contains("第一章"));
    }

    #[test]
    fn decode_utf8_by_default_and_by_bom() {
        let s = "中文内容";
        assert_eq!(decode(s.as_bytes(), None), s);
        assert_eq!(decode(s.as_bytes(), Some("text/html")), s);
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(s.as_bytes());
        assert_eq!(decode(&with_bom, None), s, "BOM 不能留在正文里");
        assert_eq!(decode(b"", None), "");
    }

    #[test]
    fn absolute_urls() {
        let b = "http://www.daomubiji.com/dao-mu-bi-ji-1";
        assert_eq!(
            absolute(b, "qi-xing-lu-wang-01.html"),
            "http://www.daomubiji.com/qi-xing-lu-wang-01.html"
        );
        assert_eq!(
            absolute(b, "/a/b.html"),
            "http://www.daomubiji.com/a/b.html"
        );
        assert_eq!(absolute(b, "http://other.com/x"), "http://other.com/x");
        assert_eq!(absolute(b, "https://other.com/x"), "https://other.com/x");
        // 协议相对
        assert_eq!(absolute(b, "//cdn.com/x.png"), "http://cdn.com/x.png");
        assert_eq!(
            absolute("https://a.com/x/", "//cdn.com/i.png"),
            "https://cdn.com/i.png"
        );
        // 目录基准
        assert_eq!(
            absolute("https://a.com/x/y/z", "p.html"),
            "https://a.com/x/y/p.html"
        );
        assert_eq!(
            absolute("https://a.com/x/y/", "p.html"),
            "https://a.com/x/y/p.html"
        );
        // 没有路径
        assert_eq!(absolute("https://a.com", "p.html"), "https://a.com/p.html");
        // . 和 ..
        assert_eq!(
            absolute("https://a.com/x/y/z", "../p.html"),
            "https://a.com/x/p.html"
        );
        assert_eq!(
            absolute("https://a.com/x/y/z", "./p.html"),
            "https://a.com/x/y/p.html"
        );
        // query / fragment
        assert_eq!(
            absolute("https://a.com/x?p=1", "?p=2"),
            "https://a.com/x?p=2"
        );
        assert_eq!(
            absolute("https://a.com/x?p=1", "#top"),
            "https://a.com/x#top"
        );
        // javascript: 丢掉（legado 也丢）
        assert_eq!(absolute(b, "javascript:void(0)"), "");
        assert_eq!(absolute(b, "  "), "");
        // 基准本身不是 URL 时只能原样返回
        assert_eq!(absolute("不是URL", "p.html"), "p.html");
    }

    #[test]
    fn url_options_are_split_off() {
        let (u, o) = split_url_option("http://x/s.php?s=1");
        assert_eq!(u, "http://x/s.php?s=1");
        assert!(o.is_none());

        let (u, o) = split_url_option(
            r#"http://x/s.php , {"method":"POST","body":"key={{key}}","charset":"gbk"}"#,
        );
        assert_eq!(u, "http://x/s.php");
        let o = UrlOption::parse(o.as_deref());
        assert_eq!(o.method.as_deref(), Some("POST"));
        assert_eq!(o.body.as_deref(), Some("key={{key}}"));
        assert_eq!(o.charset.as_deref(), Some("gbk"));

        // 逗号出现在 query 里时不是选项分隔符
        let (u, o) = split_url_option("http://x/s?q=a,b&p=1");
        assert_eq!(u, "http://x/s?q=a,b&p=1");
        assert!(o.is_none());
        // 坏 JSON 不能崩，当没有选项
        let (u, o) = split_url_option("http://x/,{不是json}");
        assert_eq!(u, "http://x/");
        let o = UrlOption::parse(o.as_deref());
        assert!(o.method.is_none());
    }

    #[test]
    fn url_option_accepts_single_quoted_json() {
        // 真书源「手机小说」「就爱文学」就是这么写的
        let (u, o) = split_url_option(
            r#"/modules/article/search.php,{'charset':'gbk','method':'post','body':'searchkey={{key}}'}"#,
        );
        assert_eq!(u, "/modules/article/search.php");
        let o = UrlOption::parse(o.as_deref());
        assert_eq!(o.charset.as_deref(), Some("gbk"));
        assert_eq!(o.method.as_deref(), Some("post"));
        assert_eq!(o.body.as_deref(), Some("searchkey={{key}}"));
    }

    #[test]
    fn url_option_headers_accept_object_or_string() {
        let (_, o) = split_url_option(r#"http://x/,{"headers":{"Referer":"http://a/"}}"#);
        assert_eq!(
            UrlOption::parse(o.as_deref()).headers(),
            vec![("Referer".to_string(), "http://a/".to_string())]
        );
        let (_, o) = split_url_option(r#"http://x/,{"headers":"{\"Referer\":\"http://a/\"}"}"#);
        assert_eq!(
            UrlOption::parse(o.as_deref()).headers(),
            vec![("Referer".to_string(), "http://a/".to_string())]
        );
        // 不认识的字段不能让它整条解析失败
        let (_, o) =
            split_url_option(r#"http://x/,{"method":"GET","dnsIp":"1.2.3.4","serverID":7}"#);
        assert_eq!(
            UrlOption::parse(o.as_deref()).method.as_deref(),
            Some("GET")
        );
    }

    #[test]
    fn form_body_encoding() {
        assert_eq!(form_bytes("a=1&b=2", None), b"a=1&b=2".to_vec());
        assert_eq!(
            form_bytes("key=斗破", None),
            b"key=%E6%96%97%E7%A0%B4".to_vec()
        );
        // 指定编码：GBK 站点要发 GBK 字节
        assert_eq!(
            form_bytes("key=斗破", Some("gbk")),
            b"key=%B6%B7%C6%C6".to_vec()
        );
        // 已经编码过的原样保留
        assert_eq!(form_bytes("key=%20a", None), b"key=%20a".to_vec());
        assert_eq!(form_bytes("search", None), b"search".to_vec());
        assert_eq!(form_bytes("", None), Vec::<u8>::new());
    }

    #[test]
    fn non_ascii_is_percent_encoded() {
        assert_eq!(
            encode_non_ascii("http://x/s?q=斗破&p=1"),
            "http://x/s?q=%E6%96%97%E7%A0%B4&p=1"
        );
        // 纯 ASCII 原样，且不二次编码
        assert_eq!(encode_non_ascii("http://x/a%20b"), "http://x/a%20b");
    }
}

// ── URL 选项 ────────────────────────────────────────────────────────────

/// `url,{...}` 里那一段 JSON。字段名照抄 legado 的 `UrlOption`。
#[derive(Debug, Default, Deserialize)]
pub struct UrlOption {
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub charset: Option<String>,
    /// 对象，或者装着对象的 JSON 字符串（GSON 两种都收）
    #[serde(default)]
    pub headers: Option<serde_json::Value>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub retry: Option<u32>,
    /// 拿到响应后跑一段 JS 改结果 —— 我们不跑
    #[serde(default, rename = "bodyJs")]
    pub body_js: Option<String>,
    #[serde(default, rename = "webJs")]
    pub web_js: Option<String>,
    #[serde(default, rename = "webView")]
    pub web_view: Option<serde_json::Value>,
    /// 拿 URL 本身再跑一段 JS —— 我们不跑
    #[serde(default)]
    pub js: Option<String>,
}

impl UrlOption {
    /// 从选项 JSON 原文解析。坏 JSON / 缺选项都给默认值，绝不因为选项坏了就不发请求。
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(r) = raw else {
            return Self::default();
        };
        serde_json::from_str(r)
            .ok()
            // GSON 的宽松模式把单引号也当字符串定界符，所以真书源里确实有
            // `{'charset':'gbk','body':'searchkey={{key}}'}` 这种写法（123 个源）。
            // serde_json 不认，就地兜一下。
            .or_else(|| serde_json::from_str(&r.replace('\'', "\"")).ok())
            .unwrap_or_default()
    }

    pub fn headers(&self) -> Vec<(String, String)> {
        let value = match &self.headers {
            Some(serde_json::Value::Object(m)) => serde_json::Value::Object(m.clone()),
            Some(serde_json::Value::String(s)) => match serde_json::from_str(s) {
                Ok(v) => v,
                Err(_) => return Vec::new(),
            },
            _ => return Vec::new(),
        };
        let serde_json::Value::Object(m) = value else {
            return Vec::new();
        };
        m.into_iter()
            .filter_map(|(k, v)| Some((k, v.as_str()?.to_string())))
            .collect()
    }
}

/// 按 `\s*,\s*(?=\{)` 把 URL 和 `{...}` 选项切开。
///
/// 返回选项的**原文**而不是解析结果 —— 调用方可能要先把 URL 拼成绝对地址，
/// 再把选项原样接回去。
pub fn split_url_option(spec: &str) -> (String, Option<String>) {
    let spec = spec.trim();
    let b = spec.as_bytes();
    for i in 0..b.len() {
        if b[i] != b',' {
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j) == Some(&b'{') {
            return (spec[..i].trim().to_string(), Some(spec[j..].to_string()));
        }
    }
    (spec.to_string(), None)
}

/// 把 `k=v&k2=v2` 编成请求体。已经百分号编码过的片段原样保留（legado 也这么判）。
fn form_bytes(body: &str, charset: Option<&str>) -> Vec<u8> {
    let enc = charset
        .and_then(|c| encoding_rs::Encoding::for_label(c.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let mut out = Vec::new();
    for (i, pair) in body.split('&').enumerate() {
        if i > 0 {
            out.push(b'&');
        }
        let (k, v) = match pair.find('=') {
            Some(at) => (&pair[..at], Some(&pair[at + 1..])),
            None => (pair, None),
        };
        out.extend(percent_encode(k, enc));
        if let Some(v) = v {
            out.push(b'=');
            out.extend(percent_encode(v, enc));
        }
    }
    out
}

/// 按目标编码逐字符转义。保留 `A-Za-z0-9-_.~`，其余变成 `%XX`。
fn percent_encode(s: &str, enc: &'static encoding_rs::Encoding) -> Vec<u8> {
    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // 已经是 %XX 的序列别再编一次
        if c == '%'
            && i + 2 < chars.len()
            && chars[i + 1].is_ascii_hexdigit()
            && chars[i + 2].is_ascii_hexdigit()
        {
            out.push(b'%');
            i += 1;
            continue;
        }
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c as u8);
        } else {
            let text = c.to_string();
            let (bytes, _, _) = enc.encode(&text);
            for b in bytes.iter() {
                out.extend_from_slice(format!("%{b:02X}").as_bytes());
            }
        }
        i += 1;
    }
    out
}
