//! 在真实终端之外把界面画成文本，方便截图、对比、贴进 issue。
//!
//! ```text
//! cargo run --example preview                 # 初始界面
//! cargo run --example preview -- "/夜"        # 边打边筛的搜索
//! cargo run --example preview -- "g3" ENTER   # 跳章（ENTER/TAB/ESC 是特殊键）
//! ```

use novel_core::state::Session;
use novel_tui::app::{App, Key};
use novel_tui::ui;

fn main() {
    let dir = std::env::temp_dir().join("novel-preview");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let mut app = App::new(Session::open(
        dir.join("state.json"),
        dir.join("reader.novel"),
    ));

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "ENTER" => app.handle(Key::Enter),
            "TAB" => app.handle(Key::Tab),
            "ESC" => app.handle(Key::Esc),
            _ => {
                for c in arg.chars() {
                    app.handle(Key::Char(c));
                }
            }
        }
    }

    println!("{}", ui::render_to_text(&mut app, 110, 34));
}
