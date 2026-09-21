//! P0 演示书库。P1 起换成真的书源抓取。

pub struct BookDef {
    pub name: &'static str,
    pub author: &'static str,
    pub source: &'static str,
    /// (章标题, 段落)
    pub chapters: &'static [(&'static str, &'static [&'static str])],
}

/// 演示书源。配置页就是给这几个开关用的。
pub const SOURCES: &[&str] = &["演示源·站点A", "演示源·站点B", "演示源·站点C"];

pub const BOOKS: &[BookDef] = &[
    BookDef {
        name: "夜色渐浓",
        author: "示例作者",
        source: SOURCES[0],
        chapters: &[
            (
                "推开门",
                &[
                    "他推开门，楼道里的声控灯亮了一下，又灭了。",
                    "外面在下雨。雨点打在铁皮雨棚上，像有人在楼上不停地挪椅子。",
                ],
            ),
            (
                "风灌了进来",
                &[
                    "风灌了进来，吹得桌上的稿纸哗哗作响。",
                    "他伸手去按，纸还是散了一地。",
                    "“你终于来了。”那人背对着他说。",
                ],
            ),
            (
                "稿纸哗哗作响",
                &[
                    "稿纸哗哗作响，一直响到后半夜。",
                    "他数了三遍，还是少一张。",
                    "少的正是写着结尾的那一张。",
                ],
            ),
        ],
    },
    BookDef {
        name: "长夜将尽",
        author: "另一个作者",
        source: SOURCES[1],
        chapters: &[
            (
                "第一节 起雾",
                &[
                    "雾从江面上升起来的时候，渡口还没有开。",
                    "他在石阶上坐了很久，直到鞋底被露水浸透。",
                ],
            ),
            (
                "第二节 摆渡人",
                &[
                    "摆渡人把竹篙往水里一插，船就停住了。",
                    "“过了这条江，你就回不来了。”",
                ],
            ),
            (
                "第三节 对岸",
                &["对岸没有灯。", "但他看见了人影，一排，站得很齐。"],
            ),
            ("第四节 天亮", &["天亮的时候雾散了。", "江面上什么都没有。"]),
        ],
    },
    BookDef {
        name: "雨落长安",
        author: "第三作者",
        source: SOURCES[2],
        chapters: &[
            ("卷一 · 入城", &["雨落长安，朱雀大街上一个行人也没有。"]),
            ("卷二 · 灯市", &["灯市照旧开着，只是摊主都压低了声音说话。"]),
            ("卷三 · 旧识", &["“你怎么回来了。”", "“回来取一样东西。”"]),
            ("卷四 · 出城", &["城门在身后合上，雨还在下。"]),
        ],
    },
];

pub fn books() -> &'static [BookDef] {
    BOOKS
}

pub fn book(index: usize) -> Option<&'static BookDef> {
    BOOKS.get(index)
}

pub fn sources() -> &'static [&'static str] {
    SOURCES
}

pub fn chapter(
    book_index: usize,
    chapter: usize,
) -> Option<(&'static str, &'static [&'static str])> {
    let b = book(book_index)?;
    let i = chapter.checked_sub(1)?;
    b.chapters.get(i).copied()
}

pub fn chapter_count(book_index: usize) -> usize {
    book(book_index).map(|b| b.chapters.len()).unwrap_or(0)
}

/// 一个搜索结果。
pub struct Hit {
    pub book: usize,
    /// 有几个书源有这本书 —— 演示「多源都有」，P3 换成真的。
    pub origins: usize,
}

pub fn search(query: &str) -> Vec<Hit> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    // 忽略大小写，方便用英文/拼音试
    let q_lower = q.to_lowercase();
    BOOKS
        .iter()
        .enumerate()
        .filter(|(_, b)| {
            b.name.to_lowercase().contains(&q_lower) || b.author.to_lowercase().contains(&q_lower)
        })
        .map(|(i, _)| Hit {
            book: i,
            origins: demo_origins(i),
        })
        .collect()
}

// ponytail: 演示用的固定源数。真书源已经接上了（见 `online` / `crawl`），
// 这个函数留着只是让 `--offline` 模式下「几个源都有」那栏有东西显示。
fn demo_origins(book_index: usize) -> usize {
    match book_index {
        0 => 3,
        1 => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_is_self_consistent() {
        assert_eq!(BOOKS.len(), 3);
        assert_eq!(SOURCES.len(), 3);
        for (i, b) in BOOKS.iter().enumerate() {
            assert!(!b.name.is_empty() && !b.author.is_empty());
            assert!(!b.chapters.is_empty(), "{} 没有章节", b.name);
            assert!(
                SOURCES.contains(&b.source),
                "{} 的源不在 SOURCES 里",
                b.name
            );
            for c in 1..=b.chapters.len() {
                let (title, body) = chapter(i, c).expect("章节应在范围内");
                assert!(!title.is_empty());
                assert!(!body.is_empty(), "{title} 没有正文");
            }
            assert!(chapter(i, 0).is_none());
            assert!(chapter(i, b.chapters.len() + 1).is_none());
            assert_eq!(chapter_count(i), b.chapters.len());
        }
        assert!(book(BOOKS.len()).is_none());
    }

    #[test]
    fn search_matches_name_author_and_reports_origins() {
        assert_eq!(search("夜色").len(), 1);
        assert_eq!(search("夜色")[0].book, 0);
        // 「多源都有」要能被看见：第 0 本 3 个源，第 1 本 2 个
        assert_eq!(search("夜色")[0].origins, 3);
        assert_eq!(search("长夜")[0].origins, 2);
        assert_eq!(search("雨落")[0].origins, 1);
        // 按作者也能搜到
        assert_eq!(search("第三作者")[0].book, 2);
        // 空查询不返回任何东西（而不是返回全部）
        assert!(search("").is_empty());
        assert!(search("   ").is_empty());
        assert!(search("不存在的书名").is_empty());
    }
}
