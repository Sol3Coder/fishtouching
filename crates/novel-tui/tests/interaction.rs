//! 自测交互：用**真实的事件循环**跑完整条链，断言屏幕上出现了什么。
//!
//! 和 `app.rs` 里的单元测试不同，这里跑的是 `run::run` 本体 ——
//! 「按键 → 排后台任务 → 收结果 → 重画 → 再按键」整条链，
//! 包括 `Workers::pump`、进度消息和 `Done` 的分发。
//!
//! 后台是**假的**：`Workers::for_test()` 把任务丢掉，`Driver` 在脚本的
//! 指定位置把 `Done::Search` / `Done::Toc` / `Done::Read` 塞回来。
//! 所以这一套完全离线、确定性、随时能跑。
//!
//! 交互模型是**一屏一屏地走**：书架 →（Enter）→ 目录 →（Enter）→ 阅读，
//! Esc 逐层退回；搜索和书源是另外两屏，从书架进。

use std::fs;

use novel_core::crawl::{Chapter, Hit};
use novel_core::online::{Done, Online};
use novel_core::source::BookSource;
use novel_core::state::Session;
use novel_tui::app::{App, Key, ReaderMode, Screen, SearchFocus};
use novel_tui::run::{parse_script, parse_steps, Driver, Step, Workers};
use novel_tui::ui;

// ── 跑测试的脚手架 ──────────────────────────────────────────────────────

fn tmp(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("novel-interact-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn app_in(dir: &std::path::Path, with_sources: bool) -> App {
    let mut app = App::new(Session::open(
        dir.join("state.json"),
        dir.join("reader.novel"),
    ));
    if with_sources {
        let all =
            BookSource::parse_file(include_str!("../../novel-core/tests/fixtures/sources.json"))
                .unwrap();
        app.set_online(Online::from_sources(all));
    }
    app
}

/// 一帧一帧地跑，把所有画面收起来。
fn drive_in(app: &mut App, steps: Vec<Step>) -> Vec<String> {
    let (workers, tx) = Workers::for_test();
    let mut driver = Driver::new(steps, tx);
    let mut frames = Vec::new();
    novel_tui::run::run(
        app,
        &workers,
        |a| {
            frames.push(ui::render_to_text(a, 110, 32));
            Ok(())
        },
        |a| driver.next(a),
    )
    .expect("事件循环不该出错");
    frames
}

/// 起一个 app，跑一段脚本（每个字符一个键），返回（app, 所有帧）。
fn run_script(tag: &str, keys: &str, with_sources: bool) -> (App, Vec<String>) {
    let dir = tmp(tag);
    let mut app = app_in(&dir, with_sources);
    let steps: Vec<Step> = parse_script(keys).into_iter().map(Step::Key).collect();
    let frames = drive_in(&mut app, steps);
    (app, frames)
}

fn last_frame(tag: &str, keys: &str) -> String {
    let (_, frames) = run_script(tag, keys, false);
    frames.last().cloned().unwrap_or_default()
}

fn hits() -> Vec<Hit> {
    vec![
        Hit {
            name: "斗破苍穹".into(),
            author: "天蚕土豆".into(),
            url: "http://a.com/1".into(),
            ..Default::default()
        },
        Hit {
            name: "武动乾坤".into(),
            author: "天蚕土豆".into(),
            url: "http://a.com/2".into(),
            ..Default::default()
        },
    ]
}

fn toc() -> Vec<Chapter> {
    vec![
        Chapter {
            title: "第一章 陨落的天才".into(),
            url: "http://a.com/1/1".into(),
        },
        Chapter {
            title: "第二章 斗气大陆".into(),
            url: "http://a.com/1/2".into(),
        },
        Chapter {
            title: "第三章 客人".into(),
            url: "http://a.com/1/3".into(),
        },
    ]
}

fn read(title: &str, body: &str) -> Done {
    Done::Read {
        title: title.into(),
        text: body.into(),
    }
}

/// 「搜索屏 + 结果已到 + 键盘在结果列表上」——很多测试的起点。
fn search_with_results(tag: &str) -> (App, std::path::PathBuf) {
    let dir = tmp(tag);
    let mut app = app_in(&dir, true);
    app.go(Screen::Search);
    app.on_done(Done::Search(hits()));
    assert_eq!(app.search_focus, SearchFocus::Results);
    (app, dir)
}

// ── 屏与导航 ────────────────────────────────────────────────────────────

#[test]
fn quitting_ends_the_loop() {
    let (app, frames) = run_script("quit", "q", false);
    assert!(app.quit);
    assert!(!frames.is_empty(), "至少画过一帧");
}

#[test]
fn the_shelf_is_the_entry_screen() {
    let (app, frames) = run_script("entry", "", false);
    assert_eq!(app.screen, Screen::Shelf);
    let out = frames.last().unwrap();
    assert!(out.contains("书架"), "{out}");
    // 一屏一屏：书架屏里不该有别的屏的内容
    assert!(!out.contains("目录 ·"), "{out}");
    assert!(!out.contains("关键字"), "{out}");
}

#[test]
fn enter_walks_into_the_toc_and_esc_walks_back_out() {
    // 这是这次交互重做的核心路径：书架 → 目录 → 阅读，Esc 逐层退回
    let dir = tmp("walk");
    let mut app = app_in(&dir, false);
    let mut frames = drive_in(
        &mut app,
        parse_script("{enter}").into_iter().map(Step::Key).collect(),
    );
    assert_eq!(app.screen, Screen::Toc, "回车应当进目录屏");
    let toc_frame = frames.remove(frames.len() - 1);
    assert!(toc_frame.contains("目录 ·"), "{toc_frame}");
    assert!(
        !toc_frame.contains("示例作者  3 章"),
        "书架的清单还在：\n{toc_frame}"
    );

    let mut frames = drive_in(
        &mut app,
        parse_script("{enter}").into_iter().map(Step::Key).collect(),
    );
    assert_eq!(app.screen, Screen::Reader, "再回车应当进阅读屏");
    let read_frame = frames.remove(frames.len() - 1);
    // 标签栏里会出现「目录」二字，所以断言的是**目录那份清单**没被画出来
    assert!(
        !read_frame.contains("1. 推开门"),
        "阅读屏里不该有目录清单：\n{read_frame}"
    );
    // 标签栏要标出当前在哪一屏（反显在纯文本里看不出来，所以看右边的补充信息）
    assert!(
        read_frame.contains("第 1 / 3 章"),
        "标签栏要指出当前屏：\n{read_frame}"
    );

    // 平级之后 Esc 是「回书架」，不是「退一层」
    drive_in(
        &mut app,
        parse_script("{esc}").into_iter().map(Step::Key).collect(),
    );
    assert_eq!(app.screen, Screen::Shelf, "Esc 回书架");
}

/// 五屏平级：数字键直接跳，不留历史。
#[test]
fn the_screens_are_peers_not_a_stack() {
    let (app, frames) = run_script("flat-screens", "{3}", false);
    assert_eq!(app.screen, Screen::Toc, "按 3 直接进目录屏");

    // 从目录按 4 进阅读，再按 3 回目录 —— 不叠任何东西
    let (app, _) = run_script("flat-screens2", "{enter}{4}{3}", false);
    assert_eq!(app.screen, Screen::Toc);
    // Esc 一律回书架（层级版里会退回「进来的那一屏」）
    let (app, _) = run_script("flat-screens3", "{3}{esc}", false);
    assert_eq!(app.screen, Screen::Shelf);

    // 标签栏五屏都在，而且把数字键写在名字前面
    let frame = frames.last().unwrap();
    for t in ["1 书架", "2 搜索", "3 目录", "4 阅读", "5 书源"] {
        assert!(frame.contains(t), "标签栏里该有 {t}：\n{frame}");
    }
}

/// `q` 全局退出：哪一屏都行（搜索框里除外，那里它是文本）。
///
/// **不能拿 `app.quit` 当证据** —— 驱动台收尾时自己就会设 `app.quit`，
/// 所以脚本跑完它永远是 true。改成在 `q` 后面跟一个 `{esc}`：
/// 真退出了的话循环当场结束、后面的键根本不会执行，屏幕就停在原地；
/// 没退出的话 `{esc}` 会把人送回书架。两条路的结果不一样，才验得出来。
#[test]
fn q_quits_from_any_screen_through_the_real_loop() {
    for keys in ["{3}q{esc}", "{4}q{esc}", "{5}q{esc}"] {
        let (app, _) = run_script("quit-anywhere", keys, false);
        assert_ne!(
            app.screen,
            Screen::Shelf,
            "`q` 该当场退出，后面的 `{{esc}}` 不该跑到（{keys}）"
        );
    }
    // 搜索框里 `q` 是文本 → 没退出 → 后面的 `{esc}` 跑了 → 回书架
    let (app, _) = run_script("quit-typing", "{2}q{esc}", false);
    assert_eq!(app.screen, Screen::Shelf, "在搜索框里 q 是文本，没退出");
}

#[test]
fn help_shows_then_any_key_closes_it() {
    let with_help = last_frame("help1", "?");
    assert!(with_help.contains("帮助"), "{with_help}");
    assert!(with_help.contains("一屏一屏地走"), "{with_help}");

    let (app, frames) = run_script("help2", "?x", false);
    assert!(!app.help);
    assert!(!frames.last().unwrap().contains("一屏一屏地走"));
}

#[test]
fn cap_s_opens_the_sources_screen_and_esc_returns() {
    let (app, frames) = run_script("tosources", "S", true);
    assert_eq!(app.screen, Screen::Sources);
    let out = frames.last().unwrap();
    assert!(out.contains("书源（"), "{out}");
    assert!(!out.contains("书架 ─"), "书源屏里不该有书架面板：\n{out}");
}

#[test]
fn each_screen_has_its_own_key_hint_line() {
    let shelf = last_frame("hint-shelf", "");
    assert!(shelf.contains("移出书架"), "{shelf}");
    assert!(!shelf.contains("上下章"), "书架屏不该提示翻章：\n{shelf}");

    let reader = last_frame("hint-reader", "{enter}{enter}");
    assert!(reader.contains("上下章"), "{reader}");

    let src = last_frame("hint-src", "S");
    assert!(src.contains("导入"), "{src}");
    assert!(src.contains("删除"), "{src}");
}

// ── 翻章快捷键 ──────────────────────────────────────────────────────────

#[test]
fn every_chapter_shortcut_works_in_the_reader_screen() {
    // 演示书《夜色渐浓》3 章。`n`/`p` 只在**阅读屏**是翻章 ——
    // 在目录屏里它们是「列表中下一个/上一个」，所以脚本要先 {enter}{enter}。
    let cases: Vec<(&str, usize)> = vec![
        ("n", 2),
        ("nn", 3),
        ("np", 1),
        ("nnn", 3),
        ("nppp", 1),
        ("npn", 2),
    ];
    for (tail, want) in cases {
        let script = format!("{{enter}}{{enter}}{tail}");
        let (app, _) = run_script(&format!("chap-{tail}"), &script, false);
        assert_eq!(
            app.state().chapter,
            want,
            "脚本 {script:?} 之后应当停在第 {want} 章，实际 {}",
            app.state().chapter
        );
    }
}

#[test]
fn chapter_shortcuts_are_bounded_at_both_ends() {
    let (app, _) = run_script("bound-low", "{enter}{enter}ppppp", false);
    assert_eq!(app.state().chapter, 1);
    assert!(app.status.contains("第一章"), "{}", app.status);

    let (app, _) = run_script("bound-high", "{enter}{enter}nnnnnnnnn", false);
    assert_eq!(app.state().chapter, app.toc_len());
    assert!(app.status.contains("最后一章"), "{}", app.status);
}

#[test]
fn goto_prompt_accepts_a_number_and_rejects_junk() {
    let (app, _) = run_script("goto-ok", "{enter}{enter}g2\n", false);
    assert_eq!(app.state().chapter, 2);

    let (app, _) = run_script("goto-bad", "{enter}{enter}gabc\n", false);
    assert_eq!(app.state().chapter, 1, "烂输入不该跳走");
    assert!(app.status.contains("不是章号"), "{}", app.status);

    let (app, _) = run_script("goto-oob", "{enter}{enter}g99\n", false);
    assert_eq!(app.state().chapter, 1);
    assert!(app.status.contains("没有第 99 章"), "{}", app.status);
}

#[test]
fn the_toc_cursor_and_the_chapter_stay_in_step() {
    // 目录屏里 Enter 之后应当进阅读屏，并且读的就是光标那一章
    let dir = tmp("tocstep");
    let mut app = app_in(&dir, false);
    drive_in(
        &mut app,
        parse_script("{enter}").into_iter().map(Step::Key).collect(),
    );
    assert_eq!(app.screen, Screen::Toc);
    drive_in(
        &mut app,
        parse_script("{down}{down}{enter}")
            .into_iter()
            .map(Step::Key)
            .collect(),
    );
    assert_eq!(app.state().chapter, 3);
    assert_eq!(app.screen, Screen::Reader);
}

// ── 搜索屏 ──────────────────────────────────────────────────────────────

#[test]
fn the_search_screen_types_a_keyword_and_runs_a_real_search() {
    let dir = tmp("search-type");
    let mut app = app_in(&dir, true);
    let mut steps: Vec<Step> = parse_script("s斗破苍穹\n")
        .into_iter()
        .map(Step::Key)
        .collect();
    // 后台把结果交回来
    steps.push(Step::Feed(Done::Search(hits())));
    let frames = drive_in(&mut app, steps);

    assert_eq!(app.screen, Screen::Search);
    assert_eq!(app.state().query, "斗破苍穹", "打进去的字要留在关键字框里");
    assert_eq!(
        app.search_focus,
        SearchFocus::Results,
        "结果到了键盘就该去列表"
    );
    let all = frames.join("\n---\n");
    assert!(all.contains("斗破苍穹"), "{all}");
    assert!(all.contains("武动乾坤"), "{all}");
}

#[test]
fn tab_switches_between_the_keyword_box_and_the_results() {
    let (mut app, _) = search_with_results("search-tab");
    // 结果到手时已经在 Results 上
    assert_eq!(app.search_focus, SearchFocus::Results);
    app.handle(Key::Tab);
    assert_eq!(app.search_focus, SearchFocus::Input, "Tab 回关键字框");
    app.handle(Key::Tab);
    assert_eq!(app.search_focus, SearchFocus::Results, "再 Tab 回结果");
}

#[test]
fn typing_goes_to_the_keyword_box_without_hijacking_the_results() {
    let (mut app, _) = search_with_results("search-typing");
    app.handle(Key::Tab); // 回关键字框
    assert_eq!(app.search_focus, SearchFocus::Input);
    // 这时 `n`/`p` 应当是打字，不是翻章
    for c in "np".chars() {
        app.handle(Key::Char(c));
    }
    assert_eq!(app.state().query, "np");
    assert_eq!(app.state().chapter, 1, "关键字框里打字不该翻章");
}

#[test]
fn escape_goes_from_search_back_to_the_shelf_and_drops_the_results() {
    let (mut app, dir) = search_with_results("search-esc");
    let frames = drive_in(
        &mut app,
        parse_script("{esc}").into_iter().map(Step::Key).collect(),
    );
    assert_eq!(app.screen, Screen::Shelf);
    assert!(app.web_hits.is_none(), "搜索结果是一次性的，退出去就丢掉");
    let last = frames.last().unwrap();
    assert!(!last.contains("武动乾坤"), "{last}");
    let _ = dir;
}

#[test]
fn b_on_a_search_result_adds_it_to_the_shelf() {
    let (mut app, dir) = search_with_results("search-bookmark");
    let frames = drive_in(
        &mut app,
        parse_script("b").into_iter().map(Step::Key).collect(),
    );
    assert_eq!(app.shelf.len(), 1, "{}", app.status);
    assert!(app.shelf.contains("http://a.com/1"));
    assert!(
        frames.last().unwrap().contains("加入书架"),
        "{:?}",
        frames.last()
    );

    // 要落盘
    let on_disk = fs::read_to_string(dir.join("shelf.json")).unwrap();
    assert!(on_disk.contains("斗破苍穹"), "{on_disk}");

    // 再加一次不该变两条
    drive_in(
        &mut app,
        parse_script("b").into_iter().map(Step::Key).collect(),
    );
    assert_eq!(app.shelf.len(), 1);
    assert!(app.status.contains("已经在书架上"), "{}", app.status);
}

#[test]
fn offline_search_filters_the_built_in_books() {
    // 没有书源文件时搜索屏仍然有用：筛内置演示书
    let (mut app, frames) = run_script("offline-search", "s夜\n", false);
    assert!(app.take_job_check_none(), "没有书源就不该排任务");
    assert_eq!(app.rows().len(), 2, "「夜」命中《夜色渐浓》和《长夜将尽》");
    let all = frames.join("\n");
    assert!(all.contains("夜色渐浓"), "{all}");
}

// ── 在线全链路 ──────────────────────────────────────────────────────────

#[test]
fn the_whole_online_chain_runs_through_the_real_loop() {
    let (mut app, _) = search_with_results("chain");
    let mut steps: Vec<Step> = parse_script("{enter}").into_iter().map(Step::Key).collect();
    steps.push(Step::Feed(Done::Toc {
        chapters: toc(),
        from_cache: false,
    }));
    steps.push(Step::Feed(read(
        "第一章 陨落的天才",
        "“斗之力，三段！”\n望着测验魔石碑。",
    )));
    // 进阅读屏（目录到手后已经在目录屏），翻一章，再送正文
    steps.extend(parse_script("{enter}n").into_iter().map(Step::Key));
    steps.push(Step::Feed(read(
        "第二章 斗气大陆",
        "这里是斗气大陆。\n没有魔法。",
    )));
    let frames = drive_in(&mut app, steps);

    let all = frames.join("\n---帧---\n");
    // 目录屏列出抓来的章节
    assert!(all.contains("第一章 陨落的天才"), "{all}");
    assert!(all.contains("第二章 斗气大陆"), "{all}");
    // 阅读屏显示正文（纯文本，和 reader.novel 里那份同源）
    assert!(all.contains("斗之力，三段！"), "{all}");
    assert!(all.contains("这里是斗气大陆。"), "{all}");
    assert_eq!(app.state().chapter, 2);
    assert_eq!(app.screen, Screen::Reader);
    let last = frames.last().unwrap();
    assert!(last.contains("第 2 / 3 章"), "{last}");
    // 阅读屏里不该还画着目录的清单
    assert!(!last.contains("第三章 客人"), "{last}");
}

#[test]
fn opening_a_book_from_search_puts_it_on_the_shelf_automatically() {
    let (mut app, dir) = search_with_results("auto-shelf");
    let mut steps: Vec<Step> = parse_script("{enter}").into_iter().map(Step::Key).collect();
    steps.push(Step::Feed(Done::Toc {
        chapters: toc(),
        from_cache: false,
    }));
    steps.push(Step::Feed(read("第一章", "正文")));
    drive_in(&mut app, steps);

    assert_eq!(app.shelf.len(), 1, "打开过的书应当自动进书架");
    let b = app.shelf.get("http://a.com/1").unwrap();
    assert_eq!(b.name, "斗破苍穹");
    assert_eq!(b.chapters, 3);
    // 书源名不能记成书名（否则下次按源名反查就找不到）
    assert_eq!(b.source, app.source_names()[app.toc_source].clone());
    let on_disk = fs::read_to_string(dir.join("shelf.json")).unwrap();
    assert!(on_disk.contains("斗破苍穹"), "{on_disk}");
}

#[test]
fn reading_progress_is_remembered_and_restored_across_restarts() {
    let dir = tmp("memory");
    {
        let mut app = app_in(&dir, true);
        app.go(Screen::Search);
        app.on_done(Done::Search(hits()));
        let mut steps: Vec<Step> = parse_script("{enter}").into_iter().map(Step::Key).collect();
        steps.push(Step::Feed(Done::Toc {
            chapters: toc(),
            from_cache: false,
        }));
        steps.push(Step::Feed(read("第一章", "正文")));
        steps.extend(parse_script("{enter}nn").into_iter().map(Step::Key));
        steps.push(Step::Feed(read("第三章 客人", "第三章正文")));
        drive_in(&mut app, steps);
        assert_eq!(app.state().chapter, 3);
        assert_eq!(app.shelf.get("http://a.com/1").unwrap().chapter, 3);
    }

    // 重开一个 app（模拟重启）：什么键都不按，书架还在
    let mut fresh = app_in(&dir, true);
    assert_eq!(fresh.shelf.len(), 1, "重启后书架还在");
    assert_eq!(fresh.shelf.get("http://a.com/1").unwrap().chapter, 3);

    // 从书架打开：应当跳回第 3 章
    let mut steps: Vec<Step> = parse_script("{enter}").into_iter().map(Step::Key).collect();
    steps.push(Step::Feed(Done::Toc {
        chapters: toc(),
        from_cache: false,
    }));
    steps.push(Step::Feed(read("第三章 客人", "第三章正文")));
    let frames = drive_in(&mut fresh, steps);
    assert_eq!(fresh.state().chapter, 3, "重开应当接着上次那一章");
    assert!(fresh.status.contains("接着读"), "{}", fresh.status);
    assert!(
        frames.last().unwrap().contains("第 3 / 3 章"),
        "{}",
        frames.last().unwrap()
    );
}

#[test]
fn a_failed_background_job_shows_up_and_does_not_hang() {
    let dir = tmp("feed-fail");
    let mut app = app_in(&dir, true);
    let mut steps: Vec<Step> = parse_script("s斗破\n").into_iter().map(Step::Key).collect();
    steps.push(Step::Feed(Done::Failed(
        "试过的书源都需要 JS 解释器".into(),
    )));
    drive_in(&mut app, steps);
    assert!(app.busy.is_none(), "失败之后不能再显示「在加载」");
    assert!(app.status.contains("JS"), "{}", app.status);
}

#[test]
fn switching_books_with_braces_walks_the_shelf() {
    let dir = tmp("switchbook");
    let mut app = app_in(&dir, true);
    let src = app.source_names()[0].clone();
    app.shelf.add(novel_core::shelf::ShelfBook {
        source: src.clone(),
        name: "A 书".into(),
        url: "http://a.com/A".into(),
        chapter: 5,
        read_at: 200,
        ..Default::default()
    });
    app.shelf.add(novel_core::shelf::ShelfBook {
        source: src,
        name: "B 书".into(),
        url: "http://a.com/B".into(),
        chapter: 1,
        read_at: 100,
        ..Default::default()
    });

    let frames = drive_in(
        &mut app,
        parse_script("}").into_iter().map(Step::Key).collect(),
    );
    // `}` 打开最近在读的下一本：A（read_at 更大）在前，所以下一本是 B
    assert_eq!(app.book_name, "B 书", "实际 {}", app.book_name);
    // 任务已经被驱动台取走了（后台是假的），所以看状态栏和书名
    assert!(
        app.status.contains("加载目录") || app.screen == Screen::Toc,
        "应当去拉目录，实际 status={:?} screen={:?}",
        app.status,
        app.screen
    );
    let _ = frames;
}

// ── 书源屏 ──────────────────────────────────────────────────────────────

#[test]
fn source_panel_toggles_imports_edits_and_deletes() {
    let dir = tmp("srcedit");
    let src_file = dir.join("sources.json");
    fs::write(
        &src_file,
        include_str!("../../novel-core/tests/fixtures/sources.json"),
    )
    .unwrap();

    let all = BookSource::parse_file(&fs::read_to_string(&src_file).unwrap()).unwrap();
    let mut online = Online::from_sources(all);
    online.path = Some(src_file.clone());

    let mut app = app_in(&dir, false);
    app.set_online(online);
    app.go(Screen::Sources);
    app.source_cursor = 0;

    let before = app.source_len();
    // 关掉第一个源（空格 = 开关）
    drive_in(
        &mut app,
        parse_script(" ").into_iter().map(Step::Key).collect(),
    );
    assert!(!app.source_on(0));
    assert_eq!(app.sources_on(), before - 1);

    // 改名字 + 改网址：Enter 进表单，一次会话里改完两个字段
    //（老的 e/u/s 三条快捷键已经并进这一个表单）
    drive_in(
        &mut app,
        parse_script("{enter}{ctrl-u}改过的源{tab}{ctrl-u}http://new.example/{enter}")
            .into_iter()
            .map(Step::Key)
            .collect(),
    );
    assert!(app.status.contains("已保存"), "{}", app.status);
    assert_eq!(app.source_names()[0], "改过的源", "{}", app.status);
    assert_eq!(
        app.online.as_ref().unwrap().field(0, "url"),
        "http://new.example/"
    );
    assert!(
        fs::read_to_string(&src_file).unwrap().contains("改过的源"),
        "改完要落盘"
    );

    // 导入一个新的
    let extra = dir.join("extra.json");
    fs::write(
        &extra,
        r#"[{"bookSourceName":"导进来的","bookSourceUrl":"http://zzz.example/",
             "searchUrl":"http://zzz.example/s?q={{key}}"}]"#,
    )
    .unwrap();
    drive_in(
        &mut app,
        parse_script(&format!("a{}\n", extra.display()))
            .into_iter()
            .map(Step::Key)
            .collect(),
    );
    assert_eq!(app.source_len(), before + 1);
    assert!(app.source_names().contains(&"导进来的".to_string()));

    // 删掉第一个
    app.source_cursor = 0;
    drive_in(
        &mut app,
        parse_script("d").into_iter().map(Step::Key).collect(),
    );
    assert_eq!(app.source_len(), before);
    assert!(!app.source_names().contains(&"改过的源".to_string()));
    assert!(app.status.contains("已删除书源"), "{}", app.status);
}

#[test]
fn the_sources_screen_says_when_it_is_empty() {
    let dir = tmp("srcempty");
    let mut app = app_in(&dir, false);
    app.set_online(Online::from_sources(Vec::new()));
    app.go(Screen::Sources);
    let frames = drive_in(
        &mut app,
        parse_script("").into_iter().map(Step::Key).collect(),
    );
    let out = frames.last().unwrap();
    assert!(out.contains("一个书源都没有"), "{out}");
    assert!(out.contains("按 a 导入"), "{out}");
}

// ── 边界 ────────────────────────────────────────────────────────────────

#[test]
fn tiny_terminals_and_resizes_do_not_break_the_loop() {
    let dir = tmp("tiny");
    let mut app = app_in(&dir, false);
    let (workers, tx) = Workers::for_test();
    let mut d = Driver::script("{enter}{enter}nnp", tx);
    let mut w = 4u16;
    novel_tui::run::run(
        &mut app,
        &workers,
        |a| {
            w = w.saturating_add(9).max(4);
            let text = ui::render_to_text(a, w.min(120), 3 + (w % 28));
            let _ = text;
            Ok(())
        },
        |a| d.next(a),
    )
    .unwrap();
    assert_eq!(app.state().chapter, 2);
}

#[test]
fn the_shelf_shows_a_hint_when_it_is_empty_but_sources_exist() {
    let dir = tmp("emptyshelf");
    let mut app = app_in(&dir, true);
    let frames = drive_in(
        &mut app,
        parse_script("").into_iter().map(Step::Key).collect(),
    );
    let out = frames.last().unwrap();
    assert!(out.contains("书架是空的"), "{out}");
    assert!(out.contains("按 / 搜索") || out.contains("按 /"), "{out}");
    // 有书源、书架空着时**不**塞演示书
    assert!(!out.contains("夜色渐浓"), "{out}");
}

/// 隐蔽模式下闲置够久，注释里的正文自动藏起来；按一下键又回来。
///
/// 这条走**真实事件循环 + 真实时钟**（`{wait:}` 真的 sleep），
/// 所以验的是「事件循环确实把时钟喂进去了」这件事 —— 光测 `prose_visible`
/// 是测不到接线对不对的。
#[test]
fn stealth_mode_hides_the_prose_after_a_real_idle_wait() {
    let dir = tmp("idlehide-live");
    let mut app = app_in(&dir, false);
    // 真等 15 秒太久，把阈值调小 —— 所以阈值做成了字段
    app.idle_hide_ms = 300;

    let steps: Vec<Step> = parse_steps("{enter}{enter}{m}{wait:800}");
    let frames = drive_in(&mut app, steps);

    // 收尾那一帧（等完之后）应当已经藏了
    assert!(!app.prose_visible(), "闲置超过阈值就该藏");
    let hidden = frames.last().expect("应当有帧");
    assert!(
        !hidden.lines().any(|l| l.contains("推开门")),
        "藏起来之后画面上不该还有正文：
{hidden}"
    );
    assert!(
        !hidden.contains("推开门"),
        "文件头里也有章节名 —— 只抹 `//` 是不够的：
{hidden}"
    );
    // 注释标记还在（行数和布局不变），只是内容没了
    assert!(
        hidden.lines().any(|l| l.contains("│/*")),
        "注释标记要留着，否则屏幕会跳：
{hidden}"
    );
    assert!(
        hidden.contains("已隐藏"),
        "标题栏要说清楚现在为什么看不见正文：
{hidden}"
    );

    // 再按一下键（不用等）—— 立刻回来
    let frames = drive_in(
        &mut app,
        parse_script("{j}").into_iter().map(Step::Key).collect(),
    );
    let shown = frames.last().expect("应当有帧");
    assert!(app.prose_visible(), "有输入就该显示");
    assert!(
        shown.lines().any(|l| l.contains("推开门")),
        "按完键正文该回来：
{shown}"
    );
}

/// 隐蔽模式：阅读屏显示生成出来的 TS，正文在 `//` 注释里。
/// 走的是真实事件循环（`run::run`），不是直接调方法。
#[test]
fn the_reader_can_disguise_itself_as_typescript() {
    let (app, frames) = run_script("stealth", "{enter}{enter}{m}", false);

    assert_eq!(app.reader_mode(), ReaderMode::Stealth);
    let frame = frames.last().expect("应当有帧");

    // 内容区是代码
    assert!(
        frame.contains("export") || frame.contains("interface") || frame.contains("function"),
        "隐蔽模式下该看到 TypeScript：\n{frame}"
    );
    // 正文在 `//` 注释里
    assert!(
        frame
            .lines()
            .any(|l| l.contains("//") && l.contains("推开门")),
        "正文该以 `//` 注释的形式出现：\n{frame}"
    );
    // 标题栏要标出来现在是哪个模式，否则用户不知道怎么切回去
    assert!(frame.contains("隐蔽"), "该标出当前模式：\n{frame}");

    // 再按一次切回普通模式
    let (app2, frames2) = run_script("stealth2", "{enter}{enter}{m}{m}", false);
    assert_eq!(app2.reader_mode(), ReaderMode::Normal);
    let frame2 = frames2.last().unwrap();
    assert!(
        !frame2.lines().any(|l| l.contains("export interface")),
        "切回去之后不该还有代码：\n{frame2}"
    );
}
