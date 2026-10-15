//! 本地 EPUB 书 —— 拆成「书名 / 作者 / 目录 / 每章正文」四样东西。
//!
//! 形状刻意和在线抓到的东西对齐（`crawl::Chapter`、`Done::Read` 的
//! `title` + 换行分段 `text`），这样界面层读本地书和读在线书走同一条路，
//! 不用分两套画法。
//!
//! EPUB 就是个 zip：
//! `META-INF/container.xml` 指出 OPF → OPF 里是元信息 + manifest + spine，
//! 章节目录在 nav.xhtml（EPUB3）或 toc.ncx（EPUB2）。
//!
//! **不支持的能力一律报错，不猜**：打不开的 epub 在 [`scan_dir`] 里被跳过，
//! 读不到的章节在 [`Epub::chapter_text`] 里报出来，绝不静默返回空正文。

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use crate::html::{format_text, Html};
use crate::{Error, R};

/// 本地书在书架上的「书源名」。界面靠它区分本地书和在线书。
pub const LOCAL_SOURCE: &str = "本地";

/// 书架上认本地书的前缀。`BookSource` 那边全是 http(s)，不会撞。
pub const URL_SCHEME: &str = "epub://";

/// 一本地书里的章节在书架 / 目录里怎么表示。
pub fn url_of(path: &Path) -> String {
    format!("{URL_SCHEME}{}", path.display())
}

/// `url_of` 的反向。
pub fn path_of(url: &str) -> Option<&str> {
    url.strip_prefix(URL_SCHEME)
}

fn bad(what: impl Into<String>) -> Error {
    Error::Net(what.into())
}

/// 一章。`href` 是 **zip 内的完整路径**（已经和 OPF 所在目录拼好）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapter {
    pub title: String,
    pub href: String,
}

/// 一本拆好的 epub。**不持有 zip**，每次读正文重开一次 ——
/// 一本几 MB 的书重开一次比常驻几十 MB 解压结果划算。
#[derive(Debug, Clone)]
pub struct Epub {
    pub path: PathBuf,
    pub title: String,
    pub author: String,
    pub chapters: Vec<Chapter>,
}

impl Epub {
    pub fn open(path: impl AsRef<Path>) -> R<Self> {
        let path = path.as_ref().to_path_buf();
        let bytes = std::fs::read(&path)
            .map_err(|e| bad(format!("读不了 {}：{e}", path.display())))?;
        let mut zip = zip_of(&bytes)?;

        // 1. container.xml → OPF
        let opf_path = rootfile(&read_entry(&mut zip, "META-INF/container.xml")?)?;
        let opf_dir = dir_of(&opf_path);
        let opf = read_entry(&mut zip, &opf_path)?;
        let doc = roxmltree::Document::parse(&opf)
            .map_err(|e| bad(format!("{opf_path} 不是合法 XML：{e}")))?;

        // 2. 元信息
        let title = text_of(&doc, "title")
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| stem_of(&path.display().to_string()));
        let author = text_of(&doc, "creator").unwrap_or_default();

        // 3. manifest（id → zip 内路径）与 spine
        let mut by_id: HashMap<String, String> = HashMap::new();
        let mut media: HashMap<String, String> = HashMap::new();
        let mut props: HashMap<String, String> = HashMap::new();
        for item in elements(&doc, "item") {
            let (Some(id), Some(href)) = (item.attribute("id"), item.attribute("href")) else {
                continue;
            };
            let full = join(&opf_dir, href);
            by_id.insert(id.to_string(), full.clone());
            media.insert(full.clone(), item.attribute("media-type").unwrap_or("").into());
            props.insert(full.clone(), item.attribute("properties").unwrap_or("").into());
        }
        let spine: Vec<String> = elements(&doc, "itemref")
            .into_iter()
            .filter_map(|n| n.attribute("idref"))
            .filter_map(|id| by_id.get(id).cloned())
            .collect();

        // 4. 目录：nav.xhtml → toc.ncx → spine 顺序（书名退回文件名）
        let nav = props
            .iter()
            .find(|(_, p)| p.split_whitespace().any(|x| x == "nav"))
            .map(|(h, _)| h.clone());
        let mut chapters = Vec::new();
        if let Some(h) = &nav {
            // 声明了 `properties="nav"` 但文件不在/解不开的书真的存在 ——
            // 那种情况退回 ncx / spine，而不是整本打不开
            if let Ok(text) = read_entry(&mut zip, h) {
                chapters = nav_chapters(&text, &dir_of(h));
            }
        }
        if chapters.is_empty() {
            if let Some(ncx) = ncx_path(&doc, &by_id, &media) {
                chapters = ncx_chapters(&read_entry(&mut zip, &ncx)?, &dir_of(&ncx));
            }
        }
        if chapters.is_empty() {
            chapters = spine
                .iter()
                .map(|h| Chapter {
                    title: stem_of(h),
                    href: h.clone(),
                })
                .collect();
        }
        if chapters.is_empty() {
            return Err(bad(format!("{} 里一章都没有", path.display())));
        }

        Ok(Self {
            path,
            title,
            author,
            chapters,
        })
    }

    /// 第 i 章（0-based）的正文，**换行分段** —— 和 `Done::Read` 的 `text` 同形状。
    pub fn chapter_text(&self, i: usize) -> R<String> {
        let ch = self.chapters.get(i).ok_or_else(|| {
            bad(format!("没有第 {} 章（共 {} 章）", i + 1, self.chapters.len()))
        })?;
        let bytes = std::fs::read(&self.path)
            .map_err(|e| bad(format!("读不了 {}：{e}", self.path.display())))?;
        let mut zip = zip_of(&bytes)?;
        let xhtml = read_entry(&mut zip, &ch.href)?;
        Ok(clean_body(&xhtml, &ch.title))
    }
}

/// 扫一个目录里的 `*.epub`。**打不开的直接跳过** —— 一个坏文件不该挡住启动，
/// 也不该让别的书消失。顺序按路径排，保证每帧稳定。
pub fn scan_dir(dir: &Path) -> Vec<Epub> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|x| x.eq_ignore_ascii_case("epub"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    files.iter().filter_map(|p| Epub::open(p).ok()).collect()
}

// ── zip / XML 小工具 ────────────────────────────────────────────

type Zip<'a> = zip::ZipArchive<Cursor<&'a [u8]>>;

fn zip_of(bytes: &[u8]) -> R<Zip<'_>> {
    zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| bad(format!("不是合法的 zip/epub：{e}")))
}

fn read_entry(zip: &mut Zip<'_>, name: &str) -> R<String> {
    let mut f = zip
        .by_name(name)
        .map_err(|e| bad(format!("epub 里没有 {name}：{e}")))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|e| bad(format!("解不开 {name}：{e}")))?;
    Ok(decode(&buf))
}

/// EPUB 规范要求 UTF-8，但老书/中文书有 GBK 的。解不开再退回 GBK。
fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let (s, _, _) = encoding_rs::GBK.decode(bytes);
            s.into_owned()
        }
    }
}

fn elements<'a, 'i>(doc: &'i roxmltree::Document<'a>, name: &str) -> Vec<roxmltree::Node<'a, 'i>> {
    doc.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == name)
        .collect()
}

/// 按**局部名**取第一个元素的文本。OPF 里 `<dc:title>` 的局部名就是 `title`。
fn text_of(doc: &roxmltree::Document<'_>, name: &str) -> Option<String> {
    elements(doc, name)
        .into_iter()
        .next()
        .map(|n| n.text().unwrap_or("").trim().to_string())
}

/// `META-INF/container.xml` → OPF 路径。
fn rootfile(container: &str) -> R<String> {
    let doc = roxmltree::Document::parse(container)
        .map_err(|e| bad(format!("container.xml 不是合法 XML：{e}")))?;
    elements(&doc, "rootfile")
        .into_iter()
        .find_map(|n| n.attribute("full-path"))
        .map(str::to_string)
        .ok_or_else(|| bad("container.xml 里没有 rootfile"))
}

/// EPUB2 的目录路径：先看 `<spine toc="...">`，再看有没有 ncx 的 media-type。
fn ncx_path(
    doc: &roxmltree::Document<'_>,
    by_id: &HashMap<String, String>,
    media: &HashMap<String, String>,
) -> Option<String> {
    elements(doc, "spine")
        .into_iter()
        .filter_map(|n| n.attribute("toc"))
        .find_map(|id| by_id.get(id).cloned())
        .or_else(|| {
            media
                .iter()
                .find(|(_, m)| m.contains("dtbncx"))
                .map(|(h, _)| h.clone())
        })
}

/// EPUB3 的 `nav.xhtml`：文档序里的每个 `<a href>`。
///
/// **按 href 去重**：真书里见过整份目录被原样写了两遍的
/// （Tomato-Novel-Downloader 出的那本就重复了一次，1692 条 = 846 × 2），
/// 不去重的话目录里每本书都会出现两轮。
///
/// 用自家的 `Html` 而不是正则 —— `tl` 已经是依赖了，正则解 XML 是自找麻烦。
fn nav_chapters(nav: &str, base: &str) -> Vec<Chapter> {
    let html = Html::parse(nav);
    let mut seen = std::collections::HashSet::new();
    html.subtree(None)
        .into_iter()
        .filter(|&i| html.tag_name(i) == "a")
        .filter_map(|i| {
            let href = html.attr(i, "href");
            let title = html.text(i).trim().to_string();
            if href.is_empty() || title.is_empty() {
                return None;
            }
            let full = join(base, &href);
            if !seen.insert(full.clone()) {
                return None;
            }
            Some(Chapter { title, href: full })
        })
        .collect()
}

/// EPUB2 的 `toc.ncx`：每个 `<navPoint>` 是一章。`src` 相对 ncx 自己。
fn ncx_chapters(ncx: &str, base: &str) -> Vec<Chapter> {
    let Ok(doc) = roxmltree::Document::parse(ncx) else {
        return Vec::new();
    };
    elements(&doc, "navPoint")
        .into_iter()
        .filter_map(|p| {
            let src = p
                .descendants()
                .find(|n| n.is_element() && n.tag_name().name() == "content")
                .and_then(|n| n.attribute("src"))?;
            let title = p
                .descendants()
                .find(|n| n.is_element() && n.tag_name().name() == "text")
                .and_then(|n| n.text())
                .unwrap_or("")
                .trim();
            if title.is_empty() {
                return None;
            }
            Some(Chapter {
                title: title.to_string(),
                href: join(base, src),
            })
        })
        .collect()
}

/// 只留 `<body>` 再清洗：不然 `<title>` 里的章节名会混进正文。
///
/// 正文第一段常常就是章节名（epub 里是 `<h1>`），标题栏已经显示了，
/// 去掉避免读起来重复。
fn clean_body(xhtml: &str, title: &str) -> String {
    let html = Html::parse(xhtml);
    let body = html.select(None, "body").ok().and_then(|v| v.first().copied());
    let raw = match body {
        Some(i) => html.outer_html(i),
        None => xhtml.to_string(),
    };
    let text = format_text(&raw);
    let mut lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim_matches('\u{3000}').trim())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.first().is_some_and(|f| *f == title.trim()) {
        lines.remove(0);
    }
    lines.join("\n")
}

/// 目录名（不含最后的文件名）。`"OEBPS/content.opf"` → `"OEBPS"`。
fn dir_of(path: &str) -> String {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("").to_string()
}

/// 文件名去掉扩展名。`"OEBPS/chapter_1.xhtml"` → `"chapter_1"`。
fn stem_of(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name).to_string()
}

/// 把 epub 内部的相对路径拼成 zip 内的完整路径。
///
/// 要处理三件事：`#` 锚点、`%20` 这种转义、`../` 回溯。
fn join(base: &str, href: &str) -> String {
    let href = percent_decode(href.split('#').next().unwrap_or(""));
    let raw = if let Some(stripped) = href.strip_prefix('/') {
        stripped.to_string()
    } else if base.is_empty() {
        href
    } else {
        format!("{base}/{href}")
    };
    let mut out: Vec<&str> = Vec::new();
    for seg in raw.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// 只解 `%XX`（epub 里只有 ASCII 需要转义）。解不开就原样留着。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 现造一本最小 epub。不依赖仓库里的文件 —— 那个 3.7 MB 的书不该进 git。
    fn make_epub(tag: &str, with_nav: bool) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("novel-epub-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("书.epub");

        let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let opt = zip::write::SimpleFileOptions::default();
        let mut put = |name: &str, body: &str| {
            z.start_file(name, opt).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        };
        put("mimetype", "application/epub+zip");
        put(
            "META-INF/container.xml",
            r#"<?xml version="1.0"?><container><rootfiles>
                 <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
               </rootfiles></container>"#,
        );
        put(
            "OEBPS/content.opf",
            &format!(
                r#"<?xml version="1.0"?>
               <package xmlns="http://www.idpf.org/2007/opf" version="3.0">
                 <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
                   <dc:title>测试书</dc:title>
                   <dc:creator>某作者</dc:creator>
                 </metadata>
                 <manifest>
                   {nav_item}
                   <item id="c1" href="text/c1.xhtml" media-type="application/xhtml+xml"/>
                   <item id="c2" href="text/c2.xhtml" media-type="application/xhtml+xml"/>
                 </manifest>
                 <spine><itemref idref="c1"/><itemref idref="c2"/></spine>
               </package>"#,
                nav_item = if with_nav {
                    r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>"#
                } else {
                    ""
                }
            ),
        );
        if with_nav {
            put(
                "OEBPS/nav.xhtml",
                r#"<html><body><nav epub:type="toc"><ol>
                     <li><a href="text/c1.xhtml">第一章 起</a></li>
                     <li><a href="text/c2.xhtml">第二章 承</a></li>
                   </ol><ol>
                     <li><a href="text/c1.xhtml">第一章 起</a></li>
                     <li><a href="text/c2.xhtml">第二章 承</a></li>
                   </ol></nav></body></html>"#,
            );
        }
        // 故意把章节标题放在 <title> 和 <h1> 里 —— format_text 不该把
        // <title> 的内容漏进正文，也不该让标题重复成第一段
        put(
            "OEBPS/text/c1.xhtml",
            "<html><head><title>第一章 起</title></head><body>\
             <h1>第一章 起</h1><p>甲。</p><p>乙。</p></body></html>",
        );
        put(
            "OEBPS/text/c2.xhtml",
            "<html><head><title>第二章 承</title></head><body>\
             <h1>第二章 承</h1><p>丙。</p></body></html>",
        );
        z.finish().unwrap();
        path
    }

    #[test]
    fn reads_metadata_and_nav_toc() {
        // 夹具里的 nav 故意把整份目录写了两遍 —— 真书就这么干过
        let e = Epub::open(make_epub("nav", true)).unwrap();
        assert_eq!(e.title, "测试书");
        assert_eq!(e.author, "某作者");
        let titles: Vec<&str> = e.chapters.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["第一章 起", "第二章 承"], "重复的目录条目要去掉");
        assert_eq!(e.chapters[0].href, "OEBPS/text/c1.xhtml");
        assert_eq!(e.chapters[1].href, "OEBPS/text/c2.xhtml");
    }

    #[test]
    fn falls_back_to_spine_when_there_is_no_nav() {
        let e = Epub::open(make_epub("nonav", false)).unwrap();
        assert_eq!(e.chapters.len(), 2);
        assert_eq!(e.chapters[0].title, "c1", "没有目录时退回文件名");
        assert_eq!(e.chapters[0].href, "OEBPS/text/c1.xhtml");
    }

    /// 声明了 nav 但文件不在：退回 spine，而不是整本打不开。
    #[test]
    fn a_declared_but_missing_nav_does_not_kill_the_book() {
        let dir = std::env::temp_dir().join("novel-epub-navgone");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = make_epub("navgone-src", true);
        let dst = dir.join("书.epub");

        // 拷一份，把 nav.xhtml 从 zip 里去掉
        {
            let mut zin = zip::ZipArchive::new(std::fs::File::open(&src).unwrap()).unwrap();
            let mut zout = zip::ZipWriter::new(std::fs::File::create(&dst).unwrap());
            let opt = zip::write::SimpleFileOptions::default();
            for i in 0..zin.len() {
                let mut f = zin.by_index(i).unwrap();
                let name = f.name().to_string();
                if name.ends_with("nav.xhtml") {
                    continue;
                }
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).unwrap();
                drop(f);
                zout.start_file(name, opt).unwrap();
                zout.write_all(&buf).unwrap();
            }
            zout.finish().unwrap();
        }

        let e = Epub::open(&dst).unwrap();
        assert_eq!(e.chapters.len(), 2, "退回 spine 的两章");
        assert_eq!(e.chapters[0].title, "c1");
    }

    #[test]
    fn chapter_text_is_paragraphs_and_has_no_title_duplicate() {
        let e = Epub::open(make_epub("text", true)).unwrap();
        let t = e.chapter_text(0).unwrap();
        assert!(!t.contains("<"), "标签要清掉：{t}");
        assert!(!t.contains("第一章 起"), "标题在 <title>/<h1> 里，不该混进正文：{t}");
        assert!(t.contains("甲。"), "{t}");
        assert!(t.contains("乙。"), "{t}");
        assert_eq!(t.lines().count(), 2, "两段：{t}");
    }

    #[test]
    fn bad_files_are_skipped_not_fatal() {
        let dir = std::env::temp_dir().join("novel-epub-scan");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("坏的.epub"), "这不是 zip").unwrap();
        std::fs::write(dir.join("无关.txt"), "x").unwrap();
        let good = make_epub("scan-good", true);
        std::fs::copy(&good, dir.join("好的.epub")).unwrap();

        let found = scan_dir(&dir);
        assert_eq!(found.len(), 1, "只该找到那本好的");
        assert_eq!(found[0].title, "测试书");
    }

    #[test]
    fn out_of_range_chapter_errors() {
        let e = Epub::open(make_epub("range", true)).unwrap();
        assert!(e.chapter_text(9).is_err(), "越界要报错，不能返回空正文");
    }

    /// 拿一本真 epub 验证（默认不跑）：
    /// `NOVEL_TEST_EPUB=某.epub cargo test -p novel-core -- --ignored real_epub`
    #[test]
    #[ignore = "需要一个真实 epub（环境变量 NOVEL_TEST_EPUB）"]
    fn real_epub_reads_end_to_end() {
        let path = std::env::var("NOVEL_TEST_EPUB").expect("要给 NOVEL_TEST_EPUB");
        let e = Epub::open(&path).unwrap();
        println!(
            "《{}》{} · {} 章 · 首章「{}」",
            e.title,
            e.author,
            e.chapters.len(),
            e.chapters.first().map(|c| c.title.as_str()).unwrap_or("")
        );
        assert!(!e.title.is_empty());
        assert!(e.chapters.len() > 100, "小说应当有很多章");
        // 抽三章读，每章都要真出正文
        for i in [0, e.chapters.len() / 2, e.chapters.len() - 1] {
            let t = e.chapter_text(i).unwrap();
            println!("第 {} 章「{}」 {} 字", i + 1, e.chapters[i].title, t.chars().count());
            assert!(t.chars().count() > 50, "第 {} 章正文太短：{t}", i + 1);
            assert!(!t.contains("<"), "第 {} 章有没清干净的标签", i + 1);
        }
    }

    #[test]
    fn url_round_trip() {
        let p = PathBuf::from("F:/x/书.epub");
        let u = url_of(&p);
        assert_eq!(u, "epub://F:/x/书.epub");
        assert_eq!(path_of(&u).unwrap(), p.display().to_string());
        assert!(path_of("http://a/b").is_none());
    }

    #[test]
    fn join_handles_fragments_escapes_and_parent() {
        assert_eq!(join("OEBPS", "text/c1.xhtml"), "OEBPS/text/c1.xhtml");
        assert_eq!(join("OEBPS", "c1.xhtml#p2"), "OEBPS/c1.xhtml");
        assert_eq!(join("OEBPS", "../a.xhtml"), "a.xhtml");
        assert_eq!(join("OEBPS", "/abs.xhtml"), "abs.xhtml");
        assert_eq!(join("", "c1.xhtml"), "c1.xhtml");
        assert_eq!(join("OEBPS", "a%20b.xhtml"), "OEBPS/a b.xhtml");
    }
}
