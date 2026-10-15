//! 小说阅读器的引擎层 —— 与界面无关。
//!
//! * [`state`] —— 状态（`state.json`）与渲染结果写盘；
//! * [`shelf`] —— 书架与章节记忆（`shelf.json`）；
//! * [`render`] —— 把一章小说变成一份像样的 TS 文件，正文在 `//` 注释里；
//! * [`source`] —— legado 书源模型（`BookSource`）；
//! * [`rule`] —— legado 规则引擎（切分 / 前缀 / `##` 替换 / `{{ }}`）；
//! * [`html`] —— 对齐 JSoup 的 HTML 选择器引擎；
//! * [`net`] —— 拉页面 + 猜编码（中文站点大量是 GBK）；
//! * [`js`] —— JS 执行环境（QuickJS）+ legado 的 `java.*` 宿主 API；
//! * [`crawl`] —— 搜索 / 详情 / 目录 / 正文四步的抓取链路；
//! * [`online`] —— 把抓取包成「丢给后台线程的任务」；
//! * [`epub`] —— 本地 epub 书（书名/作者/目录/正文）；
//! * [`demo`] —— 离线演示书库，在没有可用书源时兜底。

pub mod cache;
pub mod crawl;
pub mod demo;
pub mod epub;
pub mod html;
pub mod js;
pub mod jsonpath;
pub mod net;
pub mod online;
pub mod render;
pub mod rng;
pub mod rule;
pub mod shelf;
pub mod source;
pub mod state;
pub mod xpath;

/// 引擎里所有会失败的事。刻意做得粗 —— 抓取本来就会因为网络、
/// 站点改版、书源过时而失败，调用方要的是「哪一步、为什么」，不是类型学。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 规则要用 JS / XPath，本实现不支持。带上是哪一段。
    Unsupported(String),
    /// 选择器语法不认识。
    BadSelector(String),
    /// 网络层失败（DNS、TLS、超时…）。
    Net(String),
    /// 站点返回了非 2xx。
    Http { status: u16, url: String },
    /// 页面拿到了，但规则没找到必需的东西（比如书源过期了）。
    Missing(String),
    /// JS 脚本出错（语法错、跑飞了、用了没实现的 API）。
    Js(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unsupported(what) => write!(f, "规则需要 JS 解释器，暂不支持：{what}"),
            Error::BadSelector(s) => write!(f, "选择器不认识：{s}"),
            Error::Net(e) => write!(f, "网络错误：{e}"),
            Error::Http { status, url } => write!(f, "HTTP {status}：{url}"),
            Error::Missing(what) => write!(f, "页面上没找到 {what}（书源可能已过期）"),
            Error::Js(e) => write!(f, "JS 出错：{e}"),
        }
    }
}

impl std::error::Error for Error {}

pub type R<T> = Result<T, Error>;

pub use render::{render, Book, Chapter};
pub use state::{ReaderMode, Session, State, READER_PATH, STATE_PATH};
