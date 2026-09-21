//! 拿真实书源打真实站点的探针。用来验证「引擎 + 某条书源」是不是真的能跑通。
//!
//! ```text
//! cargo run -p novel-core --example probe -- <sources.json> list
//! cargo run -p novel-core --example probe -- <sources.json> search <关键字> [源名片段] [最多打几个源]
//! cargo run -p novel-core --example probe -- <sources.json> book   <书详情页> [源名片段]
//! cargo run -p novel-core --example probe -- <sources.json> toc    <书详情页> [源名片段]
//! cargo run -p novel-core --example probe -- <sources.json> read   <章节页>   [源名片段]
//! ```

use std::process::ExitCode;

use novel_core::crawl::{Crawler, Hit};
use novel_core::net::Fetcher;
use novel_core::source::BookSource;
use novel_core::{Error, R};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.len() < 2 {
        eprintln!("用法见 examples/probe.rs 顶部的注释");
        return ExitCode::from(2);
    }
    let (path, cmd) = (&argv[0], argv[1].as_str());
    let rest = &argv[2..];

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读不了 {path}：{e}");
            return ExitCode::from(2);
        }
    };
    let sources = match BookSource::parse_file(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{path} 不是合法的书源文件：{e}");
            return ExitCode::from(2);
        }
    };
    let usable: Vec<BookSource> = sources
        .iter()
        .filter(|s| s.enabled && s.kind == 0)
        .cloned()
        .collect();
    println!(
        "书源 {} 个（启用的文本源 {} 个）",
        sources.len(),
        usable.len()
    );

    if cmd == "fuzz" {
        // 逐条喂语料，每条先打印再处理 —— 崩了就能从最后一行看出是哪条规则
        let corpus = std::fs::read_to_string(&argv[2]).expect("语料文件");
        let from: usize = argv.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
        let doc = novel_core::html::Html::parse(
            "<html><body><div class='a'><p id='b'>正文<span>子</span></p></div></body></html>",
        );
        for (i, line) in corpus.lines().enumerate() {
            if i < from || line.trim().is_empty() {
                continue;
            }
            println!("[{i}] {line}");
            use std::io::Write;
            std::io::stdout().flush().ok();
            if let Ok(rule) = novel_core::rule::Rule::parse(line, "关键字", 2) {
                let (alts, _) = novel_core::rule::split_rule(&rule.text, &["&&", "||", "%%"]);
                for alt in &alts {
                    let alt = alt.trim().trim_start_matches(['@', ' ']);
                    if alt.is_empty() {
                        continue;
                    }
                    if rule.css {
                        let (sel, acc) = match alt.rfind('@') {
                            Some(k) => (&alt[..k], &alt[k + 1..]),
                            None => (alt, "text"),
                        };
                        if let Ok(els) = doc.select(None, sel) {
                            let ids: Vec<Option<usize>> = els.into_iter().map(Some).collect();
                            let _ = doc.accessor(&ids, acc);
                        }
                    } else {
                        let (steps, _) = novel_core::rule::split_rule(alt, &["@"]);
                        let (last, rest) = steps.split_last().expect("至少一段");
                        let mut cur: Vec<Option<usize>> = vec![None];
                        for st in rest {
                            match doc.step(&cur, st) {
                                Ok(v) => cur = v,
                                Err(_) => break,
                            }
                        }
                        let _ = doc.accessor(&cur, last.trim());
                        let _ = rule.apply_replace("样例文本 123");
                    }
                }
            }
        }
        return ExitCode::SUCCESS;
    }

    if cmd == "list" {
        for s in &usable {
            println!(
                "  {:26} {}{}",
                trim(&s.name, 26),
                s.url,
                if s.can_search() {
                    ""
                } else {
                    "  （不能搜索）"
                }
            );
        }
        return ExitCode::SUCCESS;
    }

    let out = match cmd {
        "search" => {
            let key = rest.first().cloned().unwrap_or_default();
            let filter = rest.get(1).cloned().unwrap_or_default();
            let limit: usize = rest.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
            if key.is_empty() {
                eprintln!("search 后面要给关键字");
                return ExitCode::from(2);
            }
            search(&usable, &key, &filter, limit)
        }
        "book" => on_url(&usable, rest, |c, url| {
            let i = c.info(url)?;
            println!("  书名 {}", i.name);
            println!("  作者 {}", i.author);
            println!("  简介 {}", trim(&i.intro, 90));
            println!(
                "  目录 {}",
                if i.toc_url.is_empty() {
                    url
                } else {
                    &i.toc_url
                }
            );
            Ok(())
        }),
        "toc" => on_url(&usable, rest, |c, url| {
            let toc = c.toc(url)?;
            println!("  {} 章", toc.len());
            for ch in toc.iter().take(3) {
                println!("      {}  {}", trim(&ch.title, 30), ch.url);
            }
            Ok(())
        }),
        // 一次跑通「搜索 → 目录 → 正文」，用来看某批书源到底有几个真能用
        "smoke" => {
            let key = rest.first().cloned().unwrap_or_else(|| "斗破苍穹".into());
            let limit: usize = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(20);
            let skip: usize = rest.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            smoke(&usable, &key, limit, skip)
        }
        "read" => on_url(&usable, rest, |c, url| {
            let t = c.content(url)?;
            println!("  {} 字", t.chars().count());
            for line in t.lines().take(4) {
                println!("      {}", trim(line, 72));
            }
            Ok(())
        }),
        other => {
            eprintln!("不认识的命令 {other}");
            return ExitCode::from(2);
        }
    };
    match out {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("失败：{e}");
            ExitCode::FAILURE
        }
    }
}

fn search(sources: &[BookSource], key: &str, filter: &str, limit: usize) -> R<()> {
    let candidates: Vec<&BookSource> = sources
        .iter()
        .filter(|s| s.can_search() && (filter.is_empty() || s.name.contains(filter)))
        .collect();
    println!("候选 {} 个，最多试 {limit} 个", candidates.len());
    let (mut ok, mut blocked, mut failed) = (0, 0, 0);
    for s in candidates.iter().take(limit) {
        match crawler(s)
            .and_then(|c| c.search(key, 1))
            .map_err(|e| (s, e))
        {
            Ok(hits) => {
                ok += 1;
                show_hits(&s.name, &hits);
            }
            Err((s, Error::Unsupported(what))) => {
                blocked += 1;
                println!("  ⊘ {}：需要 JS —— {what}", s.name);
            }
            Err((s, e)) => {
                failed += 1;
                println!("  ✗ {}：{e}", s.name);
            }
        }
    }
    println!("—— 成功 {ok} / 因 JS 跳过 {blocked} / 失败 {failed}");
    Ok(())
}

/// 对每个源走一遍完整链路，报出卡在哪一步。这是唯一能回答
/// 「这套实现到底能读几本书」的办法 —— 单看规则能不能解析没用，
/// 站点挂了、书源过期了、正文规则取到的是广告，都会在这里现形。
fn smoke(sources: &[BookSource], key: &str, limit: usize, skip: usize) -> R<()> {
    let mut full = 0;
    let mut stats: Vec<(String, String)> = Vec::new();
    for s in sources
        .iter()
        .filter(|s| s.can_search())
        .skip(skip)
        .take(limit)
    {
        let c = match crawler(s) {
            Ok(c) => c,
            Err(e) => {
                stats.push((s.name.clone(), format!("构造失败 {e}")));
                continue;
            }
        };
        let step = (|| -> R<(usize, String)> {
            let hits = c.search(key, 1)?;
            if hits.is_empty() {
                return Err(Error::Missing("搜索结果".into()));
            }
            let toc = c.toc(&hits[0].url)?;
            if toc.is_empty() {
                return Err(Error::Missing("目录".into()));
            }
            let text = c.content(&toc[0].url)?;
            if text.chars().count() < 200 {
                return Err(Error::Missing(format!(
                    "正文（只有 {} 字）",
                    text.chars().count()
                )));
            }
            Ok((toc.len(), text))
        })();
        match step {
            Ok((n, text)) => {
                full += 1;
                stats.push((
                    s.name.clone(),
                    format!("✅ {n} 章 / 正文 {} 字", text.chars().count()),
                ));
                println!("── {} ──", s.name);
                for line in text.lines().take(3) {
                    println!("   {}", trim(line, 74));
                }
            }
            Err(Error::Unsupported(w)) => {
                stats.push((s.name.clone(), format!("⊘ 需要 JS：{}", trim(&w, 40))))
            }
            Err(e) => stats.push((s.name.clone(), format!("✗ {e}"))),
        }
    }
    println!();
    for (name, what) in &stats {
        println!("  {:24} {}", trim(name, 24), what);
    }
    println!("—— {limit} 个源里，{} 个走通了全链路", full);
    Ok(())
}

fn show_hits(name: &str, hits: &[Hit]) {
    println!("  ✓ {name}：{} 条", hits.len());
    for h in hits.iter().take(3) {
        println!(
            "      《{}》 {}  {}",
            trim(&h.name, 24),
            trim(&h.author, 12),
            trim(&h.url, 56)
        );
    }
}

fn on_url(
    sources: &[BookSource],
    rest: &[String],
    f: impl FnOnce(&Crawler<Fetcher>, &str) -> R<()>,
) -> R<()> {
    let url = rest.first().cloned().unwrap_or_default();
    if url.is_empty() {
        return Err(Error::Missing("URL 参数".into()));
    }
    let filter = rest.get(1).cloned().unwrap_or_default();
    let source = pick(sources, &url, &filter)?;
    println!("  用源：{}", source.name);
    f(&crawler(&source)?, &url)
}

fn crawler(s: &BookSource) -> R<Crawler<Fetcher>> {
    Ok(Crawler::new(s.clone(), Fetcher::new(s.headers())))
}

fn pick(sources: &[BookSource], url: &str, filter: &str) -> R<BookSource> {
    let host = url
        .split("//")
        .nth(1)
        .map(|r| r.split('/').next().unwrap_or(""))
        .unwrap_or("");
    if !filter.is_empty() {
        if let Some(s) = sources.iter().find(|s| s.name.contains(filter)) {
            return Ok(s.clone());
        }
    }
    // 域名能对上就用它 —— 这是唯一能保证规则和页面结构匹配的判据
    if let Some(s) = sources
        .iter()
        .find(|s| !host.is_empty() && s.url.contains(host))
    {
        return Ok(s.clone());
    }
    Err(Error::Missing(format!("没有书源的域名匹配 {host}")))
}

fn trim(s: &str, n: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        return flat;
    }
    flat.chars().take(n).collect::<String>() + "…"
}
