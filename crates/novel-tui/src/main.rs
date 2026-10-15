//! 独立 TUI 小说阅读器。不依赖任何编辑器。
//!
//! 用法：
//! * `novel-tui` —— 进交互界面
//! * `novel-tui --dump` —— 只把渲染结果打到 stdout（管道/脚本里用）
//! * `--dir DIR` —— 把 `.novel/` 放在 DIR 下，默认当前目录

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use novel_core::online::Online;
use novel_core::state::Session;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use novel_tui::app::{App, Key};
use novel_tui::run::{self, Driver, Workers};
use novel_tui::ui;

/// 书源文件默认放在状态文件旁边。
const SOURCES_FILE: &str = ".novel/sources.json";

/// 内置默认书源集合。`.novel/sources.json` 不存在时自动拉这一份。
///
/// 只在「文件不存在」时下载（约 20 MB）：每次启动都拉一遍是拿 20 MB 换一个
/// 大概率没变的集合，不划算。想手动全删就用文件里的空列表，或 `--offline`。
const DEFAULT_SOURCES_URL: &str = "https://legado.aoaostar.com/sources/b778fe6b.json";

/// 没有按键时事件循环醒来看看后台有没有结果。100ms 足够跟手，也不烧 CPU。
const TICK: Duration = Duration::from_millis(100);

const HELP: &str = "\
novel-tui —— 独立的小说阅读器（TUI）

  novel-tui                进交互界面
  novel-tui --dump         只把当前这一章的渲染结果打到 stdout，然后退出
  novel-tui --dir DIR      把 .novel/ 放在 DIR 下（默认当前目录）
  novel-tui --sources FILE 指定书源文件（默认 .novel/sources.json）
  novel-tui --offline      不加载书源（也不下载内置集合），只用内置演示书库
  novel-tui --script KEYS  不进真终端：把一串按键喂进去，把最后的界面打到 stdout
  novel-tui --trace        配合 --script，把每一帧都打出来
  novel-tui --help         这个帮助
  novel-tui --version

界面里按 ? 看按键表。

**本地书**：把 `.epub` 放进 exe 旁边的 `library/`，重启就在书架上。

`--script` 的写法：普通字符就是字符键，`{down}` `{enter}` `{esc}` `{pgup}`
`{ctrl-c}` 这类是特殊键（见 run.rs 的 named_key）。
例：novel-tui --script '{down}{enter}nnp'  —— 打开第二本、翻两章再回一章。

状态在 .novel/state.json，渲染结果在 .novel/reader.novel；
每次改动都立刻写盘，所以别的程序可以随时读 reader.novel。

书源问的是 legado 的导出格式（`bookSourceName` / `ruleSearch` …），
可以直接用现成的书源集合，不用转换。不支持的能力（如 AES）会明确报错，
而不是悄悄抓到空内容。
";

/// 命令行。选项只能出现在这个结构里，别处一律用字段，
/// 免得「忘了过滤某个选项 → 被当成子命令」这种错再犯一次。
struct Args {
    dir: PathBuf,
    sources: Option<PathBuf>,
    dump: bool,
    offline: bool,
    help: bool,
    version: bool,
    /// 不进真终端，把这段按键脚本喂给同一个事件循环
    script: Option<String>,
    /// 配合 `--script`：每一帧都打出来
    trace: bool,
}

impl Args {
    fn parse(argv: &[String]) -> Result<Self, String> {
        let mut me = Self {
            dir: PathBuf::from("."),
            sources: None,
            dump: false,
            offline: false,
            help: false,
            version: false,
            script: None,
            trace: false,
        };
        let mut i = 0;
        while i < argv.len() {
            let take = |name: &str| -> Result<PathBuf, String> {
                argv.get(i + 1)
                    .map(PathBuf::from)
                    .ok_or_else(|| format!("{name} 后面要跟一个路径"))
            };
            match argv[i].as_str() {
                "--dir" => {
                    me.dir = take("--dir")?;
                    i += 2;
                }
                "--sources" => {
                    me.sources = Some(take("--sources")?);
                    i += 2;
                }
                "--dump" => {
                    me.dump = true;
                    i += 1;
                }
                "--offline" => {
                    me.offline = true;
                    i += 1;
                }
                "--script" => {
                    me.script = Some(
                        argv.get(i + 1)
                            .cloned()
                            .ok_or("--script 后面要跟一串按键")?,
                    );
                    i += 2;
                }
                "--trace" => {
                    me.trace = true;
                    i += 1;
                }
                "--help" | "-h" => {
                    me.help = true;
                    i += 1;
                }
                "--version" | "-V" => {
                    me.version = true;
                    i += 1;
                }
                other => return Err(format!("不认识的参数 {other:?}")),
            }
        }
        Ok(me)
    }

    /// 书源路径：命令行给的优先，否则按约定找 `.novel/sources.json`。
    fn sources_path(&self) -> PathBuf {
        self.sources
            .clone()
            .unwrap_or_else(|| self.dir.join(SOURCES_FILE))
    }
}

fn main() -> io::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match Args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}；用 --help 看用法");
            std::process::exit(2);
        }
    };
    if args.help {
        println!("{HELP}");
        return Ok(());
    }
    if args.version {
        println!("novel-tui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.dump {
        return dump(&args.dir);
    }

    // 书源是可选的：没有就只跑演示书库，界面照旧能用。
    let sources = if args.offline {
        None
    } else {
        load_sources(&args.sources_path())
    };
    if let Some(script) = args.script {
        return run_script(&args.dir, sources, &script, args.trace);
    }
    run(&args.dir, sources)
}

/// 读不到书源不算错误 —— 用户可能就想看看演示书库。
fn load_sources(path: &Path) -> Option<Online> {
    // 全新安装：下内置集合，下完照常进界面。
    if !path.exists() {
        return Some(install_default_sources(path));
    }
    match Online::load(path) {
        Ok(o) if !o.is_empty() => {
            eprintln!(
                "书源 {} 个（可搜索 {} 个），来自 {}",
                o.len(),
                o.searchable(),
                path.display()
            );
            Some(o)
        }
        // 空集合也算「有书源」：否则删光最后一条之后，`a` 会永远报
        // 「没有加载书源文件」，加不回来。（`--offline` 仍然是 None → 演示书库。）
        Ok(o) => Some(o),
        Err(e) => {
            eprintln!("书源文件有问题：{e}");
            None
        }
    }
}

/// 下内置书源集合，落盘到 `path`，返回**和正常启动同一条加载路径**得到的列表。
///
/// 失败不致命：返回空集合（路径已绑好），界面上按 `a` 还能手动导入。
fn install_default_sources(path: &Path) -> Online {
    let mut online = Online::at(path);
    eprintln!(
        "没有 {} —— 下载内置书源集合（约 20 MB，可能要一分钟）…\n{}",
        path.display(),
        DEFAULT_SOURCES_URL
    );
    if let Err(e) = online.import(DEFAULT_SOURCES_URL) {
        eprintln!("内置书源下载失败：{e}");
        eprintln!("书源屏按 a 手动导入；或加 --offline 只用演示书库");
        return online;
    }
    // 重新走一遍 load：`import` 不滤掉停用/非文本源，直接用它会和下次启动的
    // 列表条数对不上（2783 vs 2588），白送一个「怎么少了几百个」的疑问。
    match Online::load(path) {
        Ok(o) if !o.is_empty() => {
            eprintln!(
                "内置书源就位：{} 个（可搜索 {} 个）→ {}",
                o.len(),
                o.searchable(),
                path.display()
            );
            o
        }
        _ => online,
    }
}

fn session(dir: &Path) -> Session {
    Session::open(
        dir.join(novel_core::STATE_PATH),
        dir.join(novel_core::READER_PATH),
    )
}

fn dump(dir: &Path) -> io::Result<()> {
    let s = session(dir);
    let Some(text) = s.rendered() else {
        eprintln!("没有要渲染的章节");
        std::process::exit(1);
    };
    print!("{text}");
    Ok(())
}

fn run(dir: &Path, sources: Option<Online>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let mut app = build_app(dir, sources);

    let workers = Workers::new(app.online.as_ref());
    let result = run::run(
        &mut app,
        &workers,
        |app| {
            terminal.draw(|f| ui::draw(f, app))?;
            Ok(())
        },
        |_app: &mut App| {
            // 两条容易忽略的：
            // * **只认 Press/Repeat** —— Windows 上 crossterm 会把 Release 也送过来，
            //   不过滤的话每个键都被处理两遍（翻章会一次跳两章）；
            // * **错误要往外抛** —— 早先写成 `let Ok(..) = event::read() else { continue }`，
            //   stdin 一关 `event::read()` 就永远返回 Err，于是死循环烧 CPU 且退不出去。
            if !event::poll(TICK)? {
                return Ok(None);
            }
            Ok(match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => translate(k),
                Event::Resize(..) => None,
                _ => None,
            })
        },
    );

    ratatui::restore();
    result
}

/// 装配一个 `App`。真跑和脚本模式共用，免得两边配置不一样。
fn build_app(dir: &Path, sources: Option<Online>) -> App {
    let mut app = App::new(session(dir));
    if let Some(o) = sources {
        app.set_online(o);
    }
    // 本地书库：exe 旁边的 `library/`。没有就建一个 —— 不然用户不知道 epub 往哪放。
    let lib = library_dir();
    let _ = std::fs::create_dir_all(&lib);
    let n = app.load_local_library(&lib);
    if n > 0 {
        eprintln!("本地书 {n} 本，来自 {}", lib.display());
    } else {
        eprintln!("本地书库 {}（把 .epub 丢进去就会上架）", lib.display());
    }
    // 进界面先把这一章写出去，保证 reader.novel 一定是最新的
    app.persist();
    app
}

/// 本地书库在**exe 旁边**的 `library/`。
///
/// 刻意按 exe 而不是当前目录：双击、换目录跑的时候「我的书在哪」不该跟着变。
fn library_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("library")
}

/// 脚本模式：把按键喂给**同一个事件循环**，把界面打到 stdout。
///
/// 存在的理由是自测 —— 「按键 → 重画 → 再按键」这条链以前只能人眼验，
/// 而卡死、丢结果、退不出去都出在这里。
fn run_script(dir: &Path, sources: Option<Online>, script: &str, trace: bool) -> io::Result<()> {
    let mut app = build_app(dir, sources);
    let workers = Workers::new(app.online.as_ref());
    let mut frames: Vec<String> = Vec::new();

    run::run(
        &mut app,
        &workers,
        |app: &mut App| {
            frames.push(ui::render_to_text(app, 110, 34));
            Ok(())
        },
        // 用同一个驱动台：按键喂完它自己收尾（发退出键），
        // 排了抓取任务时还会多等几轮让 worker 交结果
        {
            let (tx, _rx) = std::sync::mpsc::channel();
            let mut driver = Driver::script(script, tx);
            move |app: &mut App| driver.next(app)
        },
    )?;

    if trace {
        for (i, f) in frames.iter().enumerate() {
            println!("── 第 {i} 帧 ──");
            println!("{f}");
        }
    } else if let Some(last) = frames.last() {
        println!("{last}");
    }
    Ok(())
}

fn translate(k: KeyEvent) -> Option<Key> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    Some(match k.code {
        KeyCode::Char(c) if ctrl => Key::Ctrl(c.to_ascii_lowercase()),
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab if shift => Key::BackTab,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventState;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn translate_maps_the_keys_the_ui_documents() {
        assert_eq!(
            translate(ev(KeyCode::Char('n'), KeyModifiers::NONE)),
            Some(Key::Char('n'))
        );
        assert_eq!(
            translate(ev(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Key::Enter)
        );
        assert_eq!(
            translate(ev(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Key::Esc)
        );
        assert_eq!(
            translate(ev(KeyCode::Char('?'), KeyModifiers::SHIFT)),
            Some(Key::Char('?'))
        );
        // 大写字母不能被当成 Ctrl：Shift 也要走 Char
        assert_eq!(
            translate(ev(KeyCode::Char('N'), KeyModifiers::SHIFT)),
            Some(Key::Char('N'))
        );
    }

    #[test]
    fn translate_lowercases_ctrl_so_ctrl_shift_c_is_still_ctrl_c() {
        assert_eq!(
            translate(ev(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Key::Ctrl('c'))
        );
        assert_eq!(
            translate(ev(
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(Key::Ctrl('c'))
        );
    }

    #[test]
    fn translate_distinguishes_tab_from_backtab() {
        assert_eq!(
            translate(ev(KeyCode::Tab, KeyModifiers::NONE)),
            Some(Key::Tab)
        );
        assert_eq!(
            translate(ev(KeyCode::Tab, KeyModifiers::SHIFT)),
            Some(Key::BackTab)
        );
        assert_eq!(
            translate(ev(KeyCode::BackTab, KeyModifiers::NONE)),
            Some(Key::BackTab)
        );
    }

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// 这条当初漏了：`--sources` 没被认出来，被当成子命令
    /// 「不认识的参数 --sources」直接退出。
    #[test]
    fn options_are_recognised_in_any_order() {
        let a = Args::parse(&argv(&["--dump", "--dir", "/tmp/x"])).unwrap();
        assert!(a.dump);
        assert_eq!(a.dir, PathBuf::from("/tmp/x"));

        let a = Args::parse(&argv(&["--dir", "/tmp/x", "--sources", "s.json"])).unwrap();
        assert_eq!(a.sources_path(), PathBuf::from("s.json"));
        // 没给 --sources 就按约定找
        let a = Args::parse(&argv(&["--dir", "/tmp/x"])).unwrap();
        assert_eq!(a.sources_path(), PathBuf::from("/tmp/x").join(SOURCES_FILE));

        assert!(Args::parse(&argv(&["--offline"])).unwrap().offline);
        assert!(Args::parse(&argv(&[])).is_ok());
    }

    #[test]
    fn missing_option_values_and_typos_are_errors_not_panics() {
        assert!(Args::parse(&argv(&["--dir"])).is_err());
        assert!(Args::parse(&argv(&["--sources"])).is_err());
        assert!(Args::parse(&argv(&["--nope"])).is_err());
        assert!(Args::parse(&argv(&["dump"])).is_err(), "子命令要带 --");
    }

    #[test]
    fn unmapped_keys_are_dropped_rather_than_guessed() {
        assert_eq!(translate(ev(KeyCode::F(5), KeyModifiers::NONE)), None);
        assert_eq!(translate(ev(KeyCode::Insert, KeyModifiers::NONE)), None);
    }
}
