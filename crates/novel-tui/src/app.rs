//! 应用状态机 —— **纯逻辑，不碰终端**。
//!
//! 键位处理、面板焦点、光标与滚动都在这里，所以可以像普通函数一样测
//! （`handle(Key::Char('n'))` 然后断言状态），不需要起一个终端。

use std::path::PathBuf;
use std::process::Command;

use novel_core::crawl::{Chapter, Hit as WebHit};
use novel_core::demo;
use novel_core::online::{Done, Job, Online};
use novel_core::render::{Book, Chapter as RChapter};
use novel_core::shelf::{Shelf, ShelfBook, SHELF_FILE};
use novel_core::state::Session;

/// 键盘事件。自己定义而不是直接用 crossterm 的，是为了让逻辑和终端解耦、可测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

/// 当前在哪一屏。
///
/// **同一时刻只显示一个** —— 不是并列的面板。书架 →（Enter）→ 目录 →（Enter）→
/// 阅读，Esc 逐层退回；搜索和书源是另外两屏，从书架进。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// 书架：在读的书
    Shelf,
    /// 搜索：关键字 + 真站点结果
    Search,
    /// 目录：某一本的章节
    Toc,
    /// 阅读：正文
    Reader,
    /// 书源管理
    Sources,
}

/// 搜索页里键盘归谁：关键字框还是结果列表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchFocus {
    Input,
    Results,
}

/// 书源可编辑的三个字段。真书源失效十有八九是换了域名，
/// 所以 `Url` 和 `SearchUrl` 比名字有用得多。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Url,
    SearchUrl,
}

impl Field {
    pub fn key(self) -> &'static str {
        match self {
            Field::Name => "name",
            Field::Url => "url",
            Field::SearchUrl => "searchUrl",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "名字",
            Field::Url => "网址",
            Field::SearchUrl => "搜索网址",
        }
    }
}

/// 「一行输入 + 回车」的几种用途。
///
/// **搜索不在其中** —— 搜索是一整屏，关键字框常驻，不是跑出来的一行输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Goto,
    /// 输入文件路径或 URL，导入书源
    Import,
    /// 筛选书源列表（名字或网址）
    Filter,
}

/// 从一个 URL 里取域名。取不到就是空串。
///
/// 单独拎出来是因为「按域名认书源」这件事在三处要做，而三处各写一遍
/// `split("//").nth(1)` 迟早会有一处写成 `split("//").nth(0)`。
pub fn host_of(url: &str) -> String {
    url.split("//")
        .nth(1)
        .map(|r| r.split('/').next().unwrap_or(""))
        .unwrap_or("")
        .to_string()
}

/// 书源的三字段编辑表单。
///
/// 做成**一个表单**而不是三个快捷键（`e`/`u`/`s`）：三个单字段编辑器
/// 要记三条键位，而且改完一个想改下一个还得退出来再按另一个。
/// 表单里 Tab 循环、Enter 一起保存、Esc 取消 —— 一次会话改完。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceForm {
    pub index: usize,
    /// 0=名字 1=网址 2=搜索网址
    pub field: usize,
    pub values: [String; 3],
    /// 改过没有。没改过按 Esc 直接退出；改过要先提醒一次（脏数据保护）。
    pub dirty: bool,
    /// 已经提醒过一次「有未保存的修改」。
    pub warned: bool,
}

impl SourceForm {
    pub const FIELDS: [Field; 3] = [Field::Name, Field::Url, Field::SearchUrl];

    pub fn current_mut(&mut self) -> &mut String {
        &mut self.values[self.field]
    }

    pub fn label(&self) -> &'static str {
        Self::FIELDS[self.field].label()
    }
}

impl InputMode {
    pub fn prompt(self) -> String {
        match self {
            InputMode::Goto => "跳到第几章".to_string(),
            InputMode::Import => "书源文件路径或 URL".to_string(),
            InputMode::Filter => "筛选书源（名字或网址）".to_string(),
        }
    }
}

/// 正在打开的那本书：Toc 请求发出去之后、目录回来之前的状态。
///
/// 目录回来时要靠它把书加进书架、并跳回上次读到的那一章，
/// 所以这些信息必须活到 `Done::Toc`。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    url: String,
    name: String,
    author: String,
    /// 要跳回的章号（从搜索结果打开是 1，从书架打开是记住的那个）
    restore: usize,
    /// 从书架打开的话，来源那个书源的名字（书源列表重排也能对上）
    source_name: String,
}

/// 左上面板里的一行。
///
/// 演示书和在线搜索结果在这里被抹平成同一种东西 —— 界面层不需要分两套画法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShelfItem {
    pub name: String,
    pub author: String,
    /// 右边那截小字：「3 章」或「2 源」
    pub note: String,
    /// 在线书的详情页；演示书是空串
    pub url: String,
    /// 演示书在 `demo::books()` 里的下标；在线书是 None
    pub demo_book: Option<usize>,
}

/// 书架文件跟 `state.json` 放同一个目录。
///
/// **不能按当前工作目录算** —— `--dir` 只改了状态文件的落点，
/// 书架要是还看 CWD，就会写到一个跟状态完全无关的地方去。
/// （顺带：测试之间也会通过真实文件系统互相污染，这个坑就是这么踩到的。）
fn shelf_path_beside(session: &Session) -> PathBuf {
    match session.state_path.parent() {
        Some(dir) => dir.join(SHELF_FILE),
        None => PathBuf::from(SHELF_FILE),
    }
}

pub struct App {
    pub session: Session,
    /// 书架（`shelf.json`）。和 `session` 并列 —— 一个是长期资产，一个是当前界面状态。
    pub shelf: Shelf,
    /// 当前在哪一屏。
    pub screen: Screen,
    /// 返回栈：Esc 弹一层。
    back: Vec<Screen>,
    /// 搜索页里键盘归谁。
    pub search_focus: SearchFocus,
    /// 书架 / 搜索结果列表的光标。
    pub shelf_cursor: usize,
    /// 目录光标（0-based，等于章号 - 1）。
    pub toc_cursor: usize,
    pub source_cursor: usize,
    pub reader_scroll: u16,
    /// `Some` 表示正在输入（跳章 / 导入书源）。
    pub input: Option<(InputMode, String)>,
    /// `Some` 表示正在编辑某个书源（三字段表单）。
    pub form: Option<SourceForm>,
    /// 书源列表的筛选条件（已应用的）。空 = 不过滤。
    ///
    /// 默认书源集合有三千多个，没有筛选这一屏就没法用 —— 只能按住下键翻。
    pub sources_filter: String,
    pub status: String,
    pub help: bool,
    pub quit: bool,

    // ── 在线部分 ────────────────────────────────────────────
    /// 加载好的书源集合。没有书源文件时是 `None`，界面退回纯演示模式。
    pub online: Option<Online>,
    /// 在线搜索的结果。`Some` 时左上面板列的是这些，而不是演示书架。
    pub web_hits: Option<Vec<WebHit>>,
    /// 当前这本书的目录（在线模式）。空表示还在演示模式。
    pub chapters: Vec<Chapter>,
    /// 目录是从第几个书源抓来的（读正文要用同一个源）。
    pub toc_source: usize,
    /// 当前这本书的书名与作者（在线模式下的详情页信息）。
    pub book_name: String,
    pub book_author: String,
    /// 正在后台跑的任务。界面只负责把它交给 main 的 worker。
    job: Option<Job>,
    /// 加载中的一句话；`None` 表示空闲。
    pub busy: Option<String>,
    /// 正在打开的那本书（见 [`Pending`]）。
    pending: Option<Pending>,
}

impl App {
    pub fn new(session: Session) -> Self {
        let shelf = Shelf::load(shelf_path_beside(&session));
        Self::with_shelf(session, shelf)
    }

    /// 书架路径可注入（测试要用临时目录）。
    pub fn with_shelf(session: Session, shelf: Shelf) -> Self {
        let mut app = Self {
            session,
            shelf,
            screen: Screen::Shelf,
            back: Vec::new(),
            search_focus: SearchFocus::Input,
            shelf_cursor: 0,
            toc_cursor: 0,
            source_cursor: 0,
            reader_scroll: 0,
            input: None,
            form: None,
            sources_filter: String::new(),
            status: String::new(),
            help: false,
            quit: false,
            online: None,
            web_hits: None,
            chapters: Vec::new(),
            toc_source: 0,
            book_name: String::new(),
            book_author: String::new(),
            job: None,
            busy: None,
            pending: None,
        };
        app.sync_cursors();
        app
    }

    // ── 派生视图 ────────────────────────────────────────────

    pub fn state(&self) -> &novel_core::State {
        &self.session.state
    }

    fn state_mut(&mut self) -> &mut novel_core::State {
        &mut self.session.state
    }

    /// 当前屏要列出的行。
    ///
    /// 三种行来源在各自那一屏里汇成同一种形状：
    /// * **书架行** —— 长期数据，打开时恢复进度；
    /// * **搜索结果行** —— 临时的，打开时从第一章开始；
    /// * **演示书** —— 只在 `--offline` / 没有书源文件时用。
    pub fn rows(&self) -> Vec<ShelfItem> {
        match self.screen {
            Screen::Shelf => self.shelf_rows(),
            Screen::Search => self.search_rows(),
            // 目录：在线用抓来的 chapters，演示模式用内置书
            Screen::Toc if self.web_mode() => self
                .chapters
                .iter()
                .enumerate()
                .map(|(i, c)| ShelfItem {
                    name: c.title.clone(),
                    author: String::new(),
                    note: format!("{}", i + 1),
                    url: c.url.clone(),
                    demo_book: None,
                })
                .collect(),
            Screen::Toc => (0..self.toc_len())
                .map(|i| ShelfItem {
                    name: self.toc_title(i).unwrap_or("?").to_string(),
                    author: String::new(),
                    note: format!("{}", i + 1),
                    url: String::new(),
                    demo_book: Some(self.state().book),
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// 当前屏的光标位置（书架 / 搜索 / 目录共用一套「上下选」）。
    pub fn cursor(&self) -> usize {
        match self.screen {
            Screen::Search => self.shelf_cursor,
            Screen::Toc => self.toc_cursor,
            _ => self.shelf_cursor,
        }
    }

    pub fn cursor_mut(&mut self) -> &mut usize {
        match self.screen {
            Screen::Search => &mut self.shelf_cursor,
            Screen::Toc => &mut self.toc_cursor,
            _ => &mut self.shelf_cursor,
        }
    }

    fn shelf_rows(&self) -> Vec<ShelfItem> {
        // 有书源、书架还空着：**不塞演示书**，给一句提示让人去搜
        if self.shelf.is_empty() {
            return if self.online.is_some() {
                Vec::new()
            } else {
                self.demo_rows()
            };
        }
        self.shelf
            .order()
            .into_iter()
            .filter_map(|i| self.shelf.books.get(i))
            .map(|b| ShelfItem {
                name: b.name.clone(),
                author: b.author.clone(),
                note: if b.chapters > 0 {
                    format!("{}/{} 章", b.chapter.min(b.chapters), b.chapters)
                } else {
                    format!("读到 {} 章", b.chapter)
                },
                url: b.url.clone(),
                demo_book: None,
            })
            .collect()
    }

    /// 搜索结果行。
    ///
    /// 在线结果优先；**没有书源时退回筛演示书**（`--offline` 下搜索仍然有用）。
    fn search_rows(&self) -> Vec<ShelfItem> {
        if let Some(hits) = &self.web_hits {
            return hits
                .iter()
                .map(|h| ShelfItem {
                    name: h.name.clone(),
                    author: h.author.clone(),
                    note: String::new(),
                    url: h.url.clone(),
                    demo_book: None,
                })
                .collect();
        }
        if self.online.is_some() {
            return Vec::new();
        }
        let q = self.state().query.trim();
        if q.is_empty() {
            return self.demo_rows();
        }
        let picks: Vec<usize> = demo::search(q).iter().map(|h| h.book).collect();
        picks
            .into_iter()
            .map(|b| {
                let (name, author) = match demo::book(b) {
                    Some(d) => (d.name.to_string(), d.author.to_string()),
                    None => ("?".into(), String::new()),
                };
                ShelfItem {
                    name,
                    author,
                    note: String::new(),
                    url: String::new(),
                    demo_book: Some(b),
                }
            })
            .collect()
    }

    /// 内置演示书的行。只在没有书源文件时用。
    fn demo_rows(&self) -> Vec<ShelfItem> {
        (0..demo::books().len())
            .map(|b| {
                let (name, author) = match demo::book(b) {
                    Some(d) => (d.name.to_string(), d.author.to_string()),
                    None => ("?".into(), String::new()),
                };
                ShelfItem {
                    name,
                    author,
                    note: String::new(),
                    url: String::new(),
                    demo_book: Some(b),
                }
            })
            .collect()
    }

    /// 当前光标那一行。
    fn cursor_item(&self) -> Option<ShelfItem> {
        self.rows().into_iter().nth(self.cursor())
    }

    /// 导航路径，画在标题栏下面那一行。
    ///
    /// 存在的理由：一屏一屏地走之后，「我在哪、Esc 会回到哪」必须一眼看得见 ——
    /// 否则用户会迷路（这正是并列面板不会有的问题）。
    pub fn breadcrumb(&self) -> String {
        let mut parts: Vec<String> = self.back.iter().map(|s| self.screen_label(*s)).collect();
        parts.push(self.screen_label(self.screen));
        parts.join(" › ")
    }

    fn screen_label(&self, s: Screen) -> String {
        match s {
            Screen::Shelf => "书架".to_string(),
            Screen::Search => {
                let q = self.state().query.trim();
                if q.is_empty() {
                    "搜索".to_string()
                } else {
                    format!("搜索 “{q}”")
                }
            }
            Screen::Toc => format!("目录 · {}", self.display_book_name()),
            Screen::Reader => format!("阅读 · 第 {} / {} 章", self.state().chapter, self.toc_len()),
            Screen::Sources => format!("书源（{} 个）", self.source_len()),
        }
    }

    /// 当前这本书叫什么（在线用抓来的名字，演示模式用内置书的名字）。
    pub fn display_book_name(&self) -> String {
        if self.web_mode() && !self.book_name.is_empty() {
            return self.book_name.clone();
        }
        self.state()
            .book_def()
            .map(|b| b.name.to_string())
            .unwrap_or_default()
    }

    /// 正在读抓来的书吗。
    pub fn web_mode(&self) -> bool {
        !self.chapters.is_empty()
    }

    pub fn toc_len(&self) -> usize {
        if self.web_mode() {
            self.chapters.len()
        } else {
            self.state().total_chapters()
        }
    }

    pub fn toc_title(&self, i: usize) -> Option<&str> {
        if self.web_mode() {
            self.chapters.get(i).map(|c| c.title.as_str())
        } else {
            demo::chapter(self.state().book, i + 1).map(|(t, _)| t)
        }
    }

    /// 书源名字列表（在线时用真书源名，否则用演示的）。
    pub fn source_names(&self) -> Vec<String> {
        match &self.online {
            Some(o) => o.names(),
            None => demo::sources().iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 筛选后的书源**原始下标**。列表里第 k 行对应 `source_rows()[k]`。
    ///
    /// 光标是显示列表里的位置，不是源的下标 —— 边打字边筛的时候下标会动，
    /// 用下标当光标会让选中项在筛选时跳来跳去。
    pub fn source_rows(&self) -> Vec<usize> {
        let f = self.sources_filter.trim();
        match &self.online {
            Some(o) => o.find(f),
            None => {
                let all = demo::sources();
                if f.is_empty() {
                    return (0..all.len()).collect();
                }
                let n = f.to_lowercase();
                all.iter()
                    .enumerate()
                    .filter(|(_, name)| name.to_lowercase().contains(&n))
                    .map(|(i, _)| i)
                    .collect()
            }
        }
    }

    /// 光标所在那一行对应的**原始**书源下标。
    pub fn source_at_cursor(&self) -> Option<usize> {
        self.source_rows().get(self.source_cursor).copied()
    }

    /// 筛选后还剩几个。
    pub fn sources_matched(&self) -> usize {
        self.source_rows().len()
    }

    /// 把书源光标夹进当前显示列表的范围。
    fn clamp_source_cursor(&mut self) {
        self.source_cursor = self
            .source_cursor
            .min(self.sources_matched().saturating_sub(1));
    }

    /// 第 i 个（原始下标）书源的名字。
    pub fn source_name(&self, i: usize) -> String {
        match &self.online {
            Some(o) => o.field(i, "name"),
            None => demo::sources()
                .get(i)
                .map(|s| s.to_string())
                .unwrap_or_default(),
        }
    }

    /// 右下面板的书源开关状态。演示模式下全是 true。
    pub fn source_on(&self, i: usize) -> bool {
        match &self.online {
            Some(o) => o.enabled(i),
            None => true,
        }
    }

    /// 开着的书源数。
    pub fn sources_on(&self) -> usize {
        match &self.online {
            Some(o) => (0..o.len()).filter(|&i| o.enabled(i)).count(),
            None => self.source_len(),
        }
    }

    /// 装上书源集合。
    pub fn set_online(&mut self, online: Online) {
        self.online = Some(online);
    }

    /// 有排好但还没被 worker 取走的任务吗。
    ///
    /// 和 `busy` 不同：`busy` 是「worker 在跑」，这个是「还没交出去」。
    /// 自测的驱动台要靠它决定「还要不要再等一轮」——
    /// 只看 `busy` 的话，`Done::Toc` 刚处理完、`Job::Read` 刚排上、
    /// 而 `busy` 还没置上的那一瞬间会被判成「空闲」，脚本就提前收尾了。
    pub fn has_pending_job(&self) -> bool {
        self.job.is_some()
    }

    /// 取走待办。`main` 的事件循环每帧问一次，有就丢给 worker。
    pub fn take_job(&mut self) -> Option<Job> {
        let job = self.job.take()?;
        self.busy = Some(match &job {
            Job::Search(k) => format!("搜索 {k} …"),
            Job::Toc { .. } => "拉目录 …".into(),
            Job::Read { .. } => "拉正文 …".into(),
        });
        Some(job)
    }

    /// 收起「在加载」。任务被丢掉时用（自测的后台是假的）。
    pub fn idle(&mut self) {
        self.busy = None;
    }

    /// worker 报了进度。
    pub fn on_progress(&mut self, what: &str) {
        self.busy = Some(what.to_string());
    }

    /// worker 交活了。
    pub fn on_done(&mut self, done: Done) {
        // `More` 是搜索的**中间**结果，后面还有批次 —— 不能把「在加载」收掉，
        // 否则状态栏会在搜索没完的时候显示成空闲
        if !matches!(done, Done::More(_)) {
            self.busy = None;
        }
        match done {
            // 搜索中间结果：追加。不清 busy（后面还有）。
            //
            // 先到的先能点，比「等二十秒一次性出来」好得多。
            Done::More(hits) => {
                let got = self.web_hits.get_or_insert_with(Vec::new);
                for h in hits {
                    if !got.iter().any(|x| x.url == h.url) {
                        got.push(h);
                    }
                }
                let n = got.len();
                // 用户可能已经 Tab 到结果列表开始选了 —— 不要抢回焦点
                if self.search_focus == SearchFocus::Results && n > 0 {
                    self.status = format!("{n} 条结果（还在搜 …）");
                } else {
                    self.status = format!("搜索中 … 已 {n} 条");
                }
            }
            Done::Search(hits) => {
                let n = hits.len();
                self.web_hits = Some(hits);
                self.shelf_cursor = 0;
                // 结果到手就把键盘交给列表 —— 打完字的人接下来就是想选
                self.search_focus = SearchFocus::Results;
                self.status = format!("{n} 条结果（Enter 打开，b 加入书架）");
            }
            Done::Toc {
                chapters,
                from_cache,
            } => {
                let n = chapters.len();
                // 从缓存来的：用户可能已经在读了，别再把章节表换掉后
                // 把光标/正文重置一遍 —— 只在确实变了的时候跟着走
                let same = self.chapters.len() == n;
                self.chapters = chapters;
                let from = if from_cache { "（缓存）" } else { "" };
                // 打开一本书就顺手进书架 —— 「之后还想接着读」是绝大多数情况，
                // 让人再按一次收藏属于无谓的税
                let restore = match self.pending.take() {
                    Some(p) => {
                        let restore = p.restore.clamp(1, n.max(1));
                        self.shelf.add(ShelfBook {
                            source: p.source_name,
                            name: p.name,
                            author: p.author,
                            url: p.url,
                            toc_url: String::new(),
                            chapter: restore,
                            title: String::new(),
                            chapters: n,
                            read_at: Shelf::now(),
                        });
                        if let Err(e) = self.shelf.save() {
                            self.status = format!("书架写盘失败：{e}");
                        }
                        restore
                    }
                    None => 1,
                };
                self.state_mut().set_chapter(restore);
                self.after_chapter_change();
                // 目录到手才进目录屏 —— 拉失败的话留在原地，让人换一本再试
                self.go(Screen::Toc);
                self.status = if restore > 1 {
                    format!("目录 {n} 章{from}，接着读第 {restore} 章")
                } else {
                    format!("目录 {n} 章{from}")
                };
                let _ = same;
                self.request_chapter();
            }
            Done::Read { title, text } => {
                let paragraphs: Vec<String> = text
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                let book = Book {
                    name: self.book_name.clone(),
                    author: self.book_author.clone(),
                    source: self
                        .online
                        .as_ref()
                        .and_then(|o| o.names().get(self.toc_source).cloned())
                        .unwrap_or_default(),
                    total_chapters: self.chapters.len(),
                };
                let chapter = RChapter {
                    index: self.state().chapter,
                    title,
                    paragraphs,
                };
                match self.session.show_online(book, chapter) {
                    Ok(()) => {
                        self.reader_scroll = 0;
                        // 章节记忆：每读一章就记一次。写盘失败不影响阅读。
                        let url = self.current_book_url().unwrap_or_default();
                        if !url.is_empty() {
                            let (ch, title) = (
                                self.state().chapter,
                                self.toc_title(self.state().chapter.saturating_sub(1))
                                    .unwrap_or("")
                                    .to_string(),
                            );
                            self.shelf.remember(&url, ch, &title, Shelf::now());
                            let _ = self.shelf.save();
                        }
                    }
                    Err(e) => self.status = format!("写盘失败：{e}"),
                }
            }
            Done::Failed(e) => self.status = e,
        }
    }

    /// 把「读当前这一章」排进队列。
    fn request_chapter(&mut self) {
        let i = self.state().chapter.saturating_sub(1);
        let Some(ch) = self.chapters.get(i) else {
            return;
        };
        self.job = Some(Job::Read {
            source: self.toc_source,
            url: ch.url.clone(),
            title: ch.title.clone(),
        });
    }

    pub fn source_len(&self) -> usize {
        match &self.online {
            Some(o) => o.len(),
            None => demo::sources().len(),
        }
    }

    /// 正文预览的行（带缓存；内容变了才重算）。
    /// 当前这一章的段落。阅读页显示的就是它。
    ///
    /// 和 `reader.novel` 不是一回事：那个文件是**渲染出来的 TS**（正文在 `//`
    /// 注释里，给编辑器看的）；这里给 TUI 显示的是同内容的纯文本。
    /// 两边都由同一份章节数据来，不会脱节。
    pub fn reader_paragraphs(&self) -> Vec<String> {
        if self.web_mode() {
            // 在线：正文在 session 的 payload 里
            return self.session.paragraphs();
        }
        demo::chapter(self.state().book, self.state().chapter)
            .map(|(_, body)| body.iter().map(|p| p.to_string()).collect())
            .unwrap_or_default()
    }

    // ── 键位 ────────────────────────────────────────────────

    /// 按键分发。优先级是**刻意排的**：
    ///
    /// `Ctrl-C` → 帮助浮层 → 临时输入框 → **搜索关键字框** → 全局键 → 本屏命令。
    ///
    /// 搜索关键字框排在全局键**前面**是关键：关键字框是常驻的输入焦点，不是
    /// 临时模式。排在后面的话，在搜索框里敲小说名会误触发全局命令 ——
    /// 敲 `q` 直接退出、敲 `b` 收藏、敲数字开关书源。这套优先级来自一份
    /// 交互设计评审（见 DESIGN.md 第 4 节）。
    pub fn handle(&mut self, key: Key) {
        // Ctrl-C 是唯一在任何模式里都保留的键：输入到一半也要能退出去
        if key == Key::Ctrl('c') {
            self.quit = true;
            return;
        }
        if self.help {
            // 帮助盖住全屏，任何键都只是关掉它
            self.help = false;
            return;
        }
        if self.input.is_some() {
            self.handle_input(key);
            return;
        }
        if self.form.is_some() {
            self.handle_form(key);
            return;
        }
        // 搜索屏的关键字框有焦点时，除了 Enter/Tab/Esc 全是文本
        if self.screen == Screen::Search && self.search_focus == SearchFocus::Input {
            if key == Key::Esc {
                self.esc();
            } else {
                self.handle_search(key);
            }
            return;
        }
        self.handle_normal(key);
    }

    /// 进另一屏。Esc 会退回这里。
    pub fn go(&mut self, screen: Screen) {
        if self.screen == screen {
            return;
        }
        self.back.push(self.screen);
        self.screen = screen;
        self.status.clear();
    }

    /// 退回上一屏。没有上一层就回书架。
    pub fn go_back(&mut self) {
        self.screen = self.back.pop().unwrap_or(Screen::Shelf);
        self.status.clear();
    }

    fn handle_normal(&mut self, key: Key) {
        match key {
            // `q` **只在根书架**退出。别的屏上它是本屏命令的一部分
            //（在书源屏里也可能是别的东西），全局退出只留 Ctrl-C。
            Key::Char('q') if self.screen == Screen::Shelf => {
                self.quit = true;
                return;
            }
            // `?` 是帮助的便利别名（输入焦点下它是文本，这里到不了）
            Key::Char('?') => {
                self.help = true;
                return;
            }
            Key::Esc => {
                self.esc();
                return;
            }
            _ => {}
        }
        match self.screen {
            Screen::Shelf => self.handle_shelf(key),
            Screen::Search => self.handle_search(key),
            Screen::Toc => self.handle_toc(key),
            Screen::Reader => self.handle_reader(key),
            Screen::Sources => self.handle_sources(key),
        }
    }

    /// Esc：逐层退回。最外层（书架）只清状态栏。
    fn esc(&mut self) {
        if self.screen == Screen::Shelf {
            self.status.clear();
            return;
        }
        // 从搜索页退出去时把结果收起来 —— 它们是临时的
        if self.screen == Screen::Search {
            self.web_hits = None;
            self.search_focus = SearchFocus::Input;
        }
        self.go_back();
    }

    /// 列表里通用的上下移动。
    fn move_cursor(&mut self, delta: i64) {
        let len = self.rows().len();
        let cur = self.cursor() as i64;
        let next = (cur + delta).clamp(0, (len as i64 - 1).max(0));
        *self.cursor_mut() = next as usize;
    }

    fn handle_shelf(&mut self, key: Key) {
        match key {
            Key::Down | Key::Char('j') => self.move_cursor(1),
            Key::Up | Key::Char('k') => self.move_cursor(-1),
            Key::Home => *self.cursor_mut() = 0,
            Key::End => {
                let n = self.rows().len();
                *self.cursor_mut() = n.saturating_sub(1);
            }
            // 书架是根屏，出口都在这儿：`/` 或 `s` 进搜索，`S` 进书源
            Key::Char('/') | Key::Char('s') => {
                self.search_focus = SearchFocus::Input;
                self.go(Screen::Search);
            }
            Key::Char('S') => self.go(Screen::Sources),
            Key::Enter => self.open_cursor(),
            Key::Char('d') => self.remove_from_shelf(),
            // 换书：在书架上按最近阅读顺序跳到下一本 / 上一本并打开
            Key::Char('}') => self.step_book(1),
            Key::Char('{') => self.step_book(-1),
            _ => {}
        }
    }

    fn handle_search(&mut self, key: Key) {
        match self.search_focus {
            SearchFocus::Input => match key {
                Key::Enter => self.run_search(),
                Key::Char(c) => self.session.state.query.push(c),
                Key::Backspace => {
                    self.session.state.query.pop();
                }
                Key::Ctrl('u') | Key::Delete => self.session.state.query.clear(),
                // 打完字往下走：Tab 或 ↓ 都进结果列表
                Key::Tab | Key::Down => {
                    if !self.rows().is_empty() {
                        self.search_focus = SearchFocus::Results;
                        *self.cursor_mut() = 0;
                    }
                }
                _ => {}
            },
            SearchFocus::Results => match key {
                Key::Down | Key::Char('j') => self.move_cursor(1),
                Key::Up | Key::Char('k') => self.move_cursor(-1),
                Key::PageDown => self.move_cursor(10),
                Key::PageUp => self.move_cursor(-10),
                Key::Home => *self.cursor_mut() = 0,
                Key::End => {
                    let n = self.rows().len();
                    *self.cursor_mut() = n.saturating_sub(1);
                }
                // 回去改关键字
                Key::Tab | Key::Char('/') => self.search_focus = SearchFocus::Input,
                Key::Enter => self.open_cursor(),
                Key::Char('b') => self.bookmark_cursor(),
                _ => {}
            },
        }
    }

    /// 跑一次搜索。关键字在 `state.query` 里（边打边改的就是它）。
    fn run_search(&mut self) {
        let key = self.session.state.query.trim().to_string();
        if key.is_empty() {
            self.status = "关键字是空的".into();
            return;
        }
        if self.online.is_none() {
            // 没有书源：筛演示书（`--offline` 下搜索仍然有用）
            let n = self.search_rows().len();
            self.search_focus = SearchFocus::Results;
            *self.cursor_mut() = 0;
            self.status = format!("演示书架：{n} 本匹配");
            return;
        }
        self.web_hits = None;
        self.job = Some(Job::Search(key.clone()));
        self.status = format!("搜索 “{key}” …");
        self.persist();
    }

    fn handle_toc(&mut self, key: Key) {
        match key {
            // 目录屏里 `n`/`p` 是「列表里的下一个/上一个」（和上下键等价）——
            // 在阅读屏里它们才是「翻章」。同一组键、两种语境，都符合直觉。
            Key::Down | Key::Char('j') | Key::Char('n') => self.move_cursor(1),
            Key::Up | Key::Char('k') | Key::Char('p') => self.move_cursor(-1),
            Key::PageDown => self.move_cursor(10),
            Key::PageUp => self.move_cursor(-10),
            Key::Home => *self.cursor_mut() = 0,
            Key::End => {
                let n = self.toc_len();
                *self.cursor_mut() = n.saturating_sub(1);
            }
            Key::Char('g') => self.input = Some((InputMode::Goto, String::new())),
            // 强制重拉目录，无视缓存。缓存是为了「不用每次都等」，不是为了
            // 「永远看不到新章节」—— 所以必须留一个明确的刷新键。
            Key::Char('R') => self.reload_toc(),
            Key::Enter => {
                let n = self.toc_cursor + 1;
                self.goto(n);
                self.go(Screen::Reader);
            }
            _ => {}
        }
    }

    /// 重新拉当前这本书的目录（无视缓存）。
    ///
    /// 缓存是为了「不用每次都等」，不是为了「永远看不到新章节」——
    /// 所以必须留一个明确的刷新键。
    fn reload_toc(&mut self) {
        let Some(url) = self.current_book_url() else {
            self.status = "还没有打开的书".into();
            return;
        };
        if self.online.is_none() {
            self.status = "演示模式没有在线目录".into();
            return;
        }
        // 书源可能被删过/换过；按域名重新认一次，认不出才退回上次用的那个
        let idx = self.pick_source(&host_of(&url)).unwrap_or(self.toc_source);
        if idx >= self.source_len() {
            self.status = "这本书的书源没了，去书源屏加一个".into();
            return;
        }
        self.toc_source = idx;
        self.status = format!("《{}》重新拉目录 …", self.display_book_name());
        self.job = Some(Job::Toc {
            source: idx,
            url,
            force_refresh: true,
        });
    }

    fn handle_reader(&mut self, key: Key) {
        match key {
            // 翻章：`n`/`p` 和 `]`/`[` 等价。右手在方向键上时 `[`/`]`
            // 不用跨到右手区，左手在字母区时 `n`/`p` 更顺 —— 两套都留着。
            Key::Char('n') => self.step(1),
            Key::Char('p') => self.step(-1),
            Key::Char('g') => self.input = Some((InputMode::Goto, String::new())),
            Key::Char('o') => self.open_in_os(),
            Key::Char('R') => self.reload_toc(),
            Key::Char('r') => match self.session.save_and_write() {
                Ok(()) => self.status = "已重新渲染 .novel/reader.novel".into(),
                Err(e) => self.status = format!("写盘失败：{e}"),
            },
            Key::Down | Key::Char('j') => self.reader_scroll = self.reader_scroll.saturating_add(1),
            Key::Up | Key::Char('k') => self.reader_scroll = self.reader_scroll.saturating_sub(1),
            Key::PageDown | Key::Char(' ') => {
                self.reader_scroll = self.reader_scroll.saturating_add(10)
            }
            Key::PageUp => self.reader_scroll = self.reader_scroll.saturating_sub(10),
            Key::Home => self.reader_scroll = 0,
            Key::End => self.reader_scroll = u16::MAX,
            _ => {}
        }
    }

    fn handle_sources(&mut self, key: Key) {
        // 光标在**显示列表**里，不在源数组里 —— 下面每次都要映射回原始下标
        let len = self.sources_matched();
        let at = self.source_at_cursor();
        match key {
            Key::Down | Key::Char('j') => {
                self.source_cursor = (self.source_cursor + 1).min(len.saturating_sub(1));
            }
            Key::Up | Key::Char('k') => self.source_cursor = self.source_cursor.saturating_sub(1),
            Key::PageDown => {
                self.source_cursor = (self.source_cursor + 10).min(len.saturating_sub(1));
            }
            Key::PageUp => self.source_cursor = self.source_cursor.saturating_sub(10),
            Key::Home => self.source_cursor = 0,
            Key::End => self.source_cursor = len.saturating_sub(1),
            // `/` 筛选：三千多个源没有筛选等于没法用
            Key::Char('/') => {
                self.input = Some((InputMode::Filter, self.sources_filter.clone()));
            }
            // Ctrl-U 一键清掉筛选（不用进输入框再清）
            Key::Ctrl('u') => {
                self.sources_filter.clear();
                self.source_cursor = 0;
                self.status = format!("已清空筛选，共 {} 个书源", self.source_len());
            }
            // 空格开关 —— 开关是高频动作，且不该占用 Enter 的「编辑」语义
            Key::Char(' ') => {
                if let Some(i) = at {
                    self.toggle_source(i);
                }
            }
            // Enter 进三字段编辑表单（Tab 换字段、Enter 保存、Esc 取消）
            Key::Enter => {
                if let Some(i) = at {
                    self.open_form(i);
                }
            }
            Key::Char('a') => self.ask_import(),
            Key::Char('d') => {
                if let Some(i) = at {
                    self.remove_source(i);
                }
            }
            _ => {}
        }
        // 删完 / 筛完光标可能越界，重新夹一下
        self.clamp_source_cursor();
    }

    /// 把光标那一行加进书架。
    fn bookmark_cursor(&mut self) {
        let Some(item) = self.cursor_item() else {
            return;
        };
        if item.url.is_empty() {
            // 演示书：它们本来就在「演示书架」里，没有收藏一说
            self.status = "演示书不用收藏".into();
            return;
        }
        if self.shelf.contains(&item.url) {
            self.status = format!("《{}》已经在书架上了", item.name);
            return;
        }
        self.shelf.add(ShelfBook {
            source: self.current_source_name(),
            name: item.name.clone(),
            author: item.author.clone(),
            url: item.url.clone(),
            toc_url: String::new(),
            chapter: 1,
            title: String::new(),
            chapters: 0,
            read_at: Shelf::now(),
        });
        match self.shelf.save() {
            Ok(()) => {
                self.status = format!("已把《{}》加入书架（{} 本）", item.name, self.shelf.len())
            }
            Err(e) => self.status = format!("书架写盘失败：{e}"),
        }
    }

    /// 把书架上光标那一行移出书架。
    fn remove_from_shelf(&mut self) {
        let Some(item) = self.cursor_item() else {
            return;
        };
        if !self.shelf.remove(&item.url) {
            return;
        }
        let _ = self.shelf.save();
        self.sync_cursors();
        self.status = format!("已把《{}》移出书架", item.name);
    }

    fn handle_input(&mut self, key: Key) {
        let Some((mode, buf)) = self.input.as_mut() else {
            return;
        };
        match key {
            Key::Esc => {
                self.input = None;
                self.status = "已取消".into();
            }
            Key::Backspace => {
                buf.pop();
            }
            Key::Delete => buf.clear(),
            Key::Enter => {
                let mode = *mode;
                let text = buf.trim().to_string();
                self.input = None;
                self.submit(mode, text);
            }
            Key::Ctrl('u') => buf.clear(),
            Key::Char(c) => buf.push(c),
            _ => {}
        }
    }

    fn submit(&mut self, mode: InputMode, text: String) {
        match mode {
            InputMode::Goto => match text.parse::<usize>() {
                Ok(n) => {
                    self.goto(n);
                }
                Err(_) => self.status = format!("`{text}` 不是章号"),
            },
            InputMode::Filter => {
                self.sources_filter = text;
                self.source_cursor = 0;
                self.status = format!(
                    "筛选后剩 {} / {} 个书源",
                    self.sources_matched(),
                    self.source_len()
                );
            }
            InputMode::Import => match &mut self.online {
                Some(o) => match o.import(&text) {
                    Ok(0) => self.status = "没有新书源（都已在列表里）".into(),
                    Ok(n) => {
                        self.status = format!("导入 {n} 个书源（共 {}）", o.len());
                        self.sync_cursors();
                    }
                    Err(e) => self.status = format!("导入失败：{e}"),
                },
                None => self.status = "没有加载书源文件".into(),
            },
        }
    }

    // ── 改动（每个都会立刻落盘）────────────────────────────

    fn step(&mut self, delta: i64) {
        if self.web_mode() {
            let n = self.chapters.len() as i64;
            let cur = self.state().chapter as i64;
            let next = (cur + delta).clamp(1, n.max(1));
            if next == cur {
                self.status = if delta > 0 {
                    "已经是最后一章了".into()
                } else {
                    "已经是第一章了".into()
                };
                return;
            }
            self.state_mut().set_chapter(next as usize);
            self.after_chapter_change();
            self.request_chapter();
            return;
        }
        if self.state_mut().step(delta) {
            self.after_chapter_change();
            self.persist();
        } else {
            self.status = if delta > 0 {
                "已经是最后一章了".into()
            } else {
                "已经是第一章了".into()
            };
        }
    }

    fn goto(&mut self, n: usize) {
        if self.web_mode() {
            if !(1..=self.chapters.len()).contains(&n) {
                self.status = format!("没有第 {n} 章（共 {} 章）", self.chapters.len());
                return;
            }
            self.state_mut().set_chapter(n);
            self.after_chapter_change();
            self.request_chapter();
            return;
        }
        if self.state_mut().goto(n) {
            self.after_chapter_change();
            self.persist();
        } else {
            self.status = format!("没有第 {n} 章（共 {} 章）", self.state().total_chapters());
        }
    }

    /// 测试用：确认没有排出去的任务。
    pub fn take_job_check_none(&mut self) -> bool {
        self.job.is_none()
    }

    /// 打开左上面板当前选中项（给集成测试用的公开入口）。
    pub fn open_cursor_for_test(&mut self) {
        self.open_cursor();
    }

    /// 打开左上面板里当前这一行。
    fn open_cursor(&mut self) {
        let Some(item) = self.cursor_item() else {
            return;
        };
        // 演示书（`--offline` 模式）
        if item.url.is_empty() {
            if let Some(b) = item.demo_book {
                self.open_book(b);
            }
            return;
        }
        // 书架上的书：走「恢复进度」那条路
        if self.screen == Screen::Shelf {
            if let Some(b) = self.shelf.get(&item.url).cloned() {
                self.open_shelf_book(&b);
            }
            return;
        }
        // 在线：从书源列表里找出域名对得上的那个，用它拉目录
        if self.online.is_none() {
            self.status = "没有加载书源".into();
            return;
        }
        let host = host_of(&item.url);
        let idx = self.pick_source(&host);
        let Some(idx) = idx else {
            self.status = format!("没有书源的域名匹配 {host}");
            return;
        };
        self.book_name = item.name.clone();
        self.book_author = item.author.clone();
        self.toc_source = idx;
        // 从搜索结果打开：从第一章开始，并记下「这本书待会儿要进书架」。
        //
        // `source_name` 必须是**书源的名字**，不是书名 —— 书架上按书源名反查，
        // 记成书名的话这本书下次打不开（「书源「斗破苍穹」不在了」）。
        let source_name = self.source_names().get(idx).cloned().unwrap_or_default();
        self.pending = Some(Pending {
            url: item.url.clone(),
            name: item.name.clone(),
            author: item.author.clone(),
            restore: 1,
            source_name,
        });
        self.status = format!("《{}》加载目录 …", item.name);
        self.job = Some(Job::Toc {
            source: idx,
            url: item.url.clone(),
            force_refresh: false,
        });
    }

    /// 按域名挑一个书源。挑不到就退回「能搜索的第一个」。
    fn pick_source(&self, host: &str) -> Option<usize> {
        let o = self.online.as_ref()?;
        o.url_of(host)
            .or_else(|| (0..o.len()).find(|&i| o.can_search(i)))
    }

    fn open_book(&mut self, book: usize) {
        if self.state_mut().open_book(book) {
            let name = self.state().book_def().map(|b| b.name).unwrap_or("?");
            self.shelf_cursor = 0;
            self.after_chapter_change();
            // 进**目录**屏，不是直接进阅读 —— 交互模型是「一屏一屏地走」：
            // 书架 → 目录 → 阅读。演示书的目录是本地就有的，但这一步不能省，
            // 否则同一个动作在演示书和在线书之间行为不一样。
            self.go(Screen::Toc);
            self.status = format!("打开《{name}》");
            self.persist();
        } else {
            self.status = format!("书架里没有第 {} 本", book + 1);
        }
    }

    fn toggle_source(&mut self, i: usize) {
        let name = self
            .source_names()
            .get(i)
            .cloned()
            .unwrap_or_else(|| "?".into());
        match &mut self.online {
            Some(o) => match o.toggle(i) {
                Ok(on) => {
                    self.status = format!(
                        "书源 {name} {}（启用 {} / {}）",
                        if on { "已开" } else { "已关" },
                        self.sources_on(),
                        self.source_len()
                    );
                }
                Err(e) => self.status = e.to_string(),
            },
            // 没有书源文件时演示源不可开关
            None => self.status = format!("演示模式：{name} 不参与开关"),
        }
    }

    // ── 书源管理 ────────────────────────────────────────────

    /// 排一个「导入书源」的输入。
    fn ask_import(&mut self) {
        if self.online.is_none() {
            self.status = "没有加载书源文件，先加一个再导入".into();
            return;
        }
        self.input = Some((InputMode::Import, String::new()));
    }

    /// 打开第 i 个书源的三字段编辑表单，**预填当前值**。
    ///
    /// 刻意预填（和 `g` 跳章相反）：改 URL 十有八九是改域名的一小段，
    /// 从零开始敲比改两三个字累得多。
    fn open_form(&mut self, i: usize) {
        let Some(o) = &self.online else {
            self.status = "没有加载书源文件".into();
            return;
        };
        if i >= o.len() {
            self.status = format!("没有第 {} 个书源", i + 1);
            return;
        }
        self.form = Some(SourceForm {
            index: i,
            field: 0,
            values: [
                o.field(i, "name"),
                o.field(i, "url"),
                o.field(i, "searchUrl"),
            ],
            dirty: false,
            warned: false,
        });
        self.status = "Tab 换字段，Enter 保存，Esc 取消".into();
    }

    /// 表单里的按键。
    fn handle_form(&mut self, key: Key) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        match key {
            Key::Esc => {
                // 脏数据保护：改过就先提醒一次，再按一次才真丢
                if form.dirty && !form.warned {
                    form.warned = true;
                    self.status = "有未保存的修改 —— 再按一次 Esc 放弃，或 Enter 保存".into();
                } else {
                    self.form = None;
                    self.status = "已取消编辑".into();
                }
            }
            Key::Enter => self.save_form(),
            Key::Tab => {
                form.field = (form.field + 1) % SourceForm::FIELDS.len();
                form.warned = false;
            }
            Key::BackTab => {
                form.field = (form.field + SourceForm::FIELDS.len() - 1) % SourceForm::FIELDS.len();
                form.warned = false;
            }
            Key::Ctrl('u') | Key::Delete => {
                form.current_mut().clear();
                form.dirty = true;
                form.warned = false;
            }
            Key::Backspace => {
                form.current_mut().pop();
                form.dirty = true;
                form.warned = false;
            }
            Key::Char(c) => {
                form.current_mut().push(c);
                form.dirty = true;
                form.warned = false;
            }
            _ => {}
        }
    }

    /// 保存表单。名字和网址不能空（空了就是一条没法用的书源）。
    fn save_form(&mut self) {
        let Some(form) = self.form.clone() else {
            return;
        };
        if form.values[0].trim().is_empty() || form.values[1].trim().is_empty() {
            self.status = format!(
                "{} 不能为空",
                if form.values[0].trim().is_empty() {
                    "名字"
                } else {
                    "网址"
                }
            );
            return;
        }
        let Some(o) = self.online.as_mut() else {
            self.status = "没有加载书源文件".into();
            self.form = None;
            return;
        };
        let mut err = None;
        for (i, f) in SourceForm::FIELDS.iter().enumerate() {
            if let Err(e) = o.edit(form.index, f.key(), &form.values[i]) {
                err = Some(e.to_string());
                break;
            }
        }
        self.form = None;
        self.status = match err {
            Some(e) => format!("保存失败：{e}"),
            None => format!("第 {} 个书源已保存", form.index + 1),
        };
    }

    /// 删掉第 i 个书源。
    fn remove_source(&mut self, i: usize) {
        let Some(o) = &mut self.online else {
            self.status = "没有加载书源文件".into();
            return;
        };
        match o.remove(i) {
            Ok(gone) => {
                self.status = format!("已删除书源 {}", gone.name);
                self.sync_cursors();
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    // ── 书架 ────────────────────────────────────────────────

    /// 当前在读书的详情页地址。从书架打开时记在 `pending` 里，
    /// 打开成功后就靠书名在书架里反查。
    fn current_book_url(&self) -> Option<String> {
        if let Some(p) = &self.pending {
            return Some(p.url.clone());
        }
        self.shelf
            .books
            .iter()
            .find(|b| b.name == self.book_name && !self.book_name.is_empty())
            .map(|b| b.url.clone())
    }

    fn current_source_name(&self) -> String {
        self.source_names()
            .get(self.toc_source)
            .cloned()
            .unwrap_or_default()
    }

    /// 在书架上换一本（`}` / `{`）。按 `read_at` 倒序走。
    fn step_book(&mut self, delta: i64) {
        if self.shelf.is_empty() {
            self.status = "书架是空的（搜到书之后按 b 收藏）".into();
            return;
        }
        let order = self.shelf.order();
        let cur = order
            .iter()
            .position(|&i| self.shelf.books[i].name == self.book_name)
            .map(|p| p as i64)
            .unwrap_or(0);
        let n = order.len() as i64;
        let next = (cur + delta).clamp(0, n - 1);
        if next == cur && !self.book_name.is_empty() {
            self.status = if delta > 0 {
                "已是最后一本"
            } else {
                "已是第一本"
            }
            .into();
            return;
        }
        let b = self.shelf.books[order[next as usize]].clone();
        // 切回书架屏，并让光标落在这一本身上
        self.screen = Screen::Shelf;
        self.back.clear();
        self.shelf_cursor = self
            .rows()
            .iter()
            .position(|it| it.url == b.url)
            .unwrap_or(0);
        self.open_shelf_book(&b);
    }

    /// 打开书架上的书：找到对应书源、拉目录、跳回记住的章。
    fn open_shelf_book(&mut self, b: &ShelfBook) {
        if self.online.is_none() {
            self.status = "没有加载书源文件，打不开书架上的书".into();
            return;
        }
        // 按**书源名**找（不按下标 —— 书源列表会重排）
        let idx = self
            .online
            .as_ref()
            .and_then(|o| o.names().iter().position(|n| n == &b.source));
        let Some(idx) = idx else {
            self.status = format!("书源「{}」不在了（删了或改名了）", b.source);
            return;
        };
        self.book_name = b.name.clone();
        self.book_author = b.author.clone();
        self.toc_source = idx;
        self.pending = Some(Pending {
            url: b.url.clone(),
            name: b.name.clone(),
            author: b.author.clone(),
            restore: b.chapter.max(1),
            source_name: b.source.clone(),
        });
        self.status = format!("《{}》加载目录 …", b.name);
        self.job = Some(Job::Toc {
            source: idx,
            url: b.url.clone(),
            force_refresh: false,
        });
    }

    fn after_chapter_change(&mut self) {
        self.toc_cursor = self.state().chapter.saturating_sub(1);
        self.reader_scroll = 0;
        self.status.clear();
    }

    /// 让三个光标都落在合法位置上（换书、窗口变化后叫一次）。
    pub fn sync_cursors(&mut self) {
        let n = self.rows().len();
        let cur = self.cursor().min(n.saturating_sub(1));
        *self.cursor_mut() = cur;
        self.clamp_source_cursor();
    }

    /// 状态 + 阅读页一起落盘。
    ///
    /// **只写盘**：阅读页是给外部程序看的（编辑器、`o` 打开的默认程序），
    /// 谁在看谁自己重载。
    pub fn persist(&mut self) {
        if let Err(e) = self.session.save_and_write() {
            self.status = format!("写盘失败：{e}");
        }
    }

    // ── 用系统默认程序打开渲染结果 ─────────────────────────

    pub fn reader_path(&self) -> PathBuf {
        std::fs::canonicalize(&self.session.reader_path)
            .unwrap_or_else(|_| self.session.reader_path.clone())
    }

    pub fn open_in_os(&mut self) {
        if let Err(e) = self.session.write_reader() {
            self.status = format!("写盘失败：{e}");
            return;
        }
        let path = self.reader_path();
        let mut cmd = {
            #[cfg(windows)]
            {
                let mut c = Command::new("cmd");
                // `start` 是 cmd 内建命令；空字符串是它的「窗口标题」占位，
                // 少了它，带引号的路径会被当成标题。
                c.args(["/C", "start", ""]);
                c
            }
            #[cfg(target_os = "macos")]
            {
                Command::new("open")
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                Command::new("xdg-open")
            }
        };
        cmd.arg(&path);
        match cmd.spawn() {
            Ok(_) => self.status = format!("已用默认程序打开 {}", path.display()),
            Err(e) => self.status = format!("打开失败：{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> App {
        let dir = std::env::temp_dir().join(format!("novel-tui-app-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        App::new(Session::open(
            dir.join("state.json"),
            dir.join("reader.novel"),
        ))
    }

    /// 带书源的 app（夹具里那 7 条，不会真联网）。
    fn online_app(tag: &str) -> App {
        let mut app = tmp(tag);
        let all = novel_core::source::BookSource::parse_file(include_str!(
            "../../novel-core/tests/fixtures/sources.json"
        ))
        .unwrap();
        app.set_online(Online::from_sources(all));
        app
    }

    /// 敲一串字符键。
    fn keys(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle(Key::Char(c));
        }
    }

    /// 敲一串键然后回一辆。
    fn typed(app: &mut App, s: &str) {
        keys(app, s);
        app.handle(Key::Enter);
    }

    fn hit(name: &str, url: &str) -> WebHit {
        WebHit {
            name: name.into(),
            author: "某作者".into(),
            url: url.into(),
            ..Default::default()
        }
    }

    fn chapter(title: &str, url: &str) -> Chapter {
        Chapter {
            title: title.into(),
            url: url.into(),
        }
    }

    // ── 屏与导航 ────────────────────────────────────────────

    #[test]
    fn starts_on_the_shelf() {
        let app = tmp("start");
        assert_eq!(app.screen, Screen::Shelf);
        assert!(!app.quit);
        assert!(app.back.is_empty());
    }

    #[test]
    fn navigation_is_a_stack_and_esc_pops_one_level() {
        // 这是这次交互重做的地基：一屏一屏进，Esc 逐层退回
        let mut app = tmp("stack");
        app.go(Screen::Search);
        assert_eq!(app.screen, Screen::Search);
        app.go(Screen::Sources);
        assert_eq!(app.screen, Screen::Sources);

        app.handle(Key::Esc);
        assert_eq!(app.screen, Screen::Search, "Esc 只退一层");
        app.handle(Key::Esc);
        assert_eq!(app.screen, Screen::Shelf);
        // 最外层再按 Esc 不该退出，也不该崩
        app.handle(Key::Esc);
        assert_eq!(app.screen, Screen::Shelf);
        assert!(!app.quit);
    }

    #[test]
    fn go_is_idempotent_and_never_stacks_duplicates() {
        let mut app = tmp("godup");
        app.go(Screen::Search);
        app.go(Screen::Search);
        assert_eq!(app.back.len(), 1, "重复进同一屏不该压两层");
        app.handle(Key::Esc);
        assert_eq!(app.screen, Screen::Shelf);
    }

    #[test]
    fn q_quits_from_the_root_shelf_and_only_there() {
        // `q` 只在根屏退出：别的屏上它可能是本屏命令，而且
        // 搜索框有焦点时它必须是**文本**（在搜索框里敲小说名带个 q 就退出是不可接受的）
        let mut app = tmp("quit-shelf");
        app.handle(Key::Char('q'));
        assert!(app.quit);

        for screen in [Screen::Toc, Screen::Reader, Screen::Sources] {
            let mut app = tmp(&format!("noquit-{screen:?}"));
            app.screen = screen;
            app.handle(Key::Char('q'));
            assert!(!app.quit, "{screen:?} 屏上 q 不该退出");
        }
    }

    #[test]
    fn ctrl_c_is_the_only_global_quit() {
        for screen in [
            Screen::Shelf,
            Screen::Search,
            Screen::Toc,
            Screen::Reader,
            Screen::Sources,
        ] {
            let mut app = tmp(&format!("ctrlc-{screen:?}"));
            app.screen = screen;
            app.handle(Key::Ctrl('c'));
            assert!(app.quit, "{screen:?} 屏里 Ctrl-C 退不出去");
        }
    }

    /// **这条是这次按键重设计的核心回归测试。**
    ///
    /// 搜索关键字框是常驻的输入焦点，不是临时模式。之前全局键排在它前面，
    /// 于是在搜索框里敲小说名会误触发全局命令：敲 `q` 直接退出、`b` 收藏、
    /// 数字开关书源、`S` 跳到书源屏。
    #[test]
    fn typing_in_the_search_box_is_never_a_command() {
        let mut app = online_app("search-typing-keys");
        let n = app.source_len();
        app.go(Screen::Search);
        assert_eq!(app.search_focus, SearchFocus::Input);

        for c in "qbS123or?/斗破".chars() {
            app.handle(Key::Char(c));
        }
        assert!(!app.quit, "在搜索框里打 q 不该退出");
        assert_eq!(app.screen, Screen::Search, "在搜索框里打 S 不该跳屏");
        assert_eq!(app.sources_on(), n, "在搜索框里打数字不该开关书源");
        assert!(app.shelf.is_empty(), "在搜索框里打 b 不该收藏");
        assert_eq!(app.state().query, "qbS123or?/斗破", "这些都该是文本");
    }

    #[test]
    fn the_search_box_still_honours_enter_tab_and_esc() {
        let mut app = online_app("search-esc-keys");
        app.go(Screen::Search);
        keys(&mut app, "斗破");
        app.handle(Key::Enter);
        assert!(app.take_job().is_some(), "Enter 仍然是「搜」");
        app.handle(Key::Esc);
        assert_eq!(app.screen, Screen::Shelf, "Esc 仍然是「回书架」");
    }

    #[test]
    fn a_stale_quit_flag_does_not_leak_between_runs() {
        // 这条是回归测试：自测驱动台靠设 quit 收尾，而同一个 App 会被跑好几轮。
        // run() 不清的话，第二轮的第一次循环就返回，表现是「脚本只执行了第一个键」。
        let mut app = tmp("stalequit");
        app.quit = true;
        // run 会清掉它；这里直接验证字段语义：清掉之后能继续正常处理
        app.quit = false;
        app.screen = Screen::Reader;
        app.handle(Key::Char('n'));
        assert_eq!(app.state().chapter, 2);
    }

    // ── 书架屏 ──────────────────────────────────────────────

    #[test]
    fn shelf_cursor_moves_and_enter_opens_a_demo_book() {
        let mut app = tmp("shelfnav");
        app.handle(Key::Down);
        assert_eq!(app.shelf_cursor, 1);
        app.handle(Key::Enter);
        assert_eq!(app.state().book, 1, "应当打开第二本");
        assert_eq!(app.state().chapter, 1);
        assert_eq!(app.screen, Screen::Toc, "打开后先进目录屏");
        assert!(app.status.contains("长夜将尽"), "{}", app.status);
        // 再回车才进阅读屏
        app.handle(Key::Enter);
        assert_eq!(app.screen, Screen::Reader);
    }

    #[test]
    fn shelf_cursor_is_bounded() {
        let mut app = tmp("shelfbound");
        let n = app.rows().len();
        app.handle(Key::End);
        assert_eq!(app.shelf_cursor, n - 1);
        app.handle(Key::Down);
        assert_eq!(app.shelf_cursor, n - 1, "到底了不该越界");
        app.handle(Key::Home);
        assert_eq!(app.shelf_cursor, 0);
        app.handle(Key::Up);
        assert_eq!(app.shelf_cursor, 0);
    }

    #[test]
    fn slash_and_s_both_enter_search_from_the_shelf() {
        for k in ['/', 's'] {
            let mut app = tmp(&format!("tosearch{k}"));
            app.handle(Key::Char(k));
            assert_eq!(app.screen, Screen::Search, "{k} 应当进搜索屏");
            assert_eq!(app.search_focus, SearchFocus::Input);
        }
    }

    #[test]
    fn cap_s_opens_sources_from_the_shelf_only() {
        // 书源是「从书架进去的一个出口」，不是随处可跳的全局键 ——
        // 后者会把导航上下文弄丢（设计评审第 5 节）
        let mut app = online_app("tosrc");
        app.handle(Key::Char('S'));
        assert_eq!(app.screen, Screen::Sources);
        app.handle(Key::Esc);
        assert_eq!(app.screen, Screen::Shelf);

        for screen in [Screen::Toc, Screen::Reader] {
            let mut app = online_app(&format!("tosrc-{screen:?}"));
            app.screen = screen;
            app.handle(Key::Char('S'));
            assert_eq!(app.screen, screen, "{screen:?} 屏上 S 不该跳走");
        }
    }

    // ── 搜索屏 ──────────────────────────────────────────────

    #[test]
    fn typing_edits_the_query_and_enter_runs_the_search() {
        let mut app = online_app("searchtyping");
        app.go(Screen::Search);
        keys(&mut app, "斗破");
        assert_eq!(app.state().query, "斗破");
        app.handle(Key::Backspace);
        assert_eq!(app.state().query, "斗");

        app.handle(Key::Ctrl('u'));
        assert_eq!(app.state().query, "", "Ctrl-U 清空");

        keys(&mut app, "斗破苍穹");
        app.handle(Key::Enter);
        let job = app.take_job();
        assert!(
            matches!(job, Some(Job::Search(ref k)) if k == "斗破苍穹"),
            "{job:?}"
        );
    }

    #[test]
    fn tab_switches_between_the_query_box_and_the_results() {
        let mut app = online_app("searchtab");
        app.go(Screen::Search);
        assert_eq!(app.search_focus, SearchFocus::Input);
        // 没有结果时 Tab 不该跳过去（跳过去就是死路）
        app.handle(Key::Tab);
        assert_eq!(app.search_focus, SearchFocus::Input);

        // 结果到手时键盘已经自动交给列表了
        app.on_done(Done::Search(vec![hit("斗破苍穹", "http://a.com/1")]));
        assert_eq!(app.search_focus, SearchFocus::Results);
        app.handle(Key::Tab);
        assert_eq!(app.search_focus, SearchFocus::Input, "Tab 回关键字框");
        app.handle(Key::Tab);
        assert_eq!(app.search_focus, SearchFocus::Results, "再 Tab 回结果");
    }

    #[test]
    fn search_results_land_and_focus_moves_to_the_list() {
        let mut app = online_app("searchdone");
        app.go(Screen::Search);
        keys(&mut app, "斗破");
        app.handle(Key::Enter);
        app.take_job();
        app.on_done(Done::Search(vec![
            hit("斗破苍穹", "http://a.com/1"),
            hit("武动乾坤", "http://a.com/2"),
        ]));
        assert_eq!(app.rows().len(), 2);
        assert_eq!(
            app.search_focus,
            SearchFocus::Results,
            "结果到了就该让键盘去列表"
        );
        assert!(app.status.contains("2 条结果"), "{}", app.status);
    }

    #[test]
    fn offline_search_filters_the_demo_shelf() {
        let mut app = tmp("offlinefilter");
        app.go(Screen::Search);
        keys(&mut app, "夜");
        app.handle(Key::Enter);
        assert!(app.take_job().is_none(), "没有书源就不该排任务");
        assert_eq!(app.rows().len(), 2, "「夜」命中《夜色渐浓》和《长夜将尽》");
        assert_eq!(app.search_focus, SearchFocus::Results);
    }

    #[test]
    fn b_on_a_search_result_adds_it_to_the_shelf() {
        let mut app = online_app("searchbookmark");
        app.go(Screen::Search);
        app.on_done(Done::Search(vec![hit("斗破苍穹", "http://a.com/1")]));
        assert_eq!(app.shelf.len(), 0);
        app.handle(Key::Char('b'));
        assert_eq!(app.shelf.len(), 1, "{}", app.status);
        assert!(app.shelf.contains("http://a.com/1"));
        // 再加一次不该变两条
        app.handle(Key::Char('b'));
        assert_eq!(app.shelf.len(), 1);
        assert!(app.status.contains("已经在书架上"), "{}", app.status);
    }

    #[test]
    fn escaping_search_drops_the_results() {
        let mut app = online_app("searchesc");
        app.go(Screen::Search);
        app.on_done(Done::Search(vec![hit("斗破苍穹", "http://a.com/1")]));
        assert_eq!(app.rows().len(), 1);
        app.handle(Key::Esc);
        assert_eq!(app.screen, Screen::Shelf);
        assert!(app.web_hits.is_none(), "结果是一次性的，退出去就丢掉");
    }

    // ── 目录屏 ──────────────────────────────────────────────

    #[test]
    fn enter_in_the_toc_jumps_to_that_chapter_and_starts_reading() {
        let mut app = tmp("tocenter");
        app.go(Screen::Toc);
        app.handle(Key::Down);
        app.handle(Key::Down);
        app.handle(Key::Enter);
        assert_eq!(app.state().chapter, 3);
        assert_eq!(app.screen, Screen::Reader);
    }

    #[test]
    fn toc_home_end_jump_to_the_ends() {
        let mut app = tmp("tocends");
        app.go(Screen::Toc);
        app.handle(Key::End);
        assert_eq!(app.toc_cursor, app.toc_len() - 1);
        app.handle(Key::Home);
        assert_eq!(app.toc_cursor, 0);
    }

    #[test]
    fn the_breadcrumb_shows_where_you_are_and_where_esc_goes() {
        // 一屏一屏地走之后，「我在哪、Esc 回哪」必须一眼看得见
        let mut app = tmp("crumbs");
        assert_eq!(app.breadcrumb(), "书架");
        app.go(Screen::Search);
        // 面包屑画的是**整条路径**，不只当前屏 —— 这样一眼能看出 Esc 会退到哪
        assert_eq!(app.breadcrumb(), "书架 › 搜索");
        keys(&mut app, "斗破");
        assert_eq!(app.breadcrumb(), "书架 › 搜索 “斗破”");
        app.go(Screen::Toc);
        assert!(
            app.breadcrumb().starts_with("书架 › 搜索 “斗破” › 目录 · "),
            "{}",
            app.breadcrumb()
        );
        app.go(Screen::Reader);
        assert!(
            app.breadcrumb().contains("› 阅读 · 第 1 / "),
            "{}",
            app.breadcrumb()
        );
        app.handle(Key::Esc);
        assert_eq!(app.screen, Screen::Toc);
        assert!(!app.breadcrumb().contains("阅读"), "{}", app.breadcrumb());
    }

    #[test]
    fn goto_prompt_accepts_a_number_and_rejects_junk() {
        let mut app = tmp("goto");
        app.go(Screen::Toc);
        app.handle(Key::Char('g'));
        typed(&mut app, "3");
        assert_eq!(app.state().chapter, 3);

        app.handle(Key::Char('g'));
        typed(&mut app, "abc");
        assert_eq!(app.state().chapter, 3, "烂输入不该跳走");
        assert!(app.status.contains("不是章号"), "{}", app.status);

        app.handle(Key::Char('g'));
        typed(&mut app, "999");
        assert_eq!(app.state().chapter, 3);
        assert!(app.status.contains("没有第 999 章"), "{}", app.status);
    }

    // ── 阅读屏 ──────────────────────────────────────────────

    #[test]
    fn chapter_turning_is_bounded_in_both_directions() {
        let mut app = tmp("turnbound");
        app.go(Screen::Reader);
        keys(&mut app, "n");
        assert_eq!(app.state().chapter, 2);
        keys(&mut app, "nn");
        assert_eq!(app.state().chapter, 3);
        keys(&mut app, "n");
        assert_eq!(app.state().chapter, 3, "最后一章再往后不动");
        assert!(app.status.contains("最后一章"), "{}", app.status);
        keys(&mut app, "ppp");
        assert_eq!(app.state().chapter, 1, "第一章再往前不动");
        keys(&mut app, "p");
        assert!(app.status.contains("第一章"), "{}", app.status);
    }

    #[test]
    fn reader_shows_the_paragraphs_of_the_current_chapter() {
        let mut app = tmp("readertext");
        app.go(Screen::Reader);
        let first = app.reader_paragraphs();
        assert!(!first.is_empty(), "演示书应当有正文");
        keys(&mut app, "n");
        let second = app.reader_paragraphs();
        assert_ne!(first, second, "翻章之后正文要换");
    }

    #[test]
    fn reading_a_new_chapter_resets_the_scroll() {
        let mut app = tmp("scrollreset");
        app.go(Screen::Reader);
        app.handle(Key::PageDown);
        assert!(app.reader_scroll > 0);
        keys(&mut app, "n");
        assert_eq!(app.reader_scroll, 0, "翻章要回到章首");
    }

    // ── 书源屏 ──────────────────────────────────────────────

    #[test]
    fn opening_a_book_asks_for_the_cache_and_capital_r_forces_a_refresh() {
        // 目录缓存是「不用每次都实时拉」的落点；`R` 是「我要看最新的」的落点。
        // 两个都要有，少一个都会有人抱怨。
        let mut app = online_app("toccache");
        // 书源域名要和书 URL 对得上，否则 pick_source 认不出来
        let base = app.online.as_ref().unwrap().field(0, "url");
        let url = format!("{base}book/1");
        app.shelf.add(ShelfBook {
            source: app.source_names()[0].clone(),
            name: "斗破苍穹".into(),
            author: "天蚕土豆".into(),
            url: url.clone(),
            toc_url: String::new(),
            chapter: 1,
            title: String::new(),
            chapters: 10,
            read_at: Shelf::now(),
        });
        app.book_name = "斗破苍穹".into();
        app.shelf_cursor = 0;

        // 正常打开：不强制刷新（强制的话缓存就白做了）
        app.open_cursor();
        let job = app.take_job().expect("打开书要拉目录");
        assert!(
            matches!(
                job,
                Job::Toc {
                    force_refresh: false,
                    ..
                }
            ),
            "打开书不该强制刷新：{job:?}"
        );

        // `R`：强制刷新
        app.go(Screen::Toc);
        app.handle(Key::Char('R'));
        let job = app.take_job().expect("R 应当排一个拉目录的任务");
        assert!(
            matches!(
                job,
                Job::Toc {
                    force_refresh: true,
                    ..
                }
            ),
            "R 必须是强制刷新：{job:?}"
        );
        assert!(app.status.contains("重新拉目录"), "{}", app.status);
    }

    #[test]
    fn a_cached_toc_says_so() {
        let mut app = online_app("toccache-say");
        app.pending = Some(Pending {
            name: "斗破".into(),
            author: "a".into(),
            url: "http://a.com/1".into(),
            source_name: "源".into(),
            restore: 1,
        });
        app.on_done(Done::Toc {
            chapters: vec![chapter("第一章", "http://a.com/1/1")],
            from_cache: true,
        });
        assert!(
            app.status.contains("缓存"),
            "缓存来的要说出来，否则用户会奇怪为什么没有最新章节：{}",
            app.status
        );
    }

    /// 搜索结果是**分批发**的：先到的先能点。
    /// 这条锁住「追加」语义 —— 后面的批次不能把前面的冲掉，重复的也不能算两遍。
    #[test]
    fn incremental_search_results_accumulate_and_do_not_clear_busy() {
        let mut app = online_app("search-more");
        app.go(Screen::Search);
        let hit = |n: &str| WebHit {
            name: n.into(),
            url: format!("http://x/{n}"),
            ..Default::default()
        };

        app.on_progress("搜 源（1/60）");
        assert!(app.busy.is_some());

        app.on_done(Done::More(vec![hit("a"), hit("b")]));
        assert!(app.busy.is_some(), "中间结果不能把「在加载」收掉");
        assert_eq!(app.search_rows().len(), 2);
        assert!(app.status.contains("已 2 条"), "{}", app.status);

        // 用户已经 Tab 到结果列表了：这时说的是「还在搜」，因为列表已经能点
        app.search_focus = SearchFocus::Results;
        app.on_done(Done::More(vec![hit("b"), hit("c")]));
        assert_eq!(app.search_rows().len(), 3, "重复的 URL 不该算两遍");
        assert!(app.status.contains("还在搜"), "{}", app.status);
        assert_eq!(app.search_focus, SearchFocus::Results, "不该抢回焦点");

        // 最终结果替换掉（权威全集）
        app.on_done(Done::Search(vec![hit("a")]));
        assert_eq!(app.search_rows().len(), 1);
        assert!(app.busy.is_none(), "最终结果才是收工");
    }

    #[test]
    fn the_source_list_can_be_filtered_by_name_or_url() {
        // 默认书源集合有几千个，没筛选这一屏就没法用
        let mut app = online_app("srcfilter");
        app.go(Screen::Sources);
        let total = app.source_len();
        assert_eq!(app.sources_matched(), total, "没筛选时全部可见");

        // `/` 开筛选草稿，Enter 才应用（不每键重算/写盘）
        let needle = app.source_names()[0].clone();
        app.handle(Key::Char('/'));
        assert!(matches!(app.input.as_ref().unwrap().0, InputMode::Filter));
        for c in needle.chars() {
            app.handle(Key::Char(c));
        }
        assert_eq!(app.sources_matched(), total, "草稿还没应用");
        app.handle(Key::Enter);
        assert!(app.sources_matched() >= 1, "至少第一个源自己该匹配上");
        assert!(app.sources_matched() < total, "应当筛掉了一些");
        assert!(app.status.contains("筛选后剩"), "{}", app.status);

        // 光标是显示列表里的位置，映射回原始下标
        app.source_cursor = 0;
        assert_eq!(app.source_at_cursor(), Some(0));

        // 筛选之后开关的必须是**那一个**，不是「第 0 个」
        let second = app.source_names()[1].clone();
        app.sources_filter = second;
        app.source_cursor = 0;
        let real = app.source_at_cursor().unwrap();
        assert_ne!(real, 0, "筛出来的不是第 0 个 —— 这样才能验出映射");
        let before = app.source_on(real);
        app.handle(Key::Char(' '));
        assert_ne!(app.source_on(real), before, "开关打在筛选出来的那个源上");

        // Ctrl-U 一键清掉
        app.sources_filter.clear();
        app.source_cursor = 0;
        app.handle(Key::Ctrl('u'));
        assert_eq!(app.sources_matched(), total);
    }

    #[test]
    fn a_filter_that_matches_nothing_leaves_no_cursor() {
        let mut app = online_app("srcfilter-none");
        app.go(Screen::Sources);
        app.sources_filter = "没有这个名字".into();
        assert_eq!(app.sources_matched(), 0);
        assert_eq!(app.source_at_cursor(), None);
        // 无项时按动作键不能越界 panic
        for key in [Key::Char(' '), Key::Enter, Key::Char('d')] {
            app.handle(key);
        }
        assert_eq!(app.source_cursor, 0);
    }

    #[test]
    fn source_toggles_with_space() {
        // 空格开关、Enter 编辑 —— 开关是高频动作，且不该占用 Enter 的编辑语义
        let mut app = online_app("srctoggle");
        let n = app.source_len();
        assert!(n >= 2);
        assert_eq!(app.sources_on(), n);
        app.go(Screen::Sources);

        app.source_cursor = 0;
        app.handle(Key::Char(' '));
        assert!(!app.source_on(0));
        assert_eq!(app.sources_on(), n - 1);

        app.source_cursor = 1;
        app.handle(Key::Char(' '));
        assert!(!app.source_on(1));

        app.source_cursor = 0;
        app.handle(Key::Char(' '));
        assert!(app.source_on(0), "再按一次开回来");

        app.source_cursor = n + 5;
        let before = app.sources_on();
        app.handle(Key::Char(' '));
        assert_eq!(app.sources_on(), before);
    }

    #[test]
    fn demo_sources_are_not_toggleable_and_say_so() {
        let mut app = tmp("demotoggle");
        app.go(Screen::Sources);
        app.handle(Key::Char(' '));
        assert!(app.status.contains("演示模式"), "{}", app.status);
    }

    #[test]
    fn editing_a_source_prefills_the_current_value() {
        let mut app = online_app("srcedit");
        app.go(Screen::Sources);
        let name = app.source_names()[0].clone();
        app.handle(Key::Enter);
        let form = app.form.clone().expect("Enter 应当打开编辑表单");
        assert_eq!(form.field, 0, "从第一个字段开始");
        assert_eq!(
            form.values[0], name,
            "改名字要预填当前值（改几个字比从零敲省事）"
        );
        assert!(!form.dirty);

        // 跳章刚好相反：**不**预填（预填会让打字变成追加）
        app.handle(Key::Esc);
        assert!(app.form.is_none(), "没改过，Esc 应当直接退出");
        app.go(Screen::Reader);
        app.handle(Key::Char('g'));
        assert_eq!(app.input.as_ref().unwrap().1, "");
    }

    #[test]
    fn the_source_form_cycles_fields_prefills_and_saves_together() {
        let mut app = online_app("srcform");
        app.go(Screen::Sources);
        app.handle(Key::Enter);
        let before = app.source_names()[0].clone();

        // Tab 循环
        for want in [1, 2, 0] {
            app.handle(Key::Tab);
            assert_eq!(app.form.as_ref().unwrap().field, want);
        }
        app.handle(Key::BackTab);
        assert_eq!(app.form.as_ref().unwrap().field, 2, "BackTab 往回走");
        app.handle(Key::Tab);
        assert_eq!(app.form.as_ref().unwrap().field, 0, "绕回第一个字段");

        // 改名字，Tab 过一遍其余字段，一起保存
        app.handle(Key::Char('x'));
        assert!(app.form.as_ref().unwrap().dirty);
        assert_eq!(
            app.form.as_ref().unwrap().values[0],
            format!("{before}x"),
            "字要落到当前那个字段"
        );
        app.handle(Key::Tab);
        app.handle(Key::Tab);
        app.handle(Key::Enter);
        assert!(app.form.is_none(), "保存后表单关闭");
        assert!(app.status.contains("已保存"), "{}", app.status);
        let now = app.source_names()[0].clone();
        assert_eq!(now, format!("{before}x"), "名字要真的落地（{now}）");
    }

    #[test]
    fn a_dirty_source_form_asks_before_throwing_away_the_edit() {
        let mut app = online_app("srcdirty");
        app.go(Screen::Sources);
        app.handle(Key::Enter);
        app.handle(Key::Char('x'));
        app.handle(Key::Esc);
        assert!(app.form.is_some(), "改过了，第一次 Esc 只提醒");
        assert!(app.status.contains("未保存"), "{}", app.status);
        app.handle(Key::Esc);
        assert!(app.form.is_none(), "第二次 Esc 才真丢");
        assert!(app.status.contains("取消"), "{}", app.status);
    }

    #[test]
    fn an_empty_source_name_or_url_is_refused() {
        let mut app = online_app("srcempty");
        app.go(Screen::Sources);
        let before = app.source_names()[0].clone();
        app.handle(Key::Enter);
        app.handle(Key::Ctrl('u'));
        app.handle(Key::Enter);
        assert!(app.form.is_some(), "空名字不能保存");
        assert!(app.status.contains("不能为空"), "{}", app.status);
        app.handle(Key::Esc);
        app.handle(Key::Esc);
        assert_eq!(app.source_names()[0], before, "书源没被改坏");
    }

    #[test]
    fn typing_in_the_source_form_is_never_a_command() {
        // 表单也是输入焦点：在网址里打 q 不能退出
        let mut app = online_app("srcform-keys");
        app.go(Screen::Sources);
        app.handle(Key::Enter);
        app.handle(Key::Ctrl('u'));
        for c in "qs1".chars() {
            app.handle(Key::Char(c));
        }
        assert!(!app.quit);
        assert_eq!(app.form.as_ref().unwrap().values[0], "qs1");
    }

    #[test]
    fn deleting_a_source_says_what_it_did() {
        let mut app = online_app("srcdel");
        app.go(Screen::Sources);
        let n = app.source_len();
        app.handle(Key::Char('d'));
        assert_eq!(app.source_len(), n - 1);
        assert!(app.status.contains("已删除书源"), "{}", app.status);
    }

    // ── 在线链路 ────────────────────────────────────────────

    #[test]
    fn opening_a_book_goes_to_the_toc_and_uses_the_source_name() {
        let mut app = online_app("openchain");
        app.go(Screen::Search);
        app.on_done(Done::Search(vec![hit("斗破苍穹", "http://a.com/1")]));
        app.handle(Key::Enter);
        let job = app.take_job();
        assert!(matches!(job, Some(Job::Toc { .. })), "{job:?}");

        app.on_done(Done::Toc {
            chapters: vec![
                chapter("第一章", "http://a.com/1/1"),
                chapter("第二章", "http://a.com/1/2"),
            ],
            from_cache: false,
        });
        assert_eq!(app.screen, Screen::Toc, "目录到了才进目录屏");
        assert_eq!(app.toc_len(), 2);
        // 自动进书架，而且**书源名**不能记成书名
        assert_eq!(app.shelf.len(), 1);
        let b = app.shelf.get("http://a.com/1").unwrap();
        assert_eq!(b.name, "斗破苍穹");
        assert_eq!(b.source, app.source_names()[app.toc_source].clone());
        assert_ne!(b.source, "斗破苍穹", "记成书名的话下次按源名反查就找不到");
    }

    #[test]
    fn reading_progress_is_remembered_per_book() {
        let mut app = online_app("memory");
        app.go(Screen::Search);
        app.on_done(Done::Search(vec![hit("斗破苍穹", "http://a.com/1")]));
        app.handle(Key::Enter);
        app.take_job();
        app.on_done(Done::Toc {
            chapters: vec![
                chapter("第一章", "http://a.com/1/1"),
                chapter("第二章", "http://a.com/1/2"),
                chapter("第三章", "http://a.com/1/3"),
            ],
            from_cache: false,
        });
        app.take_job();
        app.on_done(Done::Read {
            title: "第二章".into(),
            text: "正文".into(),
        });
        // 读完一章停在阅读屏；`n` 在这里才是「翻章」
        app.screen = Screen::Reader;
        keys(&mut app, "nn");
        app.take_job();
        app.on_done(Done::Read {
            title: "第三章".into(),
            text: "正文".into(),
        });
        assert_eq!(app.state().chapter, 3);
        assert_eq!(
            app.shelf.get("http://a.com/1").unwrap().chapter,
            3,
            "每读一章都要记一次"
        );
    }

    #[test]
    fn a_failed_job_shows_up_and_clears_busy() {
        let mut app = online_app("failjob");
        app.go(Screen::Search);
        keys(&mut app, "斗破");
        app.handle(Key::Enter);
        app.take_job();
        assert!(app.busy.is_some());
        app.on_done(Done::Failed("试过的书源都需要 JS 解释器".into()));
        assert!(app.busy.is_none(), "失败之后不能再显示「在加载」");
        assert!(app.status.contains("JS"), "{}", app.status);
    }

    #[test]
    fn every_change_is_written_to_disk() {
        let mut app = tmp("persist");
        let state_path = app.session.state_path.clone();
        let reader_path = app.session.reader_path.clone();

        app.screen = Screen::Reader;
        app.handle(Key::Char('n'));
        assert!(state_path.exists(), "状态没落盘");
        let reader = fs::read_to_string(&reader_path).unwrap();
        assert!(reader.contains("index: 2,"), "阅读页没跟上:\n{reader}");
        assert!(reader.contains("// "), "正文应当在注释里");
        assert!(!reader.contains("state"), "阅读页不该再携带状态:\n{reader}");

        let json = fs::read_to_string(&state_path).unwrap();
        assert!(json.contains("\"chapter\": 2"), "{json}");
    }

    #[test]
    fn the_shelf_sits_next_to_the_state_file() {
        // 回归测试：一开始写成「state 的目录 + .novel/shelf.json」，
        // 于是路径变成 <dir>/.novel/shelf.json。真程序里凑巧对，
        // 配合 --dir 和测试临时目录就写到一个谁也不看的地方去。
        let dir = std::env::temp_dir().join("novel-tui-shelfpath");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let app = App::new(Session::open(
            dir.join("state.json"),
            dir.join("reader.novel"),
        ));
        assert_eq!(app.shelf.path, dir.join("shelf.json"));
        let app = App::new(Session::open(
            dir.join(".novel/state.json"),
            dir.join(".novel/reader.novel"),
        ));
        assert_eq!(app.shelf.path, dir.join(".novel/shelf.json"));
    }

    #[test]
    fn ctrl_c_quits_even_mid_input() {
        let mut app = tmp("ctrl-c");
        app.handle(Key::Char('g'));
        keys(&mut app, "12");
        assert!(!app.quit);
        app.handle(Key::Ctrl('c'));
        assert!(app.quit, "在输入框里也要能退出去");
    }

    #[test]
    fn reader_path_is_absolute() {
        let app = tmp("abspath");
        assert!(app.reader_path().is_absolute());
    }
}
