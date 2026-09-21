//! 事件循环与后台抓取的接线。
//!
//! 单独抽出来是为了**能自测**：真跑时事件来自 crossterm，测试里来自一段脚本。
//! 之前这套逻辑长在 `main()` 里，于是「按键 → 重画 → 再按键」这条链
//! 只能靠人眼验，而它恰恰是最容易出问题的一段（卡死、丢结果、退不出去）。

use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use novel_core::online::{Done, Job, Msg, Online};

use crate::app::{App, Key};

/// 抓取线程的栈要够大：QuickJS 自己会深递归，`set_max_stack_size` 比的是
/// **原生栈用量**，线程栈只比它大一点点就会把进程直接带崩。
pub const WORKER_STACK: usize = 8 * 1024 * 1024;

/// 进度消息走 `Done::Failed` 的通道传回来，用这个前缀和真失败区分开。
///
/// 多开一个消息类型不值当 —— `Done` 是给 `App::on_done` 用的，
/// 而进度只影响状态栏一行字。
const PROGRESS_MARK: &str = "\u{1}进度\u{1}";

/// 排出去的抓取任务往哪去。
enum Backend {
    /// 真抓（起线程）
    Real(Arc<Mutex<Online>>),
    /// 没有书源文件：排了任务就当场报错，不能让人以为在加载
    NoSources,
    /// 自测：任务丢掉，结果由测试自己塞进通道。
    /// 这样「按键 → 排任务 → 收结果 → 重画」整条链能离线跑完。
    Discard,
}

/// 后台抓取的接线。
pub struct Workers {
    tx: Sender<Done>,
    rx: Receiver<Done>,
    backend: Backend,
}

impl Workers {
    pub fn new(online: Option<&Online>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            backend: match online {
                // **克隆**一份给线程：`App` 自己还要用它列书源名、按域名挑源。
                // 早先直接 take 走了，结果「打开搜索结果」那一步报「没有加载书源」。
                Some(o) => Backend::Real(Arc::new(Mutex::new(o.clone()))),
                None => Backend::NoSources,
            },
        }
    }

    /// 自测用：拿一个不真的抓东西的接线，外加一个能往结果通道里塞东西的发送端。
    pub fn for_test() -> (Self, Sender<Done>) {
        let (tx, rx) = mpsc::channel();
        let probe = tx.clone();
        (
            Self {
                tx,
                rx,
                backend: Backend::Discard,
            },
            probe,
        )
    }

    pub fn has_sources(&self) -> bool {
        matches!(self.backend, Backend::Real(_))
    }

    /// 有待办就丢一个线程出去，收到结果就喂回 `app`。每帧调一次。
    pub fn pump(&self, app: &mut App) {
        if let Some(job) = app.take_job() {
            match &self.backend {
                Backend::Real(o) => self.spawn(o.clone(), job),
                Backend::NoSources => app.on_done(Done::Failed("没有加载书源".into())),
                Backend::Discard => {
                    // 任务丢掉了，界面上的「在加载」也要跟着收起来 ——
                    // 否则自测里 busy 永远是 Some，驱动台会一直空等（实测一次 40 秒）
                    app.idle();
                }
            }
        }
        while let Ok(done) = self.rx.try_recv() {
            apply(app, done);
        }
    }

    fn spawn(&self, online: Arc<Mutex<Online>>, job: Job) {
        let tx = self.tx.clone();
        let started = std::thread::Builder::new()
            .stack_size(WORKER_STACK)
            .spawn(move || {
                let online = online.lock().unwrap_or_else(|e| e.into_inner());
                let mut last = String::new();
                let done = online.run(&job, &mut |msg| match msg {
                    Msg::Note(what) => {
                        // 进度只在变了的时候发，否则会把通道灌满
                        if what != last {
                            last = what.to_string();
                            let _ = tx.send(Done::Failed(format!("{PROGRESS_MARK}{what}")));
                        }
                    }
                    // 搜索的中间结果当场推给界面 —— 等全部源试完再给的话，
                    // 用户要对着「正在搜索」干等二十秒
                    Msg::Hits(hits) => {
                        let _ = tx.send(Done::More(hits.to_vec()));
                    }
                });
                let _ = tx.send(done);
            });
        if started.is_err() {
            // 起不了线程不该让界面卡死，明说就行
            let _ = self.tx.send(Done::Failed("起不了抓取线程".into()));
        }
    }
}

/// 把一条后台消息喂给 `app`，进度和真失败分开。
pub fn apply(app: &mut App, done: Done) {
    if let Done::Failed(msg) = &done {
        if let Some(what) = msg.strip_prefix(PROGRESS_MARK) {
            app.on_progress(what);
            return;
        }
    }
    app.on_done(done);
}

/// 事件循环。
///
/// * `draw` 每帧画一次；
/// * `next` 取下一个按键，`None` 表示这一轮没有输入（该收后台结果了），
///   `Err` 表示输入源坏了（比如 stdin 关了），要退出。
///   它拿到 `&mut App` 是为了能看一眼状态（脚本驱动要靠 `busy` 决定「还要不要再
///   等一会儿后台」）并在脚本跑完时**直接设 `quit`**。
///
///   收尾为什么不发一个假的 `q` 键：那会真的走一遍按键处理，把状态改掉 ——
///   比如 `Esc` 会清掉状态栏，于是「删掉了书源」这条断言就再也看不见了。
pub fn run<D, E>(app: &mut App, workers: &Workers, mut draw: D, mut next: E) -> io::Result<()>
where
    D: FnMut(&mut App) -> io::Result<()>,
    E: FnMut(&mut App) -> io::Result<Option<Key>>,
{
    // 清掉可能残留的退出标志。
    //
    // 它什么时候会是 true：自测里同一个 `App` 会被跑好几轮（改一下、再跑一轮），
    // 而上一轮的驱动台是**设 `app.quit` 来收尾**的。不清的话第二轮的第一次
    // 循环就会立刻返回 —— 表现是「脚本只执行了第一个键」，
    // 而报错看起来像是功能坏了，很难查到是测试台的问题。
    app.quit = false;
    // 单调时钟。**由循环喂给界面**（界面不自己读表），这样「过了多久」这件事
    // 在测试里是可控的 —— 隐蔽模式的「闲置就藏正文」全靠它。
    let started = std::time::Instant::now();

    loop {
        app.tick(started.elapsed().as_millis() as u64);
        workers.pump(app);
        draw(app)?;
        if let Some(key) = next(app)? {
            app.handle(key);
        }
        if app.quit {
            return Ok(());
        }
    }
}

// ── 自测驱动台 ──────────────────────────────────────────────────────────

/// 驱动台的一步：要么敲一个键，要么把一条后台消息塞进去。
///
/// 两者交替是必须的：抓取是异步的，「按回车 → 排任务 → 结果回来 → 重画」
/// 中间隔着好几次循环。只喂按键的话，测试会在结果回来之前就跑完了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Key(Key),
    Feed(Done),
    /// 干等一会儿。
    ///
    /// 脚本里必须有这个：`/关键字` 之后真站点要几秒才回来，脚本会在结果到达前
    /// 就把后面的键全按完。写脚本的人（包括自测）都会撞上。
    Wait(u64),
}

/// 按键 + 后台消息交替的驱动器。配 [`run`] 用。
pub struct Driver {
    steps: std::collections::VecDeque<Step>,
    tx: Sender<Done>,
    /// 队列空了之后还愿意等几轮（让 `pump` 有机会交付结果）
    idle: usize,
    /// 收尾过了没有。
    ///
    /// `run` 只在 `app.quit` 时返回，所以驱动器**必须**负责收尾 ——
    /// 否则脚本跑完就是死循环（第一版就是这么挂死的）。
    stopped: bool,
}

impl Driver {
    pub fn new(steps: Vec<Step>, tx: Sender<Done>) -> Self {
        Self {
            steps: steps.into(),
            tx,
            idle: 0,
            stopped: false,
        }
    }

    /// 从一段脚本起步（支持 `{wait:2000}`），之后用 [`Driver::feed`] 追加后台消息。
    pub fn script(script: &str, tx: Sender<Done>) -> Self {
        Self::new(parse_steps(script), tx)
    }

    pub fn feed(mut self, done: Done) -> Self {
        self.steps.push_back(Step::Feed(done));
        self
    }

    pub fn key(mut self, k: Key) -> Self {
        self.steps.push_back(Step::Key(k));
        self
    }

    pub fn keys(mut self, script: &str) -> Self {
        self.steps.extend(parse_steps(script));
        self
    }

    /// 还要喂几步。
    pub fn remaining(&self) -> usize {
        self.steps.len()
    }

    /// 交给 [`run`] 的取键函数。
    pub fn next(&mut self, app: &mut App) -> io::Result<Option<Key>> {
        match self.steps.pop_front() {
            Some(Step::Key(k)) => Ok(Some(k)),
            Some(Step::Feed(done)) => {
                let _ = self.tx.send(done);
                Ok(None)
            }
            Some(Step::Wait(ms)) => {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                Ok(None)
            }
            None => {
                // 队列空了再空转几轮，让后台把结果交回来。
                // 上限是防网络卡死 —— 自测里后台是假的，几轮就够。
                //
                // `has_pending_job` 也要看：`Done::Toc` 处理完会立刻排一个
                // `Job::Read`，而 `busy` 要到下一轮 `pump` 才置上，
                // 只看 `busy` 会在这个缝里判成空闲、提前收尾。
                if (app.busy.is_some() || app.has_pending_job()) && self.idle < 400 {
                    self.idle += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    return Ok(None);
                }
                // 直接设退出，**不按假键** —— 按键会真的走一遍处理逻辑改状态
                app.quit = true;
                self.stopped = true;
                Ok(None)
            }
        }
    }
}

// ── 按键脚本 ────────────────────────────────────────────────────────────

/// 解析脚本成 [`Step`]。`{wait}` / `{wait:2000}` 是等待，其余是按键。
pub fn parse_steps(s: &str) -> Vec<Step> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.extend(parse_one_key(c).into_iter().map(Step::Key));
            continue;
        }
        let mut name = String::new();
        for n in chars.by_ref() {
            if n == '}' {
                break;
            }
            name.push(n);
        }
        let lower = name.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("wait") {
            let ms = rest
                .trim_start_matches([':', '='])
                .trim()
                .parse()
                .unwrap_or(1000);
            out.push(Step::Wait(ms));
        } else if let Some(k) = named_key(&name) {
            out.push(Step::Key(k));
        } else if name.chars().count() == 1 {
            // `{S}` / `{/}` —— 单字符就按它自己。不加这条的话大写字母和标点
            // 会被静默丢掉（现象：脚本里写了 {S} 却没跳屏，以为功能坏了）
            let c = name.chars().next().expect("上面刚数过是 1 个字符");
            out.extend(parse_one_key(c).into_iter().map(Step::Key));
        } else {
            // 不认识的按键名字宁可炸出来，也不要静默跳过 ——
            // 静默跳过的现象是「脚本写了但没生效」，比报错难查得多
            panic!("未知按键 {{{name}}}（可用：enter esc tab btab bs del up down left right home end pgup pgdn ctrl-x wait:ms，或单字符）");
        }
    }
    out
}

/// 把一串脚本按键解析成 `Key` 序列。
///
/// 语法：普通字符就是字符键；`{名字}` 是特殊键，名字见 [`named_key`]；
/// `\\n` `\\t` `\\e` 分别是回车、Tab、Esc（在 shell 里写成真转义）。
///
/// 存在的理由是**自测**：`--script "g12\nnno"` 能把一条交互路径跑完并把
/// 每帧屏幕打出来，不用人盯着终端敲。
/// 只取按键、丢掉 `{wait:}` 的脚本解析（给只关心键序列的地方用）。
///
/// **必须走 [`parse_steps`]**，不能再抄一份 —— 之前这里是独立的第二个实现，
/// 于是「`{S}` 这种单字符键被静默丢掉」那个 bug 在 `parse_steps` 里修好了，
/// 这里是原样的，测试里写 `{m}` 一点反应都没有。
pub fn parse_script(s: &str) -> Vec<Key> {
    parse_steps(s)
        .into_iter()
        .filter_map(|st| match st {
            Step::Key(k) => Some(k),
            // 键序列里没有「等」和「喂结果」这两说；调用方自己处理
            Step::Wait(_) | Step::Feed(_) => None,
        })
        .collect()
}

/// 一个字符对应哪个键。
fn parse_one_key(c: char) -> Option<Key> {
    Some(match c {
        '\n' => Key::Enter,
        '\t' => Key::Tab,
        '\u{1b}' => Key::Esc,
        '\u{8}' | '\u{7f}' => Key::Backspace,
        // 控制字符：真终端里 Ctrl-X 送的就是这些字节，脚本里也照收
        c if ('\u{1}'..='\u{1a}').contains(&c) => Key::Ctrl((b'a' + (c as u8) - 1) as char),
        other => Key::Char(other),
    })
}

fn named_key(name: &str) -> Option<Key> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "ret" => Key::Enter,
        "esc" => Key::Esc,
        "tab" => Key::Tab,
        "btab" | "shift-tab" => Key::BackTab,
        "bs" | "backspace" => Key::Backspace,
        "del" | "delete" => Key::Delete,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pgup" => Key::PageUp,
        "pgdn" => Key::PageDown,
        // `{ctrl-c}` 这类：只支持一个字符
        other => {
            let rest = other.strip_prefix("ctrl-")?;
            let c = rest.chars().next()?;
            return Some(Key::Ctrl(c));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个脚本解析器**必须**是同一套规则。
    ///
    /// 之前它们是两份独立实现，「单字符键被静默丢掉」的 bug 只在一份里修了，
    /// 另一份照旧 —— 测试里写 `{m}` 毫无反应，查起来很费劲。
    #[test]
    fn both_script_parsers_agree() {
        // `{m}` 这种单字符键两边都要认
        assert_eq!(parse_script("{m}"), vec![Key::Char('m')]);
        assert_eq!(
            parse_steps("{m}"),
            vec![Step::Key(Key::Char('m'))],
            "parse_steps 也要认"
        );
        // 大写的、标点的
        assert_eq!(parse_script("{S}"), vec![Key::Char('S')]);
        assert_eq!(parse_script("{/}"), vec![Key::Char('/')]);
        // 具名键
        assert_eq!(parse_script("{enter}{tab}"), vec![Key::Enter, Key::Tab]);
        // `{wait:}` 是解析器的扩展，键序列里丢掉
        assert_eq!(
            parse_script("{enter}{wait:100}{esc}"),
            vec![Key::Enter, Key::Esc]
        );
        assert_eq!(parse_steps("{enter}{wait:100}").len(), 2);
    }

    #[test]
    fn script_parses_plain_and_named_keys() {
        assert_eq!(parse_script("np"), vec![Key::Char('n'), Key::Char('p')]);
        assert_eq!(parse_script("n\n"), vec![Key::Char('n'), Key::Enter]);
        assert_eq!(
            parse_script("{down}{up}{esc}"),
            vec![Key::Down, Key::Up, Key::Esc]
        );
        assert_eq!(parse_script("{ENTER}"), vec![Key::Enter]);
        assert_eq!(parse_script("{ctrl-c}"), vec![Key::Ctrl('c')]);
        // `{wait}` 不是键，不该出现在按键序列里
        assert_eq!(
            parse_script("a{wait:100}b"),
            vec![Key::Char('a'), Key::Char('b')]
        );
        assert_eq!(
            parse_steps("n{wait:100}p"),
            vec![
                Step::Key(Key::Char('n')),
                Step::Wait(100),
                Step::Key(Key::Char('p'))
            ]
        );
        assert_eq!(parse_steps("{wait}"), vec![Step::Wait(1000)], "默认 1 秒");
        assert_eq!(parse_steps("{WAIT:50}"), vec![Step::Wait(50)]);
        // 真终端里 Ctrl-U 就是一个 0x15 字节
        assert_eq!(parse_script("\u{15}"), vec![Key::Ctrl('u')]);
        assert_eq!(parse_script("\u{3}"), vec![Key::Ctrl('c')]);
        assert_eq!(parse_script(""), Vec::<Key>::new());
        // 认不出来的 `{...}` **炸出来**，不再静默丢掉。
        // 丢掉的现象是「脚本里写了却没生效」，比报错难查得多。
        // （多字符的名字也不能当成单字符键乱按 —— 那正是要拦住的。）
        let bad = std::panic::catch_unwind(|| parse_script("a{nonsense}b"));
        assert!(bad.is_err(), "未知按键名应当报错");
        // 没闭合的花括号要能吃完剩下的，不能死循环
        assert_eq!(parse_script("{down"), vec![Key::Down]);
    }

    #[test]
    fn script_covers_every_key_the_ui_documents() {
        // help 里写的每个键都要能被脚本表达出来，否则自测覆盖不到
        let k = |s: &str| parse_script(s);
        assert_eq!(k("n").len(), 1);
        assert_eq!(k("{pgdn}"), vec![Key::PageDown]);
        assert_eq!(k("{btab}"), vec![Key::BackTab]);
        assert_eq!(k("{bs}"), vec![Key::Backspace]);
        assert_eq!(k("{del}"), vec![Key::Delete]);
        assert_eq!(k("{home}"), vec![Key::Home]);
    }

    #[test]
    fn progress_messages_do_not_clear_the_busy_flag() {
        // 进度和「跑完了」走同一个通道。看着像细节，但搞反了的状态栏是
        // 「一直在转」或者「一下子就空了」，都很难查。
        use crate::app::App;
        use novel_core::state::Session;

        let dir = std::env::temp_dir().join("novel-workers-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = App::new(Session::open(
            dir.join("state.json"),
            dir.join("reader.novel"),
        ));

        let (w, tx) = Workers::for_test();
        app.on_progress("搜 A（1/3）");
        assert!(app.busy.is_some());

        tx.send(Done::Failed(format!("{PROGRESS_MARK}搜 B（2/3）")))
            .unwrap();
        w.pump(&mut app);
        assert_eq!(app.busy.as_deref(), Some("搜 B（2/3）"), "进度要更新");

        tx.send(Done::Search(Vec::new())).unwrap();
        w.pump(&mut app);
        assert!(app.busy.is_none(), "拿到结果要收起「在加载」");
    }

    #[test]
    fn jobs_without_sources_fail_loudly_instead_of_hanging() {
        use crate::app::{App, Key, Screen};
        use novel_core::source::BookSource;
        use novel_core::state::Session;

        let dir = std::env::temp_dir().join("novel-workers-nosrc");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = App::new(Session::open(
            dir.join("state.json"),
            dir.join("reader.novel"),
        ));
        // app 里有书源（所以会排任务），但 worker 一侧没有后端 ——
        // 这个组合下必须立刻给出交代，不能让人以为在加载
        let all =
            BookSource::parse_file(include_str!("../../novel-core/tests/fixtures/sources.json"))
                .unwrap();
        app.set_online(Online::from_sources(all));

        let w = Workers::new(None);
        assert!(!w.has_sources());

        app.go(Screen::Search);
        for c in "斗破".chars() {
            app.handle(Key::Char(c));
        }
        app.handle(Key::Enter);
        assert!(app.has_pending_job(), "应当排了搜索任务");
        w.pump(&mut app);
        assert!(app.status.contains("没有加载书源"), "{}", app.status);
    }
}
