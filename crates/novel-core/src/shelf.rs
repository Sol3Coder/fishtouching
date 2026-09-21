//! 书架 —— 记住你在读哪些书、每本读到第几章。
//!
//! 单独一个文件（`.novel/shelf.json`），不塞进 `state.json`：
//! `state.json` 是「当前这一刻的界面状态」（光标、搜索词、书源开关），
//! 而书架是「长期数据」，生命周期完全不一样。
//!
//! **按 URL 认书**，不按书源下标 —— 书源列表会因为导入/删除而重排，
//! 用下标当主键的话，删一个源就会把别人的阅读进度串到另一本书上。
//! 书源名只用来显示（源被删了书架条目也还在，只是打不开）。

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 书架文件（相对 worktree 根目录）。
pub const SHELF_PATH: &str = ".novel/shelf.json";

/// 光文件名。和 `state.json` 放同一个目录时用这个 ——
/// 用 `SHELF_PATH` 会把 `.novel/` 前缀算两遍（`<dir>/.novel/shelf.json`）。
pub const SHELF_FILE: &str = "shelf.json";

/// 书架上一本书。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ShelfBook {
    /// 书源名。只用来显示 —— 按名字找不到源时界面会说明。
    pub source: String,
    pub name: String,
    pub author: String,
    /// 详情页 URL。**这就是主键**。
    pub url: String,
    /// 目录页（详情页里 `tocUrl` 指到的那个）。为空表示详情页自己就是目录页。
    pub toc_url: String,
    /// 上次读到第几章（1-based）。
    pub chapter: usize,
    /// 那一章的标题，用来在书架上显示「读到哪了」。
    pub title: String,
    /// 上次已知的总章数（打开时会刷新）。
    pub chapters: usize,
    /// 最后一次阅读时间（unix 秒）。书架按它倒序。
    pub read_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct File {
    books: Vec<ShelfBook>,
}

/// 书架。文件不存在就是空架子 —— 不是错误。
#[derive(Debug, Clone)]
pub struct Shelf {
    pub path: PathBuf,
    pub books: Vec<ShelfBook>,
}

impl Shelf {
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let books = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<File>(&t).ok())
            .map(|f| f.books)
            .unwrap_or_default();
        Self { path, books }
    }

    pub fn default_path() -> Self {
        Self::load(SHELF_PATH)
    }

    /// 写盘。内容没变就不写（避免无谓的 IO 和 mtime 抖动）。
    pub fn save(&self) -> io::Result<()> {
        let json = serde_json::to_string_pretty(&File {
            books: self.books.clone(),
        })
        .unwrap_or_else(|_| "{}".into());
        let text = format!("{json}\n");
        if let Ok(old) = fs::read_to_string(&self.path) {
            if old == text {
                return Ok(());
            }
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&self.path, text)
    }

    pub fn is_empty(&self) -> bool {
        self.books.is_empty()
    }

    pub fn len(&self) -> usize {
        self.books.len()
    }

    /// 已在书架里吗（按 URL 认）。
    pub fn contains(&self, url: &str) -> bool {
        self.index_of(url).is_some()
    }

    fn index_of(&self, url: &str) -> Option<usize> {
        self.books.iter().position(|b| b.url == url)
    }

    pub fn get(&self, url: &str) -> Option<&ShelfBook> {
        self.index_of(url).map(|i| &self.books[i])
    }

    /// 加一本（已经在架子上就更新元信息，不动阅读进度）。
    ///
    /// 返回它在 `books` 里的下标 —— 界面切过去时要用。
    pub fn add(&mut self, book: ShelfBook) -> usize {
        match self.index_of(&book.url) {
            Some(i) => {
                let old = self.books[i].clone();
                // 进度是「资产」：重新加进来不能把读到哪冲掉
                self.books[i] = ShelfBook {
                    chapter: old.chapter.max(1),
                    title: old.title,
                    read_at: old.read_at.max(book.read_at),
                    ..book
                };
                i
            }
            None => {
                self.books.push(book);
                self.books.len() - 1
            }
        }
    }

    pub fn remove(&mut self, url: &str) -> bool {
        match self.index_of(url) {
            Some(i) => {
                self.books.remove(i);
                true
            }
            None => false,
        }
    }

    /// 记下「读到这一章了」。书不在架子上就什么都不做。
    pub fn remember(&mut self, url: &str, chapter: usize, title: &str, now: i64) {
        if let Some(i) = self.index_of(url) {
            self.books[i].chapter = chapter.max(1);
            self.books[i].title = title.to_string();
            self.books[i].read_at = now;
        }
    }

    /// 刷新总章数并记时间（打开书的时候用）。
    pub fn touch(&mut self, url: &str, chapters: usize, now: i64) {
        if let Some(i) = self.index_of(url) {
            if chapters > 0 {
                self.books[i].chapters = chapters;
            }
            self.books[i].read_at = now;
        }
    }

    /// 最近在读的那本（书架为空时 `None`）。启动时用它恢复现场。
    pub fn most_recent(&self) -> Option<&ShelfBook> {
        self.books.iter().max_by_key(|b| b.read_at)
    }

    /// 按最近阅读倒序的下标。书架面板照这个顺序画。
    pub fn order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.books.len()).collect();
        // 时间相同时用 URL 兜底，保证顺序稳定（不然每帧都可能换位）
        idx.sort_by(|&a, &b| {
            self.books[b]
                .read_at
                .cmp(&self.books[a].read_at)
                .then_with(|| self.books[a].url.cmp(&self.books[b].url))
        });
        idx
    }

    /// 当前时间戳（秒）。单独抽出来是为了让测试能喂固定值。
    pub fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("novel-shelf-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn book(url: &str, name: &str) -> ShelfBook {
        ShelfBook {
            source: "某源".into(),
            name: name.into(),
            author: "作者".into(),
            url: url.into(),
            ..Default::default()
        }
    }

    #[test]
    fn missing_file_is_an_empty_shelf_not_an_error() {
        let dir = tmp("empty");
        let s = Shelf::load(dir.join("shelf.json"));
        assert!(s.is_empty());
        assert!(s.most_recent().is_none());
        assert_eq!(s.order(), Vec::<usize>::new());
        // 空架子也能保存
        s.save().unwrap();
        assert!(Shelf::load(dir.join("shelf.json")).is_empty());
    }

    #[test]
    fn survives_a_round_trip() {
        let dir = tmp("roundtrip");
        let path = dir.join("shelf.json");
        let mut s = Shelf::load(&path);
        s.add(ShelfBook {
            chapter: 7,
            title: "第七章".into(),
            chapters: 100,
            read_at: 1000,
            ..book("http://a/1", "斗破苍穹")
        });
        s.save().unwrap();

        let again = Shelf::load(&path);
        assert_eq!(again.len(), 1);
        let b = again.get("http://a/1").unwrap();
        assert_eq!(b.name, "斗破苍穹");
        assert_eq!(b.chapter, 7);
        assert_eq!(b.title, "第七章");
        assert_eq!(b.chapters, 100);
        assert_eq!(b.read_at, 1000);
    }

    #[test]
    fn bad_json_degrades_to_an_empty_shelf() {
        let dir = tmp("bad");
        let path = dir.join("shelf.json");
        fs::write(&path, "{ 这不是 json").unwrap();
        assert!(
            Shelf::load(&path).is_empty(),
            "坏文件不能崩，也不能挡住启动"
        );
    }

    #[test]
    fn identity_is_the_url_not_the_position() {
        // 这是这个模块存在的主要理由：书源列表会因为导入/删除重排，
        // 用下标当主键会把别人的阅读进度串到另一本书上。
        let dir = tmp("identity");
        let mut s = Shelf::load(dir.join("shelf.json"));
        s.add(ShelfBook {
            chapter: 42,
            ..book("http://a/1", "第一本")
        });
        s.add(book("http://a/2", "第二本"));
        s.remember("http://a/2", 9, "第九章", 500);

        assert_eq!(s.get("http://a/1").unwrap().chapter, 42);
        assert_eq!(s.get("http://a/2").unwrap().chapter, 9);

        // 删掉第一本，第二本的进度不能串位
        assert!(s.remove("http://a/1"));
        assert_eq!(s.len(), 1);
        assert_eq!(s.get("http://a/2").unwrap().chapter, 9);
        assert!(s.get("http://a/1").is_none());
        assert!(!s.remove("http://a/1"), "删不存在的要返回 false");
    }

    #[test]
    fn re_adding_keeps_the_reading_progress() {
        let dir = tmp("readd");
        let mut s = Shelf::load(dir.join("shelf.json"));
        s.add(ShelfBook {
            chapter: 300,
            title: "第三百章".into(),
            read_at: 900,
            ..book("http://a/1", "斗破苍穹")
        });
        // 重新「加入书架」（比如从搜索结果再点一次收藏）
        let i = s.add(ShelfBook {
            chapter: 1,
            chapters: 1663,
            read_at: 100,
            ..book("http://a/1", "斗破苍穹（新）")
        });
        assert_eq!(i, 0);
        assert_eq!(s.len(), 1, "同一个 URL 不能变成两条");
        let b = s.get("http://a/1").unwrap();
        assert_eq!(b.name, "斗破苍穹（新）", "元信息要更新");
        assert_eq!(b.chapter, 300, "但阅读进度是资产，不能被冲掉");
        assert_eq!(b.title, "第三百章");
        assert_eq!(b.read_at, 900, "时间取较大的那个");
        assert_eq!(b.chapters, 1663);
    }

    #[test]
    fn order_is_most_recently_read_first_and_stable() {
        let dir = tmp("order");
        let mut s = Shelf::load(dir.join("shelf.json"));
        s.add(ShelfBook {
            read_at: 100,
            ..book("http://a/1", "A")
        });
        s.add(ShelfBook {
            read_at: 300,
            ..book("http://a/2", "B")
        });
        s.add(ShelfBook {
            read_at: 200,
            ..book("http://a/3", "C")
        });
        let names: Vec<&str> = s
            .order()
            .iter()
            .map(|&i| s.books[i].name.as_str())
            .collect();
        assert_eq!(names, ["B", "C", "A"]);
        assert_eq!(s.most_recent().unwrap().name, "B");

        // 时间相同（比如一次导入带进来的）时顺序要稳定，否则界面每帧都在跳
        let mut s2 = Shelf::load(dir.join("s2.json"));
        for u in ["http://b/2", "http://b/1", "http://b/3"] {
            s2.add(ShelfBook {
                read_at: 7,
                ..book(u, u)
            });
        }
        let a = s2.order();
        assert_eq!(a, s2.order());
        assert_eq!(a, s2.order(), "重复调用结果必须一致");
    }

    #[test]
    fn remember_and_touch_only_affect_books_that_are_on_the_shelf() {
        let mut s = Shelf::load(tmp("touch").join("shelf.json"));
        s.add(book("http://a/1", "A"));
        s.remember("http://a/1", 5, "第五章", 111);
        assert_eq!(s.get("http://a/1").unwrap().chapter, 5);
        // 不在架子上的书：静默忽略，不能凭空创建
        s.remember("http://nope/9", 3, "x", 111);
        s.touch("http://nope/9", 50, 111);
        assert_eq!(s.len(), 1);

        // 章数是可选刷新的：传 0 表示「不知道」，不能把已知的冲成 0
        s.touch("http://a/1", 0, 222);
        assert_eq!(s.get("http://a/1").unwrap().chapters, 0);
        s.touch("http://a/1", 88, 222);
        assert_eq!(s.get("http://a/1").unwrap().chapters, 88);
    }

    #[test]
    fn chapter_is_at_least_one() {
        let mut s = Shelf::load(tmp("clamp").join("shelf.json"));
        s.add(book("http://a/1", "A"));
        s.remember("http://a/1", 0, "x", 5);
        assert_eq!(s.get("http://a/1").unwrap().chapter, 1, "第 0 章不存在");
    }

    #[test]
    fn save_skips_writing_when_nothing_changed() {
        let dir = tmp("noop");
        let path = dir.join("shelf.json");
        let mut s = Shelf::load(&path);
        s.add(book("http://a/1", "A"));
        s.save().unwrap();
        let first = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        s.save().unwrap();
        assert_eq!(
            first,
            fs::metadata(&path).unwrap().modified().unwrap(),
            "内容没变就不该重写，否则外部监听会一直被打扰"
        );
    }
}
