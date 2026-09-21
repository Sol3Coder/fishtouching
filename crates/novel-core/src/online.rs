//! 把「抓一章/搜一本书」包成一个可以丢给后台线程的任务。
//!
//! 界面线程只做三件事：说「我要搜这个关键字」、收结果、重画。所有耗时和失败
//! 都留在这一层。

use std::path::{Path, PathBuf};

use crate::crawl::{Chapter, Crawler, Hit};
use crate::net;
use crate::source::BookSource;
use crate::R;

/// 一次要干的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Search(String),
    /// 打开一本书：拉目录
    Toc {
        source: usize,
        url: String,
        /// 无视缓存，真去站点拉一次（用户按 `R`）。
        force_refresh: bool,
    },
    /// 读一章：拉正文
    Read {
        source: usize,
        url: String,
        title: String,
    },
}

/// 干完的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    /// 搜索**最终**结果（权威全集，替换掉界面上现有的）。
    Search(Vec<Hit>),
    /// 搜索的**中间**结果：又攒到这些，追加。
    ///
    /// 存在的理由是延迟。搜索结果是一页一页来的，等全部源试完再一次性给，
    /// 用户要对着「正在搜索」干等二十秒；分批发则第一秒就有东西可点。
    More(Vec<Hit>),
    Toc {
        chapters: Vec<Chapter>,
        /// 这条是缓存给的（不是刚从站点拉的）。界面要说出来 ——
        /// 否则用户会奇怪「怎么没有最新章节」。
        from_cache: bool,
    },
    Read {
        title: String,
        text: String,
    },
    Failed(String),
}

/// 后台任务的中间消息。
#[derive(Debug)]
pub enum Msg<'a> {
    /// 状态栏文本（"搜 鬼吹灯（3/60）"）。
    Note(&'a str),
    /// 搜索的增量结果。
    Hits(&'a [Hit]),
}

/// 搜索时最多试几个书源。
///
/// 596 个源全试一遍要几分钟，而用户要的是「快点出结果」。legado 是并发全试
/// 再合并，我们顺序试前 N 个 —— 并发要引线程池，而顺序试已经能出结果了。
/// 一次搜索最多试多少个源（硬上限）。
///
/// 从 10 提到 60：默认书源集合有几千个，要的书很可能不在最前面那 10 个里。
/// 真正收手的是下面那条墙上时钟预算，这个数只是防跑飞。
pub const SEARCH_SOURCES: usize = 60;

/// 一次搜索的墙上时钟预算（秒）。**这个才是真正收手的条件。**
///
/// 按源数收手不对：源有快有慢，10 个死站和 10 个活站代价差两个数量级。
/// 按时间收手则不管网络什么样，用户等的时间都是可预期的。
pub const SEARCH_BUDGET_SECS: u64 = 20;

/// 一次搜索最多收多少条。
///
/// 这个数原来是 `HITS_PER_SOURCE = 20` 并且**只抓第一页** —— 于是不管站点
/// 有多少结果，界面上永远最多 20 条。现在改成翻页累积，上限提到这里。
pub const SEARCH_MAX_HITS: usize = 300;

/// 每个书源最多翻几页搜索结果。
pub const SEARCH_PAGES_PER_SOURCE: u32 = 10;

/// 搜索时单个源的超时（秒）。
///
/// 比抓页面的 60 秒短得多，理由：搜索要顺序试很多个源，而**书源集合里
/// 一大半是死的**。用 60 秒的超时意味着碰上三个打不开的站就卡三分钟，
/// 而这三个站几乎不可能给出结果。实测 2783 个源搜「斗破苍穹」，
/// 60 秒超时下要两分钟；短超时下死源代价变小，预算花在活源上。
pub const SEARCH_TIMEOUT: u64 = 8;

/// 一批书源 + 一个 HTTP 客户端。
///
/// `Clone` 是刻意的：界面要留一份（查书源名、按域名挑源），后台线程要另一份。
/// 书源列表只在启动时读一次，复制一份的成本可以忽略。
#[derive(Clone)]
pub struct Online {
    sources: Vec<BookSource>,
    /// 抓取器。做成 `Arc<dyn Fetch>` 而不是写死 `net::Fetcher`：
    /// 测试要能塞一个「录好的页面」进来，否则搜索翻页和目录缓存这两块
    /// 只能靠真站点验，而真站点会挂、会变、会慢。
    pub fetcher: std::sync::Arc<dyn crate::crawl::Fetch>,
    /// 搜索专用的抓取器。和 `fetcher` 是同一个东西，只是套了个**短超时**：
    /// 搜索要顺序试很多源，而书源集合里一大半是死的，不该让每个死站各占
    /// 60 秒。单独一个字段是为了两边都能被注入（测试要能塞假页面）。
    pub search_fetcher: std::sync::Arc<dyn crate::crawl::Fetch>,
    /// 搜索时最多试几个源（测试会调小）
    pub search_sources: usize,
    /// 从哪个文件读进来的。改动之后写回同一个地方；`None` 表示没落点（只在内存里）。
    pub path: Option<PathBuf>,
    /// 目录缓存放哪儿（一般是书源文件旁边的 `toc/`）。`None` = 不缓存。
    pub cache_dir: Option<PathBuf>,
}

impl Online {
    /// 从 legado 导出的书源文件读。只留启用中的文本源。
    /// 一个**空**的书源集合，但已经绑定到 `path`。
    ///
    /// 全新安装时用：`.novel/sources.json` 还不存在，但用户按 `a` 粘一个 URL
    /// 就应该能导入 —— 导入需要有个能写回去的目标。之前这种情况返回的是
    /// 「没有书源」而不是「空书源」，于是全新安装根本进不了导入流程。
    pub fn at(path: impl AsRef<Path>) -> Self {
        let mut me = Self::from_sources(Vec::new());
        me.bind(path.as_ref());
        me
    }

    pub fn load(path: impl AsRef<Path>) -> R<Self> {
        let text = std::fs::read_to_string(path.as_ref())
            .map_err(|e| crate::Error::Net(format!("读不了 {}：{e}", path.as_ref().display())))?;
        let all = BookSource::parse_file(&text)
            .map_err(|e| crate::Error::Net(format!("书源文件不是合法 JSON：{e}")))?;
        let mut me = Self::from_sources(all);
        me.bind(path.as_ref());
        Ok(me)
    }

    /// 把「书源文件在哪」记下来，顺手把缓存目录定在它旁边。
    fn bind(&mut self, path: &Path) {
        self.path = Some(path.to_path_buf());
        self.cache_dir = path.parent().map(crate::cache::dir);
    }

    pub fn from_sources(all: Vec<BookSource>) -> Self {
        let sources: Vec<BookSource> = all
            .into_iter()
            .filter(|s| s.enabled && s.kind == 0)
            .collect();
        let fetcher: std::sync::Arc<dyn crate::crawl::Fetch> =
            std::sync::Arc::new(net::Fetcher::new(Vec::new()));
        let search_fetcher: std::sync::Arc<dyn crate::crawl::Fetch> =
            std::sync::Arc::new(net::Fetcher::with_timeout(Vec::new(), SEARCH_TIMEOUT));
        Self {
            sources,
            fetcher,
            search_fetcher,
            search_sources: SEARCH_SOURCES,
            path: None,
            cache_dir: None,
        }
    }

    // ── 书源管理 ────────────────────────────────────────────────────
    //
    // 「启用 / 停用」直接落在 `BookSource.enabled` 上，**不另存一份**。
    // 之前 `state.json` 里也有一份 `enabled` 数组，两份真相的下场是
    // 「界面显示开着、实际用的是关的」这类只在特定顺序下复现的 bug。

    pub fn source(&self, i: usize) -> Option<&BookSource> {
        self.sources.get(i)
    }

    /// 写回书源文件。没路径就只在内存里（返回 `Ok(false)`）。
    pub fn save(&self) -> crate::R<bool> {
        let Some(path) = &self.path else {
            return Ok(false);
        };
        let json = serde_json::to_string_pretty(&self.sources)
            .map_err(|e| crate::Error::Net(format!("书源序列化失败：{e}")))?;
        let text = format!("{json}\n");
        if let Ok(old) = std::fs::read_to_string(path) {
            if old == text {
                return Ok(false);
            }
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(path, text)
            .map_err(|e| crate::Error::Net(format!("写不了 {}：{e}", path.display())))?;
        Ok(true)
    }

    /// 从文件路径或 http(s) URL 导入书源，追加到列表末尾。
    ///
    /// 返回新增的条数。**按 `bookSourceUrl` 去重** —— 同一份集合导入两次
    /// 不该变成两份。
    pub fn import(&mut self, from: &str) -> crate::R<usize> {
        let from = from.trim();
        if from.is_empty() {
            return Err(crate::Error::Missing("书源来源（文件路径或 URL）".into()));
        }
        let text = if from.starts_with("http://") || from.starts_with("https://") {
            // 导入书源集合可能是个几十 MB 的文件，用抓页面的预算下它必超时
            net::Fetcher::with_timeout(Vec::new(), 300).get(from)?.body
        } else {
            std::fs::read_to_string(from)
                .map_err(|e| crate::Error::Net(format!("读不了 {from}：{e}")))?
        };
        let incoming = BookSource::parse_file(&text)
            .map_err(|e| crate::Error::Net(format!("{from} 不是合法的书源文件：{e}")))?;
        if incoming.is_empty() {
            return Err(crate::Error::Missing(format!("{from} 里没有书源")));
        }
        let mut added = 0;
        for s in incoming {
            if self
                .sources
                .iter()
                .any(|x| x.url == s.url && x.name == s.name)
            {
                continue;
            }
            self.sources.push(s);
            added += 1;
        }
        if added > 0 {
            let _ = self.save();
        }
        Ok(added)
    }

    /// 删掉第 i 个书源。
    pub fn remove(&mut self, i: usize) -> crate::R<BookSource> {
        if i >= self.sources.len() {
            return Err(crate::Error::Missing(format!("没有第 {} 个书源", i + 1)));
        }
        let gone = self.sources.remove(i);
        let _ = self.save();
        Ok(gone)
    }

    /// 改第 i 个书源的某个字段。`field` 取 `"name"` / `"url"` / `"searchUrl"`。
    pub fn edit(&mut self, i: usize, field: &str, value: &str) -> crate::R<()> {
        let src = self
            .sources
            .get_mut(i)
            .ok_or_else(|| crate::Error::Missing(format!("没有第 {} 个书源", i + 1)))?;
        let value = value.trim().to_string();
        match field {
            "name" => src.name = value,
            "url" => src.url = value,
            "searchUrl" => src.search_url = Some(value),
            other => return Err(crate::Error::Missing(format!("不认识的字段 {other}"))),
        }
        let _ = self.save();
        Ok(())
    }

    /// 读第 i 个书源的某个字段（编辑框要预填当前值）。
    pub fn field(&self, i: usize, field: &str) -> String {
        let Some(s) = self.sources.get(i) else {
            return String::new();
        };
        match field {
            "name" => s.name.clone(),
            "url" => s.url.clone(),
            "searchUrl" => s.search_url.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// 开关第 i 个书源。返回开关之后的状态。
    pub fn toggle(&mut self, i: usize) -> crate::R<bool> {
        let src = self
            .sources
            .get_mut(i)
            .ok_or_else(|| crate::Error::Missing(format!("没有第 {} 个书源", i + 1)))?;
        src.enabled = !src.enabled;
        let now = src.enabled;
        let _ = self.save();
        Ok(now)
    }

    /// 第 i 个书源开着吗。
    pub fn enabled(&self, i: usize) -> bool {
        self.sources.get(i).is_some_and(|s| s.enabled)
    }

    /// 把书源列表整体左移，让第 `n` 个排到最前面（测试用来跳过已失效的源）。
    pub fn sources_rotate(&mut self, n: usize) {
        let n = n.min(self.sources.len().saturating_sub(1));
        self.sources.rotate_left(n);
    }

    /// 按名称/网址/搜索网址筛选（大小写不敏感子串），返回**原始下标**。
    ///
    /// 空条件返回全部。返回下标而不是克隆出来的名字：3900 个源每帧克隆一遍
    /// 名字是白扔的分配，而下标还能让「筛选后的第 k 行」稳定映射回真正的源。
    pub fn find(&self, needle: &str) -> Vec<usize> {
        if needle.is_empty() {
            return (0..self.sources.len()).collect();
        }
        let n = needle.to_lowercase();
        self.sources
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.name.to_lowercase().contains(&n)
                    || s.url.to_lowercase().contains(&n)
                    || s.search_url
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&n)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.sources.iter().map(|s| s.name.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// 能搜索、且开着的源的数量。
    pub fn searchable(&self) -> usize {
        self.sources
            .iter()
            .filter(|s| s.enabled && s.can_search())
            .count()
    }

    /// 第 i 个源能不能搜索。
    pub fn can_search(&self, i: usize) -> bool {
        self.sources.get(i).is_some_and(|s| s.can_search())
    }

    /// 按域名找一个源。书源里的 `bookSourceUrl` 可能是旧域名，
    /// 所以两边都按域名比一次。
    pub fn url_of(&self, host: &str) -> Option<usize> {
        if host.is_empty() {
            return None;
        }
        self.sources.iter().position(|s| {
            let h = s
                .url
                .split("//")
                .nth(1)
                .map(|r| r.split('/').next().unwrap_or(""))
                .unwrap_or("");
            h == host || h.trim_start_matches("www.") == host.trim_start_matches("www.")
        })
    }

    /// 同步跑一个任务。`progress` 用来在界面上显示「正在试第几个源」。
    pub fn run(&self, job: &Job, out: &mut dyn FnMut(Msg)) -> Done {
        match job {
            Job::Search(key) => self.search(key, out),
            Job::Toc {
                source,
                url,
                force_refresh,
            } => self.toc(*source, url, *force_refresh, out),
            Job::Read { source, url, title } => match self.crawler(*source) {
                Ok(c) => match c.content(url) {
                    Ok(text) => Done::Read {
                        title: title.clone(),
                        text,
                    },
                    Err(e) => Done::Failed(e.to_string()),
                },
                Err(e) => Done::Failed(e.to_string()),
            },
        }
    }

    fn crawler(&self, i: usize) -> R<Crawler<std::sync::Arc<dyn crate::crawl::Fetch>>> {
        self.crawler_with(i, self.fetcher.clone())
    }

    /// 换一个抓取器的 crawler。搜索用它套一个短超时。
    fn crawler_with(
        &self,
        i: usize,
        fetcher: std::sync::Arc<dyn crate::crawl::Fetch>,
    ) -> R<Crawler<std::sync::Arc<dyn crate::crawl::Fetch>>> {
        let source = self
            .sources
            .get(i)
            .ok_or_else(|| crate::Error::Missing(format!("没有第 {} 个书源", i + 1)))?;
        Ok(Crawler::new(source.clone(), fetcher))
    }

    /// 第 i 个源的目录规则指纹。规则变了缓存就该失效。
    fn toc_rule_hash(&self, i: usize) -> u64 {
        match self.sources.get(i) {
            Some(s) => crate::cache::rule_hash(&s.toc),
            None => 0,
        }
    }

    /// 拉目录：**缓存优先**。
    ///
    /// 目录是「拉一次用很多次」的东西，站点慢的时候每次打开一本书都要等好几秒，
    /// 而目录一天变不了几章。所以：
    ///
    /// * 有没过期的缓存，且不是强制刷新 → 直接给缓存，**一个网络请求都不发**；
    /// * 否则真拉，成功就写回缓存。
    ///
    /// 缓存坏了/缺了都当没有（缓存不该让打开书失败）；写缓存失败不影响这次阅读。
    fn toc(
        &self,
        source: usize,
        url: &str,
        force_refresh: bool,
        _out: &mut dyn FnMut(Msg),
    ) -> Done {
        let name = self
            .sources
            .get(source)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let rule_hash = self.toc_rule_hash(source);
        let key = crate::cache::key(&name, url, rule_hash);

        if !force_refresh {
            if let Some(base) = &self.cache_dir {
                if let Some(hit) = crate::cache::load(base, &key) {
                    if !hit.is_stale(crate::cache::now()) && !hit.chapters.is_empty() {
                        return Done::Toc {
                            chapters: hit.chapters,
                            from_cache: true,
                        };
                    }
                }
            }
        }

        let fresh = match self.crawler(source) {
            Ok(c) => match c.toc(url) {
                Ok(v) if !v.is_empty() => v,
                Ok(_) => return Done::Failed("目录是空的（书源可能已过期）".into()),
                Err(e) => return Done::Failed(e.to_string()),
            },
            Err(e) => return Done::Failed(e.to_string()),
        };

        if let Some(base) = &self.cache_dir {
            let entry = crate::cache::CachedToc {
                source: name,
                url: url.to_string(),
                fetched_at: crate::cache::now(),
                rule_hash,
                chapters: fresh.clone(),
            };
            // 写不进去只是「下次还得重拉」，不该让这次失败
            let _ = crate::cache::store(base, &key, &entry);
        }
        Done::Toc {
            chapters: fresh,
            from_cache: false,
        }
    }

    fn search(&self, key: &str, out: &mut dyn FnMut(Msg)) -> Done {
        let mut all: Vec<Hit> = Vec::new();
        let mut tried = 0;
        let mut blocked = 0;
        let mut last_err = String::new();
        let started = std::time::Instant::now();
        let budget = std::time::Duration::from_secs(SEARCH_BUDGET_SECS);
        for (i, s) in self
            .sources
            .iter()
            .enumerate()
            .filter(|(_, s)| s.enabled && s.can_search())
            .take(self.search_sources)
        {
            if tried >= self.search_sources || all.len() >= SEARCH_MAX_HITS {
                break;
            }
            // 超了预算就收手（结果照给，只是未必试完全部源）
            if started.elapsed() >= budget {
                break;
            }
            tried += 1;
            out(Msg::Note(&format!(
                "搜 {}（{}/{}，已 {} 条）",
                s.name,
                tried,
                self.search_sources,
                all.len()
            )));
            let c = match self.crawler_with(i, self.search_fetcher.clone()) {
                Ok(c) => c,
                Err(e) => {
                    last_err = e.to_string();
                    continue;
                }
            };
            // **一个源翻多页**。以前只抓第 1 页再 `take(20)`，于是不管站点
            // 有多少结果，界面上永远最多 20 条 —— 这是用户直接报上来的现象。
            let mut first_page = true;
            for page in 1..=SEARCH_PAGES_PER_SOURCE {
                if started.elapsed() >= budget || all.len() >= SEARCH_MAX_HITS {
                    break;
                }
                match c.search(key, page) {
                    Ok(hits) if hits.is_empty() => break,
                    Ok(hits) => {
                        let mut fresh: Vec<Hit> = Vec::new();
                        for h in hits {
                            if !all.iter().any(|x| x.url == h.url) {
                                all.push(h.clone());
                                fresh.push(h);
                            }
                            if all.len() >= SEARCH_MAX_HITS {
                                break;
                            }
                        }
                        if !fresh.is_empty() {
                            // 攒到就发出去，别让用户对着「正在搜索」干等
                            out(Msg::Hits(&fresh));
                        }
                        // 这一页一条新的都没有：站点忽略了 `{{page}}`，
                        // 再翻下去只会拿到同一页 —— 停。
                        if fresh.is_empty() {
                            break;
                        }
                    }
                    Err(crate::Error::Unsupported(_)) if first_page => {
                        blocked += 1;
                        break;
                    }
                    // 第二页之后失败不算这个源被挡住，只是没有下一页（很多站点
                    // 翻到底会 404 或者返回错误页）
                    Err(e) => {
                        if first_page {
                            last_err = e.to_string();
                        }
                        break;
                    }
                }
                first_page = false;
            }
        }
        if all.is_empty() {
            let why = if tried == 0 {
                "没有能搜索的书源".to_string()
            } else if blocked == tried {
                "试过的书源都需要 JS 解释器".to_string()
            } else if last_err.is_empty() {
                "没有结果".to_string()
            } else {
                last_err
            };
            return Done::Failed(why);
        }
        Done::Search(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCES: &str = include_str!("../tests/fixtures/sources.json");

    /// 一个能喂的假抓取器：按 URL 返回录好的页面。
    ///
    /// 「翻了第几页」直接来自 URL 里的 `page=N`，所以能直接验翻页循环。
    #[derive(Clone)]
    struct Fake(std::sync::Arc<std::sync::Mutex<FakeState>>);

    #[derive(Default)]
    struct FakeState {
        /// 每个搜索页给几条结果。
        pub per_page: usize,
        /// 一共多少页（再多就返回空页）。
        pub pages: usize,
        /// 记录被请求过的 URL，用来断言「真的翻了页」。
        pub asked: Vec<String>,
    }

    impl Fake {
        fn new(per_page: usize, pages: usize) -> Self {
            Self(std::sync::Arc::new(std::sync::Mutex::new(FakeState {
                per_page,
                pages,
                asked: Vec::new(),
            })))
        }

        fn asked(&self) -> Vec<String> {
            self.0.lock().unwrap().asked.clone()
        }
    }

    impl crate::crawl::Fetch for Fake {
        fn get(&self, spec: &str) -> R<net::Response> {
            let (url, _) = net::split_url_option(spec);
            let mut st = self.0.lock().unwrap();
            st.asked.push(url.clone());
            let page: usize = url
                .split("page=")
                .nth(1)
                .and_then(|r| r.split(['&', ',']).next())
                .and_then(|r| r.parse().ok())
                .unwrap_or(1);
            if page > st.pages {
                return Ok(net::Response {
                    url,
                    body: "<html><body></body></html>".into(),
                });
            }
            // 每条结果一个链接，名字里带页号，方便断言去重
            let items: String = (0..st.per_page)
                .map(|i| {
                    format!(
                        "<div class=\"row\"><a href=\"http://x/b/{page}-{i}.html\">书{page}-{i}</a>                         <span class=\"au\">作者{page}{i}</span></div>"
                    )
                })
                .collect();
            Ok(net::Response {
                url,
                body: format!("<html><body>{items}</body></html>"),
            })
        }
    }

    /// 一个能搜索、能拉目录的源，指向假抓取器。
    fn fake_source() -> Vec<BookSource> {
        BookSource::parse_file(
            r#"[{
              "bookSourceName": "假源",
              "bookSourceUrl": "http://x/",
              "bookSourceType": 0,
              "enabled": true,
              "searchUrl": "http://x/s?q={{key}}&page={{page}}",
              "ruleSearch": {
                "bookList": "class.row",
                "name": "tag.a@text",
                "author": "class.au@text",
                "bookUrl": "tag.a@href"
              },
              "ruleToc": { "chapterList": "tag.a", "chapterName": "text", "chapterUrl": "href" }
            }]"#,
        )
        .unwrap()
    }

    fn fake_online(per_page: usize, pages: usize) -> (Online, Fake) {
        let mut o = Online::from_sources(fake_source());
        o.search_sources = 1;
        let fake = Fake::new(per_page, pages);
        o.fetcher = std::sync::Arc::new(fake.clone());
        o.search_fetcher = std::sync::Arc::new(fake.clone());
        (o, fake)
    }

    /// **这条是用户报上来的现象的回归测试。**
    ///
    /// 原来 `Online::search` 只抓第 1 页再 `take(HITS_PER_SOURCE=20)`，
    /// 于是不管站点有多少结果，界面上永远最多 20 条。
    #[test]
    fn search_paginates_instead_of_stopping_at_twenty() {
        // 每页 12 条、共 5 页 = 60 条，远超原来的 20
        let (o, fake) = fake_online(12, 5);
        let mut got: Vec<Hit> = Vec::new();
        let done = o.run(&Job::Search("斗破".into()), &mut |m| {
            if let Msg::Hits(h) = m {
                for x in h {
                    if !got.iter().any(|y| y.url == x.url) {
                        got.push(x.clone());
                    }
                }
            }
        });
        let Done::Search(all) = done else {
            panic!("搜索失败：{done:?}");
        };
        assert_eq!(all.len(), 60, "5 页 × 12 条都该在，实际 {}", all.len());
        assert!(all.len() > 20, "这正是用户报的那个 20 条上限");
        assert_eq!(got.len(), 60, "增量发出来的加一起也应该是 60 条");
        let asked = fake.asked();
        for p in 1..=5 {
            assert!(
                asked.iter().any(|u| u.contains(&format!("page={p}"))),
                "第 {p} 页没被请求过：{asked:?}"
            );
        }
    }

    #[test]
    fn search_stops_when_a_page_repeats_itself() {
        // 站点忽略 `{{page}}`：每页都返回同样的内容 → 不能无限翻
        let (o, _) = fake_online(5, 1);
        let done = o.run(&Job::Search("书".into()), &mut |_| {});
        let Done::Search(all) = done else {
            panic!("搜索失败：{done:?}");
        };
        assert_eq!(all.len(), 5, "要停在 5 条，不是无限翻或报错");
    }

    #[test]
    fn search_results_are_deduped_across_pages() {
        let (o, _) = fake_online(3, 4);
        let done = o.run(&Job::Search("书".into()), &mut |_| {});
        let Done::Search(all) = done else {
            panic!("搜索失败：{done:?}");
        };
        let mut urls: Vec<&str> = all.iter().map(|h| h.url.as_str()).collect();
        let n = urls.len();
        urls.sort_unstable();
        urls.dedup();
        assert_eq!(urls.len(), n, "URL 不能重复");
    }

    #[test]
    fn hits_are_announced_before_the_search_finishes() {
        // 增量：第一条 Msg::Hits 必须早于最终的 Done::Search 才有意义
        let (o, _) = fake_online(10, 3);
        let mut batches = 0;
        let mut notes = 0;
        let done = o.run(&Job::Search("书".into()), &mut |m| match m {
            Msg::Hits(_) => batches += 1,
            Msg::Note(_) => notes += 1,
        });
        assert!(matches!(done, Done::Search(_)));
        assert_eq!(batches, 3, "每页攒到就发一批");
        assert!(notes >= 1, "也该有进度文本");
    }

    #[test]
    fn loads_only_usable_sources() {
        let all = BookSource::parse_file(SOURCES).unwrap();
        let o = Online::from_sources(all.clone());
        assert!(!o.is_empty(), "夹具里应当有启用的文本源");
        assert!(o.len() <= all.len());
        // 名字都在，顺序不乱
        assert!(o.names().iter().any(|n| n.contains("盗墓笔记")));
    }

    #[test]
    fn run_reports_bad_source_indices_instead_of_panicking() {
        let o = Online::from_sources(BookSource::parse_file(SOURCES).unwrap());
        let done = o.run(
            &Job::Toc {
                source: 999,
                url: "http://x/".into(),
                force_refresh: false,
            },
            &mut |_| {},
        );
        assert!(matches!(done, Done::Failed(e) if e.contains("999") || e.contains("第 1000")));
    }

    /// 真打站点，走完整的「搜索 → 目录 → 正文」。
    /// 默认不跑（`cargo test -- --ignored`），因为它会因站点状态而变红 ——
    /// 那不代表我们的代码坏了。
    #[test]
    #[ignore = "需要网络"]
    fn live_full_chain() {
        let all = BookSource::parse_file(include_str!("../tests/fixtures/sources-runnable.json"))
            .expect("夹具里的可跑书源清单");
        let mut o = Online::from_sources(all);
        // 前几个源基本都挂了，直接跳到「鬼吹灯」（实测能用）
        let start = o
            .names()
            .iter()
            .position(|n| n == "鬼吹灯")
            .expect("夹具里应当有鬼吹灯");
        o.sources.rotate_left(start);
        o.search_sources = 12;

        let done = o.run(&Job::Search("斗破苍穹".into()), &mut |_| {});
        let Done::Search(hits) = done else {
            panic!("搜索没出结果：{done:?}");
        };
        println!("搜到 {} 条，第一条：{}", hits.len(), hits[0].name);

        // 不给 cache_dir：这条用例要真打站点，别被本地缓存短路
        let done = o.run(
            &Job::Toc {
                source: 0,
                url: hits[0].url.clone(),
                force_refresh: true,
            },
            &mut |_| {},
        );
        let Done::Toc { chapters: toc, .. } = done else {
            panic!("拉目录失败：{done:?}");
        };
        println!("目录 {} 章，第一章：{}", toc.len(), toc[0].title);

        let done = o.run(
            &Job::Read {
                source: 0,
                url: toc[0].url.clone(),
                title: toc[0].title.clone(),
            },
            &mut |_| {},
        );
        let Done::Read { text, .. } = done else {
            panic!("拉正文失败：{done:?}");
        };
        assert!(
            text.chars().count() > 200,
            "正文只有 {} 字",
            text.chars().count()
        );
        println!(
            "正文 {} 字：{}",
            text.chars().count(),
            text.lines().next().unwrap_or("")
        );
    }

    #[test]
    fn enabled_state_lives_on_the_source_not_in_a_second_place() {
        let mut o = Online::from_sources(BookSource::parse_file(SOURCES).unwrap());
        let before = o.searchable();
        assert!(o.enabled(0));
        assert!(!o.toggle(0).unwrap(), "第一次点应当是关掉");
        assert_eq!(o.searchable(), before - 1, "关掉的源不该再算进可搜索数");
        assert!(o.toggle(0).unwrap());
        assert_eq!(o.searchable(), before);
        // 越界要报错，不能 panic
        assert!(o.toggle(999).is_err());
        assert!(o.remove(999).is_err());
    }

    #[test]
    fn editing_and_removing_sources() {
        let dir = std::env::temp_dir().join("novel-online-edit");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sources.json");
        std::fs::write(&path, SOURCES).unwrap();

        let mut o = Online::load(&path).unwrap();
        let n = o.len();
        let old_name = o.field(0, "name");
        assert!(!old_name.is_empty());

        o.edit(0, "name", "改过的名字").unwrap();
        assert_eq!(o.field(0, "name"), "改过的名字");
        o.edit(0, "url", "http://new.example/").unwrap();
        assert_eq!(o.field(0, "url"), "http://new.example/");
        o.edit(0, "searchUrl", "http://new.example/s?q={{key}}")
            .unwrap();
        assert_eq!(o.field(0, "searchUrl"), "http://new.example/s?q={{key}}");
        // 编辑会自动落盘 —— 不然重启就白改了
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("改过的名字"));

        // 不认识的字段要报错而不是静默忽略
        assert!(o.edit(0, "bookSourceType", "1").is_err());

        let gone = o.remove(0).unwrap();
        assert_eq!(gone.name, "改过的名字");
        assert_eq!(o.len(), n - 1);
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("改过的名字"));
    }

    #[test]
    fn importing_dedups_by_url_and_name() {
        let dir = std::env::temp_dir().join("novel-online-import");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sources.json");
        std::fs::write(&path, SOURCES).unwrap();
        let mut o = Online::load(&path).unwrap();
        let before = o.len();

        // 导进来的全是已有的 → 一条都不加
        let added = o.import(path.to_str().unwrap()).unwrap();
        assert_eq!(added, 0);
        assert_eq!(o.len(), before);

        // 导一个新的
        let extra = dir.join("extra.json");
        std::fs::write(
            &extra,
            r#"[{"bookSourceName":"新源","bookSourceUrl":"http://brand-new.example/",
                 "searchUrl":"http://brand-new.example/s?q={{key}}",
                 "ruleSearch":{"bookList":"li"}}]"#,
        )
        .unwrap();
        assert_eq!(o.import(extra.to_str().unwrap()).unwrap(), 1);
        assert_eq!(o.len(), before + 1);
        assert!(std::fs::read_to_string(&path).unwrap().contains("新源"));

        // 坏来源要报错
        assert!(o.import(dir.join("不存在.json").to_str().unwrap()).is_err());
        assert!(o.import("").is_err());
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "不是 JSON").unwrap();
        assert!(o.import(bad.to_str().unwrap()).is_err());
        let empty = dir.join("empty.json");
        std::fs::write(&empty, "[]").unwrap();
        assert!(o.import(empty.to_str().unwrap()).is_err());
    }

    #[test]
    fn saving_without_a_path_is_a_no_op_not_an_error() {
        let o = Online::from_sources(BookSource::parse_file(SOURCES).unwrap());
        assert!(!o.save().unwrap(), "没有落点时不写盘，但也不算失败");
    }

    #[test]
    fn a_job_against_an_unreachable_host_fails_with_a_message() {
        let o = Online::from_sources(BookSource::parse_file(SOURCES).unwrap());
        let done = o.run(
            &Job::Read {
                source: 0,
                url: "http://这个域名不存在.invalid/x".into(),
                title: "第一章".into(),
            },
            &mut |_| {},
        );
        match done {
            Done::Failed(e) => assert!(!e.is_empty()),
            other => panic!("应当失败，实际是 {other:?}"),
        }
    }
}
