//! 阅读器的状态。
//!
//! 状态存在**独立的小 JSON 文件**里（`.novel/state.json`），不塞进阅读页 ——
//! 因为阅读页现在只负责「渲染随机 TS 代码 + 把小说放进注释」，
//! 它不该再兼任数据库。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::demo;
use crate::render::{render, Book, Chapter};

/// 状态文件（相对 worktree 根目录）。
pub const STATE_PATH: &str = ".novel/state.json";
/// 阅读页。固定路径：翻章靠整篇重写，靠编辑器自己重载。
pub const READER_PATH: &str = ".novel/reader.novel";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    /// 书架下标（0-based）。界面上显示成 1-based。
    pub book: usize,
    /// 1-based。
    pub chapter: usize,
    /// 搜索关键字（空串表示还没搜过）。
    pub query: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            book: 0,
            chapter: 1,
            query: String::new(),
        }
    }
}

impl State {
    pub fn book_def(&self) -> Option<&'static demo::BookDef> {
        demo::book(self.book)
    }

    pub fn total_chapters(&self) -> usize {
        demo::chapter_count(self.book)
    }

    /// 翻页。到头了返回 false。
    pub fn step(&mut self, delta: i64) -> bool {
        let before = self.chapter;
        if delta < 0 {
            self.chapter = self
                .chapter
                .saturating_sub(delta.unsigned_abs() as usize)
                .max(1);
        } else {
            self.chapter = (self.chapter + delta as usize).min(self.total_chapters().max(1));
        }
        self.chapter != before
    }

    /// 跳到第 n 章（1-based）。越界返回 false。
    pub fn goto(&mut self, n: usize) -> bool {
        if !(1..=self.total_chapters()).contains(&n) {
            return false;
        }
        self.chapter = n;
        true
    }

    /// 直接设章号，不校验。
    ///
    /// 在线模式下章数由**抓来的目录**决定，`State` 不知道有几章，
    /// 所以边界检查由调用方（界面）负责。
    pub fn set_chapter(&mut self, n: usize) {
        self.chapter = n.max(1);
    }

    /// 换书（0-based）。成功时章号回到 1。
    pub fn open_book(&mut self, book: usize) -> bool {
        if demo::book(book).is_none() {
            return false;
        }
        self.book = book;
        self.chapter = 1;
        true
    }

    pub fn book_obj(&self) -> Option<Book> {
        self.book_def().map(|b| Book {
            name: b.name.to_string(),
            author: b.author.to_string(),
            source: b.source.to_string(),
            total_chapters: b.chapters.len(),
        })
    }

    pub fn chapter_obj(&self) -> Option<Chapter> {
        demo::chapter(self.book, self.chapter).map(|(title, body)| Chapter {
            index: self.chapter,
            title: title.to_string(),
            paragraphs: body.iter().map(|p| p.to_string()).collect(),
        })
    }

    /// 渲染出来的阅读页内容。
    pub fn rendered(&self) -> Option<String> {
        Some(render(&self.book_obj()?, &self.chapter_obj()?))
    }
}

/// 阅读页当前在渲染什么。
///
/// 演示书库和「抓来的一章」在这里被抹平成同一个形状，于是渲染层
/// （`render()`）完全不用知道正文是从哪来的。
#[derive(Debug, Clone)]
enum Payload {
    Demo,
    Online { book: Book, chapter: Chapter },
}

/// 状态文件 + 阅读页，一起管起来。
#[derive(Debug, Clone)]
pub struct Session {
    pub state_path: PathBuf,
    pub reader_path: PathBuf,
    pub state: State,
    /// 不落盘：抓来的章节是易失的，重启后重新抓即可。
    payload: Payload,
}

impl Session {
    /// 打开（不要求文件存在）。状态文件里有东西就接着上次。
    pub fn open(state_path: impl Into<PathBuf>, reader_path: impl Into<PathBuf>) -> Self {
        let state_path = state_path.into();
        let state = fs::read_to_string(&state_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            state_path,
            reader_path: reader_path.into(),
            state,
            payload: Payload::Demo,
        }
    }

    /// 让阅读页去渲染抓来的这一章。
    pub fn show_online(&mut self, book: Book, chapter: Chapter) -> io::Result<()> {
        self.payload = Payload::Online { book, chapter };
        self.write_reader()?;
        Ok(())
    }

    /// 当前这一章的段落（给 TUI 的阅读屏显示）。
    ///
    /// 和 `reader.novel` 不是一回事：那个文件是**渲染出来的 TS**
    /// （正文在 `//` 注释里，给编辑器看的）；这里给的是同内容的纯文本。
    /// 两边都从同一份章节数据来，不会脱节。
    pub fn paragraphs(&self) -> Vec<String> {
        match &self.payload {
            Payload::Demo => self
                .state
                .chapter_obj()
                .map(|c| c.paragraphs)
                .unwrap_or_default(),
            Payload::Online { chapter, .. } => chapter.paragraphs.clone(),
        }
    }

    /// 阅读页现在渲染的是抓来的章节吗。
    pub fn is_online(&self) -> bool {
        matches!(self.payload, Payload::Online { .. })
    }

    /// 阅读页当前该有的内容。
    pub fn rendered(&self) -> Option<String> {
        match &self.payload {
            Payload::Demo => self.state.rendered(),
            Payload::Online { book, chapter } => Some(render(book, chapter)),
        }
    }

    /// 默认路径（相对 worktree 根目录）。
    pub fn default_paths() -> Self {
        Self::open(STATE_PATH, READER_PATH)
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(dir) = self.state_path.parent() {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(&self.state).unwrap_or_else(|_| "{}".into());
        fs::write(&self.state_path, format!("{json}\n"))
    }

    /// 写阅读页。
    ///
    /// **只写盘、不发 `workspace/applyEdit`** —— 这样编辑器那边的 buffer 永远是
    /// 「干净」的，磁盘一变它自己重载。既不会弹「文件已在磁盘上改变」，
    /// 也让面板和命令行两个写入方能共存。
    ///
    /// 内容没变就不写（避免无谓的重载）。
    pub fn write_reader(&self) -> io::Result<bool> {
        let Some(text) = self.rendered() else {
            return Ok(false);
        };
        if let Ok(old) = fs::read_to_string(&self.reader_path) {
            if old == text {
                return Ok(false);
            }
        }
        if let Some(dir) = self.reader_path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&self.reader_path, text)?;
        Ok(true)
    }

    pub fn save_and_write(&self) -> io::Result<()> {
        self.save()?;
        self.write_reader()?;
        Ok(())
    }
}

pub fn default_state_path() -> &'static Path {
    Path::new(STATE_PATH)
}

pub fn default_reader_path() -> &'static Path {
    Path::new(READER_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 抓来的一章也要能渲染成阅读页，而且和演示书走的是同一条渲染路径。
    #[test]
    fn online_chapter_takes_over_the_reader_page() {
        let dir = std::env::temp_dir().join("novel-state-online");
        let _ = fs::remove_dir_all(&dir);
        let mut s = Session::open(dir.join("state.json"), dir.join("reader.novel"));

        // 一开始是演示书
        assert!(!s.is_online());
        let demo = s.rendered().unwrap();
        assert!(demo.contains("夜色渐浓"), "演示书应当是默认");

        s.show_online(
            Book {
                name: "盗墓笔记".into(),
                author: "南派三叔".into(),
                source: "某书源".into(),
                total_chapters: 75,
            },
            Chapter {
                index: 1,
                title: "七星鲁王 第一章 血尸".into(),
                paragraphs: vec![
                    "50年前，长沙镖子岭。".into(),
                    "四个土夫子正蹲在一个土丘上。".into(),
                ],
            },
        )
        .unwrap();

        assert!(s.is_online());
        assert!(fs::read_to_string(dir.join("reader.novel"))
            .unwrap()
            .contains("盗墓笔记"));
        let text = s.rendered().unwrap();
        // 正文必须在注释里 —— 这是渲染层唯一的硬约束
        assert!(text.contains("// 50年前，长沙镖子岭。"), "{text}");
        assert!(text.contains("// 四个土夫子正蹲在一个土丘上。"), "{text}");
        assert!(text.contains("第 1 / 75 章"), "章数要来自抓来的目录");
        // 只有正文行带这两句，所以「正文没跑进代码里」是能断言的
        assert!(!text
            .lines()
            .any(|l| !l.trim_start().starts_with("//") && l.contains("长沙镖子岭")));
    }

    #[test]
    fn set_chapter_does_not_validate() {
        let mut st = State::default();
        st.set_chapter(999);
        assert_eq!(st.chapter, 999, "在线模式的边界由界面负责");
        st.set_chapter(0);
        assert_eq!(st.chapter, 1, "但至少不能是 0");
    }

    fn tmp(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("novel-state-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (dir.join("state.json"), dir.join("reader.novel"))
    }

    #[test]
    fn defaults_when_there_is_no_state_file() {
        let (s, r) = tmp("fresh");
        let session = Session::open(&s, &r);
        assert_eq!(session.state, State::default());
        assert_eq!(session.state.chapter, 1);
        assert!(session.state.query.is_empty());
    }

    #[test]
    fn state_round_trips_through_the_json_file() {
        let (sp, rp) = tmp("roundtrip");
        let mut session = Session::open(&sp, &rp);
        session.state.open_book(1);
        session.state.goto(3);
        session.state.query = "夜".into();
        session.save_and_write().unwrap();

        let reopened = Session::open(&sp, &rp);
        assert_eq!(reopened.state, session.state, "状态没往返成功");

        // 坏掉的 JSON 退回默认，而不是崩
        fs::write(&sp, "{ 这不是 json").unwrap();
        assert_eq!(Session::open(&sp, &rp).state, State::default());
    }

    #[test]
    fn reader_is_only_rewritten_when_the_content_changes() {
        let (sp, rp) = tmp("rewrite");
        let session = Session::open(&sp, &rp);
        assert!(session.write_reader().unwrap(), "第一次应当写");
        assert!(!session.write_reader().unwrap(), "内容没变不该再写");
        assert!(fs::read_to_string(&rp)
            .unwrap()
            .contains("export const chapter"));
    }

    #[test]
    fn page_turning_and_switching_are_bounded() {
        let (sp, rp) = tmp("bounds");
        let mut s = Session::open(&sp, &rp).state;
        assert!(!s.step(-1), "第 1 章再往前不动");
        assert!(s.step(1));
        assert_eq!(s.chapter, 2);
        s.chapter = s.total_chapters();
        assert!(!s.step(1), "最后一章再往后不动");
        assert_eq!(s.chapter, s.total_chapters());

        assert!(!s.goto(0), "0 不是合法章号");
        assert!(!s.goto(999));
        assert!(s.goto(1));
        assert_eq!(s.chapter, 1);

        s.chapter = 3;
        assert!(s.open_book(2), "换书要成功");
        assert_eq!((s.book, s.chapter), (2, 1), "换书后章号回到 1");
        assert!(!s.open_book(999), "不存在的书不该改动状态");
        assert_eq!(s.book, 2);
    }

    #[test]
    fn rendered_matches_the_current_chapter() {
        let (sp, rp) = tmp("rendered");
        let mut s = Session::open(&sp, &rp).state;
        s.goto(2);
        let text = s.rendered().unwrap();
        assert!(text.contains("index: 2,"));
        let (title, body) = demo::chapter(s.book, 2).unwrap();
        assert!(text.contains(&format!("// {}", body[0])));
        assert!(text.contains(title));
    }
}
