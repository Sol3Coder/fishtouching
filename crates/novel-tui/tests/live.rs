//! 真打站点的端到端：搜索 → 目录 → 正文 → 落在阅读页上。
//!
//! 默认不跑（`cargo test -- --ignored`），因为它会因站点状态而变红 ——
//! 那不代表我们的代码坏了，只代表某个书源过期了。
//!
//! 这一条覆盖的是**接缝**：`Online::run` 的返回值经 `App::on_done` 之后
//! 真的变成了阅读页上的注释。单元测试用的是编好的 `Done`，这里用的是
//! 真站点抓来的 HTML。

use novel_core::online::{Done, Job, Online};
use novel_core::source::BookSource;
use novel_core::state::Session;
use novel_tui::app::App;

fn tmp(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("novel-live-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
#[ignore = "需要网络"]
fn search_read_and_render_a_real_chapter() {
    let all = BookSource::parse_file(include_str!(
        "../../novel-core/tests/fixtures/sources-runnable.json"
    ))
    .expect("可跑书源清单");
    let mut online = Online::from_sources(all);
    // 前几个源基本都挂了，直接跳到实测能用的那个
    let start = online
        .names()
        .iter()
        .position(|n| n == "鬼吹灯")
        .expect("夹具里应当有鬼吹灯");
    online.sources_rotate(start);
    online.search_sources = 12;

    let dir = tmp("e2e");
    let mut app = App::new(Session::open(
        dir.join("state.json"),
        dir.join("reader.novel"),
    ));
    app.set_online(online);

    // 走一遍界面会走的路径：回车都通过 job + on_done，不直接调引擎。
    // 引擎这一侧单独留一份 —— 真程序里是后台线程持有一份 clone。
    let engine = app.online.clone().unwrap();
    let Done::Search(hits) = engine.run(&Job::Search("斗破苍穹".into()), &mut |_| {}) else {
        panic!("搜索没出结果");
    };
    println!("搜索 {} 条，第一条《{}》", hits.len(), hits[0].name);
    app.on_done(Done::Search(hits));

    app.open_cursor_for_test();
    let job = app.take_job().expect("应当去拉目录");
    let Done::Toc { chapters: toc, .. } = engine.run(&job, &mut |_| {}) else {
        panic!("拉目录失败");
    };
    println!("目录 {} 章", toc.len());
    app.on_done(Done::Toc {
        chapters: toc,
        from_cache: false,
    });

    let job = app.take_job().expect("目录到手后应当去拉第一章");
    let done = engine.run(&job, &mut |_| {});
    app.on_done(done);

    let lines = app.reader_paragraphs().join("\n");
    assert!(lines.contains("第 1 / "), "阅读页没换章：\n{lines}");
    assert!(
        lines.lines().any(|l| l.chars().count() > 10),
        "阅读页上没有正文：\n{lines}"
    );
    println!(
        "阅读页正文首行：{}",
        lines.lines().find(|l| l.chars().count() > 10).unwrap_or("")
    );
}
