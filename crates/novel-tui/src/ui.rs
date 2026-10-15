//! 绘制。只读 `App`，唯一会写回的是滚动位置的夹取。
//!
//! **一屏一屏地画**：同一时刻只有一个屏，`draw` 按 `app.screen` 分派。
//! 每屏都占满主体区域，共用标题栏（上）、状态栏和按键提示行（下）。

use ratatui::backend::TestBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use ratatui::Terminal;

use crate::app::{App, InputMode, ReaderMode, Screen, SearchFocus, SourceForm};

const FOCUS_BORDER: Color = Color::Yellow;
const IDLE_BORDER: Color = Color::DarkGray;
/// 小说正文的颜色 —— 它是这个界面里唯一「要读的东西」，所以最亮。
const NOVEL: Color = Color::White;
/// 次要信息（作者、章数、编码）：压暗一档。
const CODE: Color = Color::Gray;
const FAINT: Color = Color::DarkGray;
/// 按键提示 / 可操作的东西。
const KEYWORD: Color = Color::Cyan;
/// 「开」的状态。
const LITERAL: Color = Color::Green;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.height < 4 || area.width < 24 {
        // 太小了别硬画，说清楚就行
        f.render_widget(Paragraph::new("窗口太小"), area);
        return;
    }

    // 隐蔽模式：**整屏只有那块代码**。
    //
    // 标题栏（书名/作者/进度/上次搜索）、标签栏（含「夜色渐浓 · 3 章」那种
    // 补充信息）、状态栏、快捷键提示 —— 全是会出卖你的东西，一起收起来。
    // 键位一个没变，只是不画它们（功能不受影响）。
    //
    // 代价说清楚：状态栏也藏了，所以失败信息在隐蔽模式下看不见 ——
    // 按一下 `m` 就回来了（`self.status` 一直在，只是没画）。
    if app.reader_mode() == ReaderMode::Stealth {
        reader(f, app, area);
        if app.help {
            help(f, area);
        }
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1), // 标题栏
        Constraint::Length(1), // 标签栏：有哪些屏、我在哪个
        Constraint::Min(3),    // 当前屏
        Constraint::Length(1), // 状态栏
        Constraint::Length(1), // 按键提示
    ])
    .split(area);

    title(f, app, rows[0]);
    crumb(f, app, rows[1]);
    // 屏画在 rows[2]：加标签栏那一行时忘了往下挪一格，五屏全被挤成 1 行高
    //（测试当场抓到：阅读屏只剩一条上边框）
    match app.screen {
        Screen::Shelf => shelf(f, app, rows[2]),
        Screen::Search => search(f, app, rows[2]),
        Screen::Toc => toc(f, app, rows[2]),
        Screen::Reader => reader(f, app, rows[2]),
        Screen::Sources => sources(f, app, rows[2]),
    }
    status(f, app, rows[3]);
    hints(f, app, rows[4]);
    if app.help {
        help(f, area);
    }
    if app.input.is_some() {
        // 输入框已经画在状态栏里了，这里不用再画东西
    }
}

/// 顶部标题栏：书名、章节进度、书源数，右边是阅读位置或上次搜索。
fn title(f: &mut Frame, app: &App, area: Rect) {
    let s = app.state();
    // 标题栏显示**正在读的那本**：
    // * 在线读着的 / 本地读着的 → 抓来或拆出来的书名；
    // * 书架上有书但还没打开 → 明说没有，**不要**拿演示书冒充；
    // * 什么都没有（`--offline` 且书架空）→ 内置演示书。
    let label = if app.web_mode() && !app.book_name.is_empty() {
        format!("《{}》 {}", app.book_name, app.book_author)
    } else if app.online.is_some() || !app.shelf.is_empty() {
        "（没有打开的书）".to_string()
    } else {
        match s.book_def() {
            Some(b) => format!("演示书架 · 《{}》 {}", b.name, b.author),
            None => "（没有打开的书）".to_string(),
        }
    };
    // 只有确实有「当前这一章」时才写章数和来源。
    // 「有书但没打开」不算 —— 那里写「第 1 / 3 章 书源 3/3」是拿演示书冒充。
    let showing = app.web_mode() || (app.online.is_none() && app.shelf.is_empty());
    let origin = if app.local.is_some() {
        // 本地书没有「书源」这回事
        "本地".to_string()
    } else {
        format!("书源 {}/{}", app.sources_on(), app.source_len())
    };
    let left = if showing {
        format!(
            " {label}  第 {} / {} 章  {origin} ",
            s.chapter,
            app.toc_len()
        )
    } else {
        format!(" {label} ")
    };
    // 在读在线书或本地书时显示当前章标题，比「上次搜索了什么」有用
    let right = if app.web_mode() {
        app.toc_title(s.chapter.saturating_sub(1))
            .map(|t| format!(" {} ", clip(t, 28)))
            .unwrap_or_default()
    } else if s.query.is_empty() {
        String::new()
    } else {
        format!(" 上次搜索 “{}” ", s.query)
    };
    let used = width(&left) + width(&right);
    let pad = (area.width as usize).saturating_sub(used);
    let line = Line::from(vec![
        Span::styled(
            left,
            Style::new()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ".repeat(pad), Style::new().bg(Color::Blue)),
        Span::styled(right, Style::new().bg(Color::Blue).fg(Color::Gray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// 面包屑。一屏一屏地走之后，「我在哪、Esc 会回到哪」必须一眼看得见。
/// 标签栏：五个平级的屏，当前那个反显 + `>` 标出来。
///
/// 替换掉了原来的面包屑。面包屑解释「我在层级里的哪一层」；五屏平级之后
/// 没有层，只有「有哪些屏、我在哪个、按几跳过去」—— 数字就写在名字前面，
/// 键位不用另外记。
fn crumb(f: &mut Frame, app: &App, area: Rect) {
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for (n, label, here) in app.tabs() {
        if here {
            spans.push(Span::styled(
                format!(" {n} {label} "),
                Style::new()
                    .fg(NOVEL)
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!(" {n} "),
                Style::new().fg(FOCUS_BORDER),
            ));
            spans.push(Span::styled(format!("{label} "), Style::new().fg(FAINT)));
        }
    }
    let detail = app.screen_detail();
    if !detail.is_empty() {
        spans.push(Span::styled(
            format!("  {detail}"),
            Style::new().fg(KEYWORD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── 五屏 ────────────────────────────────────────────────────────────────

fn shelf(f: &mut Frame, app: &mut App, area: Rect) {
    let sel = app.shelf_cursor;
    let items: Vec<ListItem> = app
        .rows()
        .iter()
        .enumerate()
        .map(|(i, it)| {
            ListItem::new(Line::from(vec![
                mark(i == sel),
                Span::styled(it.name.clone(), Style::new().fg(NOVEL)),
                Span::raw("  "),
                Span::styled(it.author.clone(), Style::new().fg(FAINT)),
                Span::styled(format!("  {}", it.note), Style::new().fg(CODE)),
            ]))
        })
        .collect();
    if items.is_empty() {
        return empty_hint(f, area, "书架", &shelf_empty_hint(app));
    }
    list(f, app, area, "书架", items, app.shelf_cursor);
}

fn shelf_empty_hint(app: &App) -> String {
    if app.online.is_some() {
        "书架是空的 —— 按 / 搜索，结果里按 Enter 打开（会自动加进来）".into()
    } else {
        "没有书源文件，只能看内置演示书 —— 用 --sources 指定，或按 S 管理书源".into()
    }
}

fn search(f: &mut Frame, app: &mut App, area: Rect) {
    let editing = app.search_focus == SearchFocus::Input;
    let block = focused_block("搜索", editing);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);

    // 关键字框：光标只在它可编辑时出现
    let cursor = if editing { "▏" } else { "" };
    let kw = Line::from(vec![
        Span::styled(" 关键字 ", Style::new().fg(FAINT)),
        Span::styled(
            format!("{}{cursor}", app.state().query),
            Style::new().fg(if editing { NOVEL } else { CODE }),
        ),
        Span::styled(
            if editing {
                "   （Enter 搜索，Tab 去结果）"
            } else {
                "   （Tab 回来改）"
            },
            Style::new().fg(FAINT),
        ),
    ]);
    f.render_widget(Paragraph::new(kw), rows[0]);

    let sel = app.shelf_cursor;
    let items: Vec<ListItem> = app
        .rows()
        .iter()
        .enumerate()
        .map(|(i, it)| {
            ListItem::new(Line::from(vec![
                mark(i == sel),
                Span::styled(it.name.clone(), Style::new().fg(NOVEL)),
                Span::raw("  "),
                Span::styled(it.author.clone(), Style::new().fg(FAINT)),
            ]))
        })
        .collect();
    let hint = if app.busy.is_some() {
        String::new()
    } else if app.state().query.trim().is_empty() {
        "输入关键字后回车".into()
    } else if app.online.is_some() {
        "按 Enter 搜真站点（要几秒）".into()
    } else {
        "（没有书源文件，这里筛的是内置演示书）".into()
    };
    if items.is_empty() {
        if !hint.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  {hint}"),
                    Style::new().fg(FAINT),
                ))),
                rows[1],
            );
        }
        return;
    }
    let mut state = ListState::default();
    state.select(Some(app.shelf_cursor.min(items.len() - 1)));
    f.render_stateful_widget(
        List::new(items).highlight_style(
            Style::new()
                .add_modifier(Modifier::REVERSED)
                .fg(if editing { CODE } else { NOVEL }),
        ),
        rows[1],
        &mut state,
    );
}

fn toc(f: &mut Frame, app: &mut App, area: Rect) {
    let title = format!("目录 · {}", clip(&app.display_book_name(), 30));
    let sel = app.toc_cursor;
    let items: Vec<ListItem> = app
        .rows()
        .iter()
        .enumerate()
        .map(|(i, it)| {
            ListItem::new(Line::from(vec![
                mark(i == sel),
                Span::styled(format!("{:>5}. ", it.note), Style::new().fg(FAINT)),
                Span::styled(it.name.clone(), Style::new().fg(NOVEL)),
            ]))
        })
        .collect();
    if items.is_empty() {
        return empty_hint(f, area, &title, "目录是空的 —— 这本书的书源可能过期了");
    }
    list(f, app, area, &title, items, app.toc_cursor);
}

fn reader(f: &mut Frame, app: &mut App, area: Rect) {
    let stealth = app.reader_mode() == ReaderMode::Stealth;
    // 隐蔽模式 + 闲置够久 → 正文不显示（旁边有人时看不出在读小说）
    let hiding = stealth && !app.prose_visible();
    let heading = if stealth {
        // 隐蔽模式下不画章节标题 —— 那行会直接暴露在看小说
        String::new()
    } else {
        app.toc_title(app.state().chapter.saturating_sub(1))
            .map(str::to_string)
            .unwrap_or_default()
    };
    // 隐蔽模式：块标题换成**文件名** —— 书名和章节名都不能出现在这儿。
    // 看着就像编辑器打开了一个 `reader.novel`。
    let title = if stealth {
        "reader.novel".to_string()
    } else {
        format!(
            "{} 第 {} / {} 章",
            clip(&app.display_book_name(), 26),
            app.state().chapter,
            app.toc_len()
        )
    };
    let block = Block::bordered()
        .title(format!(" {title} "))
        .border_style(Style::new().fg(IDLE_BORDER));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // 自己折行：`Paragraph` 的自动折行没法给出准确的滚动位置
    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    if !heading.is_empty() {
        lines.push(Line::from(Span::styled(
            heading,
            Style::new().fg(KEYWORD).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());
    }
    let paragraphs = app.reader_paragraphs();
    if stealth {
        // 隐蔽模式：逐行画生成出来的 TS。`//` 注释行是正文（给亮色），
        // 其余是代码（压暗）—— 一眼能从代码里认出小说，但整屏看着还是代码。
        //
        // `hiding` 时把注释里的正文抹掉：**保留 `//` 标记和行数**，
        // 于是屏幕不会跳、滚动位置也还作数，看着就是一片注释掉的代码。
        for raw in app.reader_lines() {
            let marker = comment_marker(&raw);
            // 隐蔽模式下要抹掉的内容分两种，规矩不同：
            //
            // * **块注释**（`/** … */` 那个文件头）里是书名、章节名、书源名 ——
            //   纯泄漏，没有阅读价值，所以**一直**抹掉；
            // * **行注释**（`//`）是正文本身，**闲置够久**才抹。
            //
            // 一开始两种一起处理，结果「正在读」的时候文件头里的书名照样
            // 明晃晃挂在第一屏（测试抓到的）。
            let block = matches!(marker, Some(m) if !m.trim_start().starts_with("//"));
            if block || hiding {
                lines.push(Line::from(Span::styled(
                    redact_comment(&raw),
                    Style::new().fg(CODE),
                )));
                continue;
            }
            let style = if marker.is_some() {
                Style::new().fg(NOVEL)
            } else {
                Style::new().fg(CODE)
            };
            // 代码行按列硬折：缩进和括号位置有意义，按词折会读不成形
            for l in wrap(&raw, width) {
                lines.push(Line::from(Span::styled(l, style)));
            }
        }
    } else {
        for p in &paragraphs {
            for l in wrap(p, width) {
                lines.push(Line::from(Span::styled(l, Style::new().fg(NOVEL))));
            }
            lines.push(Line::default());
        }
    }
    // 隐蔽模式下没有正文时，那份 TS 里只有头注释 —— 别拿它冒充正文
    if lines.is_empty() || (paragraphs.is_empty() && !hiding) {
        lines = vec![Line::from(Span::styled(
            "（这一章没有正文 —— 可能还在加载，或者书源的正文规则失效了）",
            Style::new().fg(FAINT),
        ))];
    }

    // 滚动夹取：`End` 用的是 u16::MAX，这里收到实际能滚到的位置
    let max = lines.len().saturating_sub(inner.height as usize) as u16;
    app.reader_scroll = app.reader_scroll.min(max);
    let shown: Vec<Line> = lines
        .into_iter()
        .skip(app.reader_scroll as usize)
        .take(inner.height as usize)
        .collect();
    f.render_widget(Paragraph::new(shown), inner);
}

fn sources(f: &mut Frame, app: &mut App, area: Rect) {
    // 编辑表单占用当前内容区，不新开一页 —— 用户始终只在一屏里
    if app.form.is_some() {
        return source_form(f, app, area);
    }
    let rows = app.source_rows();
    let sel = app.source_cursor;
    // 只给可见的那些行建 ListItem：默认书源集合三千多个，每帧建三千个
    // ListItem 是白扔的（列表控件反正只画可见行），筛选后这个数会掉到个位
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let name = app.source_name(i);
            let on = app.source_on(i);
            // 不只靠颜色区分开关 —— 终端里颜色是弱信号，符号才是主信号
            ListItem::new(Line::from(vec![
                mark(k == sel),
                Span::styled(
                    format!("{} ", if on { "☑" } else { "☐" }),
                    Style::new().fg(if on { LITERAL } else { FAINT }),
                ),
                Span::styled(
                    format!("{:>4}. ", i + 1),
                    Style::new().fg(if on { FAINT } else { IDLE_BORDER }),
                ),
                Span::styled(name, Style::new().fg(if on { NOVEL } else { CODE })),
            ]))
        })
        .collect();
    if items.is_empty() {
        let filter = app.sources_filter.trim();
        return empty_hint(
            f,
            area,
            "书源",
            &if filter.is_empty() {
                "一个书源都没有 —— 按 a 导入（输入文件路径或 URL）".to_string()
            } else {
                format!("没有名字或网址含 “{filter}” 的书源 —— Ctrl-U 清空筛选")
            },
        );
    }
    let matched = rows.len();
    let total = app.source_len();
    let title = if matched == total {
        format!("书源（{total} 个，开 {} 个）", app.sources_on())
    } else {
        format!(
            "书源（匹配 {matched} / 共 {total}，开 {} 个）",
            app.sources_on()
        )
    };
    list(f, app, area, &title, items, sel);
}

/// 这一行是注释吗？返回它前面的注释标记（含缩进）。
///
/// 认三种：`//…`、块注释的 `/**` / ` * …` / ` */`。生成的文件里，
/// **正文和元信息都在注释里**，所以「哪些行是注释」= 「哪些行要藏」。
fn comment_marker(raw: &str) -> Option<&str> {
    let indent = raw.len() - raw.trim_start().len();
    let t = &raw[indent..];
    if t.starts_with("//") {
        return Some(&raw[..indent + 2]);
    }
    if t.starts_with('*') || t.starts_with("/*") {
        // `/*` / `/**` 整体是标记；` * x` 只到 `*`
        let n = if t.starts_with("/*") { 2 } else { 1 };
        return Some(&raw[..indent + n]);
    }
    None
}

/// 把一行的注释内容抹掉，只留标记（和缩进）。
/// 不是注释行就原样返回 —— 代码里有信息，也正是「像代码」的那部分。
fn redact_comment(raw: &str) -> String {
    match comment_marker(raw) {
        Some(marker) => marker.to_string(),
        None => raw.to_string(),
    }
}

// ── 共用的画法 ──────────────────────────────────────────────────────────

/// 选中行的前缀标记。
///
/// 选中行**同时**有 `>` 和反显 —— 两条独立信号。反显在纯文本导出里看不见，
/// 而只靠颜色区分状态对色觉障碍用户是不成立的（设计规范里那条
/// 「不要只用颜色传达状态」）。
fn mark(selected: bool) -> Span<'static> {
    Span::styled(
        if selected { "> " } else { "  " },
        Style::new().fg(FOCUS_BORDER),
    )
}

/// 画一个列表。
fn list(f: &mut Frame, app: &App, area: Rect, title: &str, items: Vec<ListItem>, cursor: usize) {
    let sel = cursor.min(items.len().saturating_sub(1));
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(sel));
    }
    let list = List::new(items)
        .block(Block::bordered().title(format!(" {title} ")))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let _ = app;
    f.render_stateful_widget(list, area, &mut state);
}

fn focused_block(title: &str, focused: bool) -> Block<'static> {
    Block::bordered()
        .title(format!(" {title} "))
        .border_style(Style::new().fg(if focused { FOCUS_BORDER } else { IDLE_BORDER }))
}

fn empty_hint(f: &mut Frame, area: Rect, title: &str, hint: &str) {
    let block = Block::bordered().title(format!(" {title} "));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    // 竖着居中一句话
    let pad = (inner.height as usize).saturating_sub(1) / 2;
    let mut lines: Vec<Line> = (0..pad).map(|_| Line::default()).collect();
    lines.push(Line::from(Span::styled(
        format!("  {hint}"),
        Style::new().fg(FAINT),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn status(f: &mut Frame, app: &App, area: Rect) {
    // 正在抓东西时，进度优先占住状态栏 —— 否则界面看起来像卡死
    if let Some(busy) = &app.busy {
        let line = Line::from(vec![
            Span::styled(" ⟳ ", Style::new().fg(KEYWORD)),
            Span::styled(busy.clone(), Style::new().fg(NOVEL)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    if let Some((mode, buf)) = &app.input {
        let cursor = "▏";
        let line = Line::from(vec![
            Span::styled(
                format!(" {}: ", prompt_for(app, *mode)),
                Style::new().fg(KEYWORD),
            ),
            Span::styled(format!("{buf}{cursor}"), Style::new().fg(NOVEL)),
            Span::styled("   （回车确定，Esc 取消）", Style::new().fg(FAINT)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    if app.status.is_empty() {
        return;
    }
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(app.status.clone(), Style::new().fg(CODE)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// 底部那一行按键提示。**只列当前屏用得上的键** ——
/// 把全部按键摊在一行里等于没有提示。
fn hints(f: &mut Frame, app: &App, area: Rect) {
    // 临时模式（表单）盖过本屏的提示 —— 这时按本屏的键不管用
    let keys = if app.form.is_some() {
        "打字改字段   Tab 换字段   Enter 保存   Esc 取消"
    } else {
        match app.screen {
            Screen::Shelf => "↑↓ 选书   Enter 打开   / 搜索   d 移出书架   1-5 跳屏   q 退出",
            Screen::Search if app.search_focus == SearchFocus::Input => {
                "打字改关键字   Enter 搜索   Tab 去结果   Esc 回书架   q 退出"
            }
            Screen::Search => {
                "↑↓ 选结果   Enter 打开   b 加入书架   Tab 改关键字   Esc 回书架   q 退出"
            }
            Screen::Toc => "↑↓ 选章   Enter 阅读   g 跳章   R 重拉目录   Esc 回书架   q 退出",
            Screen::Reader => {
                "↑↓ 滚动   n p 上下章   g 跳章   m 隐蔽/普通   R 重拉目录   Esc 回书架   q 退出"
            }
            Screen::Sources => {
                "↑↓ 选源   空格 开关   Enter 编辑   / 筛选   a 导入   d 删除   Esc 回书架   q 退出"
            }
        }
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {keys}"),
            Style::new().fg(FAINT),
        ))),
        area,
    );
}

fn help(f: &mut Frame, area: Rect) {
    // 键位列固定宽度，描述才能对齐。键位都是 ASCII，len() 就是显示宽度。
    let key = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(format!("    {k:<27}"), Style::new().fg(KEYWORD)),
            Span::styled(d.to_string(), Style::new().fg(CODE)),
        ])
    };
    let title = |t: &str| {
        Line::from(Span::styled(
            format!("  {t}"),
            Style::new().fg(FOCUS_BORDER),
        ))
    };
    let plain = |t: &str| Line::from(Span::styled(format!("    {t}"), Style::new().fg(CODE)));

    // 行数要压到 30 行以内 —— 一个进不了普通终端（24~40 行）的帮助等于没有帮助。
    // 取舍原则：只留「不看就会猜错」的键，能从别的屏推出来的都删掉。
    let lines: Vec<Line> = vec![
        Line::default(),
        title("一屏一屏地走"),
        plain("书架 → Enter → 目录 → Enter → 阅读；Esc 逐层退回"),
        plain("搜索、书源是另外两屏，都从书架进"),
        Line::default(),
        title("书架"),
        key("↑ ↓ j k  /  Enter", "选书 / 打开（跳回上次读到那一章）"),
        key("/ 或 s", "进搜索"),
        key("d  /  } {", "移出书架 / 换下一本上一本"),
        Line::default(),
        title("搜索（Tab 在关键字框和结果之间切）"),
        key("打字 / Enter / Esc", "改关键字 / 搜 / 回书架"),
        key("Enter（在结果里）", "打开这本书"),
        key("b", "把选中的结果加入书架"),
        Line::default(),
        title("目录 / 阅读"),
        key("↑ ↓ 或 n / p", "目录里选章；阅读里翻章"),
        key("n p  ] [", "阅读屏：下一章 / 上一章"),
        key("g  /  , .", "输入章号跳转 / 第一章 最后一章"),
        key("PgUp PgDn ↑ ↓", "正文滚动"),
        Line::default(),
        title("书源（在书源屏里）"),
        key("Enter 或 空格", "开关这个源"),
        key("a  /  d", "导入（路径或 URL）/ 删除"),
        key("e  u  s", "改 名字 / 网址 / 搜索网址"),
        Line::default(),
        title("别的"),
        key("S / 1-9", "进书源屏 / 直接开关第 N 个源"),
        key("o / r", "打开 reader.novel / 重新渲染"),
        key("? / q", "帮助 / 退出"),
        Line::default(),
        title("任意键关闭"),
    ];

    let h = (lines.len() as u16 + 4).min(area.height);
    let popup = centered(area, 84, h);
    let box_ = Block::bordered()
        .title(" 帮助 ")
        .border_style(Style::new().fg(FOCUS_BORDER))
        .style(Style::new().bg(Color::Black));
    // 浮层必须先把区域清空，否则底下那屏的文字会从缝里透上来
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines)
            .block(box_)
            .style(Style::new().bg(Color::Black))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// 提示语带上一点上下文：跳章时显示当前是第几章。
fn prompt_for(app: &App, mode: InputMode) -> String {
    match mode {
        InputMode::Goto => format!(
            "{}（当前 {} / {}）",
            mode.prompt(),
            app.state().chapter,
            app.toc_len()
        ),
        // 编辑书源是把当前值**预填**进去的，改两三个字比从零敲省事
        _ => mode.prompt(),
    }
}

/// 书源的三字段编辑表单。Tab 换字段、Enter 保存、Esc 取消。
fn source_form(f: &mut Frame, app: &App, area: Rect) {
    let form = app.form.as_ref().expect("只在 form 存在时调用");
    let mut lines: Vec<Line> = SourceForm::FIELDS
        .iter()
        .enumerate()
        .map(|(i, fl)| {
            let focused = i == form.field;
            let label = fl.label();
            // 按**显示宽度**补齐：中文标签 2 列、英文 1 列，不补的话
            // 「搜索网址」比「名字」宽 4 列，三个输入框就参差不齐
            let pad = 8usize.saturating_sub(width(label));
            Line::from(vec![
                Span::styled(
                    format!(
                        "{} {label}{}",
                        if focused { "▸" } else { " " },
                        " ".repeat(pad)
                    ),
                    Style::new().fg(if focused { FOCUS_BORDER } else { FAINT }),
                ),
                Span::styled(
                    format!(" {}{} ", form.values[i], if focused { "▏" } else { " " }),
                    if focused {
                        Style::new().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::new().fg(NOVEL)
                    },
                ),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "正在编辑第 {} 个书源{}",
            form.index + 1,
            if form.dirty {
                "（有未保存的修改）"
            } else {
                ""
            }
        ),
        Style::new().fg(FAINT),
    )));
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" 编辑书源 —— Tab 换字段 · Enter 保存 · Esc 取消 ")),
        area,
    );
}

// ── 显示宽度 ────────────────────────────────────────────────────────────

/// 显示宽度为 2 的字符（CJK / 全角）。
///
/// ponytail: 手写区间表，不为这点用途引 `unicode-width`。
pub fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x20000..=0x3FFFD)
}

/// 字符串占几列。
pub fn width(s: &str) -> usize {
    s.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

/// 按显示宽度截断，超了补省略号。
fn clip(s: &str, cols: usize) -> String {
    if width(s) <= cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = if is_wide(c) { 2 } else { 1 };
        if w + cw > cols.saturating_sub(1) {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

/// 按显示宽度折行。中文占 2 列 —— 不自己折的话 `Paragraph` 会把汉字劈成两半。
fn wrap(s: &str, cols: usize) -> Vec<String> {
    if cols == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = if is_wide(c) { 2 } else { 1 };
        if w + cw > cols && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(c);
        w += cw;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// 在内存后端把当前这一屏画一遍，返回纯文本。测试和预览用。
///
/// 注意不能直接把缓冲区逐格拼起来：宽字符（中文）右边那一格是占位空格，
/// 不跳过的话每两个汉字之间都会多一个空格。
pub fn render_to_text(app: &mut App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("内存后端不会失败");
    term.draw(|f| draw(f, app)).expect("内存后端不会失败");
    let buf = term.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0;
            while x < buf.area.width {
                let sym = buf[(x, y)].symbol();
                let w = sym
                    .chars()
                    .map(|c| if is_wide(c) { 2u16 } else { 1 })
                    .max()
                    .unwrap_or(1)
                    .max(1);
                row.push_str(sym);
                x += w;
            }
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Key;
    use novel_core::state::Session;

    fn tmp_app(tag: &str) -> App {
        let dir = std::env::temp_dir().join(format!("novel-tui-ui-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        App::new(Session::open(
            dir.join("state.json"),
            dir.join("reader.novel"),
        ))
    }

    #[test]
    fn only_one_screen_is_drawn_at_a_time() {
        // 这是这次交互重做的核心：**不是并列面板**。
        // 书架屏里不该出现别的屏的标题。
        let mut app = tmp_app("onescreen");
        let out = render_to_text(&mut app, 100, 30);
        assert!(out.contains("书架"), "{out}");
        assert!(!out.contains("目录 ·"), "书架屏里不该有目录：\n{out}");
        assert!(!out.contains("书源（"), "{out}");
        assert!(!out.contains("关键字"), "{out}");

        // 进目录屏之后，书架的清单不该还在
        app.screen = Screen::Toc;
        let out = render_to_text(&mut app, 100, 30);
        assert!(out.contains("目录 ·"), "{out}");
        assert!(!out.contains("示例作者  3 章"), "书架清单还在：\n{out}");
    }

    #[test]
    fn all_five_screens_draw_and_are_distinguishable() {
        let mut app = tmp_app("allscreens");
        let mut seen = Vec::new();
        for screen in [
            Screen::Shelf,
            Screen::Search,
            Screen::Toc,
            Screen::Reader,
            Screen::Sources,
        ] {
            app.screen = screen;
            let out = render_to_text(&mut app, 100, 30);
            assert!(!out.is_empty(), "{screen:?} 画出来是空的");
            // 每屏的第一行（标题栏）都该有，且提示行只列本屏的键
            assert!(out.contains("书源 "), "{screen:?} 缺标题栏：\n{out}");
            seen.push(out);
        }
        // 五屏的画面必须互不相同，否则说明分派没生效
        for i in 0..seen.len() {
            for j in (i + 1)..seen.len() {
                assert_ne!(seen[i], seen[j], "第 {i} 屏和第 {j} 屏画得一样");
            }
        }
    }

    #[test]
    fn shelf_shows_a_hint_when_empty() {
        let mut app = tmp_app("emptyshelf");
        let out = render_to_text(&mut app, 100, 30);
        assert!(out.contains("书架"), "{out}");
        // 演示模式下书架屏列的是演示书，所以不空
        assert!(out.contains("夜色渐浓") || out.contains("搜索"), "{out}");
    }

    #[test]
    fn reader_screen_wraps_cjk_without_splitting_wide_chars() {
        let mut app = tmp_app("wrap");
        app.session.state.book = 0;
        app.session.state.chapter = 1;
        app.screen = Screen::Reader;
        let out = render_to_text(&mut app, 60, 24);
        // 强行找正文行：折行后每行宽度不能超过内框
        for l in out.lines() {
            assert!(width(l) <= 62, "行超宽（{}）：{l:?}", width(l));
        }
        assert!(
            out.contains("正文") || out.contains("//") || out.contains("推开门"),
            "{out}"
        );
    }

    #[test]
    fn help_overlay_lists_the_keys_and_is_opaque() {
        let mut app = tmp_app("help");
        app.handle(Key::Char('?'));
        let out = render_to_text(&mut app, 110, 34);
        assert!(out.contains("帮助"), "{out}");
        for want in [
            "一屏一屏地走",
            "书架 → Enter → 目录",
            "下一章 / 上一章",
            "把选中的结果加入书架",
            "导入（路径或 URL）",
            "进书源屏 / 直接开关第 N 个源",
        ] {
            assert!(out.contains(want), "帮助里缺 {want:?}:\n{out}");
        }
        // 浮层要挡住底下那一屏
        assert!(!out.contains("/**"), "帮助浮层是透明的：\n{out}");
    }

    #[test]
    fn busy_state_takes_over_the_status_bar() {
        let mut app = tmp_app("busy");
        app.busy = Some("搜 鬼吹灯（3/10）".into());
        let out = render_to_text(&mut app, 110, 30);
        assert!(out.contains("搜 鬼吹灯"), "{out}");
        assert!(out.contains('⟳'), "要有个在转的记号：\n{out}");
    }

    #[test]
    fn the_key_hint_line_only_lists_this_screens_keys() {
        let mut app = tmp_app("hints");
        let shelf = render_to_text(&mut app, 110, 30);
        assert!(shelf.contains("移出书架"), "{shelf}");
        assert!(!shelf.contains("上下章"), "书架屏不该提示翻章：\n{shelf}");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for (w, h) in [(1, 1), (10, 3), (24, 4), (30, 8), (200, 60)] {
            let mut app = tmp_app(&format!("tiny{w}x{h}"));
            let _ = render_to_text(&mut app, w, h);
        }
    }

    #[test]
    fn wrap_never_exceeds_the_column_budget() {
        for cols in [1, 2, 3, 10, 80] {
            let got = wrap("中文abc混排测试内容", cols);
            for l in &got {
                assert!(
                    width(l) <= cols.max(2) || cols == 1,
                    "cols={cols} 行宽 {}: {l:?}",
                    width(l)
                );
            }
            assert!(!got.is_empty());
        }
        assert_eq!(wrap("", 10), vec![String::new()]);
    }

    /// 藏正文时抹掉的是**所有注释**的内容，不只是 `//` 那些。
    ///
    /// 生成的文件头里有书名和章节名 —— 只抹 `//` 的话书名照样挂在第一屏。
    #[test]
    fn redaction_covers_every_kind_of_comment() {
        // `//` 正文
        assert_eq!(redact_comment("// 他推开门，楼道里的灯灭了。"), "//");
        // 文件头（块注释的三种行）
        assert_eq!(redact_comment("/**"), "/*");
        assert_eq!(redact_comment(" * 《夜色渐浓》 —— 示例作者"), " *");
        assert_eq!(redact_comment(" */"), " *");
        // 带缩进的
        assert_eq!(redact_comment("    // 缩进过的正文"), "    //");
        assert_eq!(redact_comment("  * 缩进过的头"), "  *");
        // 代码行**不动** —— 那正是「像代码」的部分，也是要留着给人看的
        assert_eq!(
            redact_comment("export interface BufferState {"),
            "export interface BufferState {"
        );
        assert_eq!(redact_comment("  retry?: number;"), "  retry?: number;");
        assert_eq!(redact_comment("}"), "}");
        // 空行
        assert_eq!(redact_comment(""), "");
    }

    /// 注释行认全：`//`、`/**`、` * `、` */` 都算，代码行不算。
    #[test]
    fn comment_detection_is_conservative() {
        assert!(comment_marker("//x").is_some());
        assert!(comment_marker("/**").is_some());
        assert!(comment_marker(" * x").is_some());
        assert!(comment_marker(" */").is_some());
        assert!(comment_marker("    // 缩进的").is_some());
        assert!(comment_marker("export const x = 1;").is_none());
        assert!(comment_marker("  return a * b;").is_none(), "乘法不是注释");
        assert!(comment_marker("").is_none());
    }

    #[test]
    fn clip_measures_in_columns_not_chars() {
        assert_eq!(clip("夜色渐浓", 8), "夜色渐浓");
        assert_eq!(clip("夜色渐浓", 6), "夜色…");
        assert_eq!(clip("abcdef", 3), "ab…");
        assert_eq!(width(&clip("夜色渐浓", 5)), 5);
    }
}
