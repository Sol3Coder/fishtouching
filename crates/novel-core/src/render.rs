//! 把一章小说渲染成「一份看起来像真的 TypeScript 文件」。
//!
//! 两条规矩：
//! 1. 小说正文**只出现在 `//` 行注释里**；
//! 2. 其余全是**生成的 TS 代码** —— 随机挑模板、随机挑标识符，
//!    但种子来自（书名 + 章节标题），所以同一章渲染出来**逐字节一致**。
//!
//! 之所以要「随机」：用户要的是「打开一个标签页，里面是自动生成的示例 TS 代码，
//! 注释里是小说」。固定的模板看着像占位符，随机生成才像真的有人在写这个文件。
//!
//! 之所以要「确定」：每次写文件都会触发编辑器重载，如果内容每次都变，
//! 编辑器会一直在重载，测试也没法断言。

use std::collections::HashSet;

use crate::rng::Rng;

#[derive(Debug, Clone)]
pub struct Book {
    pub name: String,
    pub author: String,
    /// 书源名，纯展示。
    pub source: String,
    pub total_chapters: usize,
}

#[derive(Debug, Clone)]
pub struct Chapter {
    /// 1-based。
    pub index: usize,
    pub title: String,
    pub paragraphs: Vec<String>,
}

/// 渲染一章。返回的文件内容永远是语法上说得通的 TS。
pub fn render(book: &Book, chapter: &Chapter) -> String {
    let mut rng = Rng::from_text(&[&book.name, &chapter.title, &chapter.index.to_string()]);
    let mut names = Names::default();
    let mut lines: Vec<String> = Vec::new();

    lines.push("/**".to_string());
    lines.push(format!(" * 《{}》 —— {}", book.name, book.author));
    lines.push(format!(
        " * 第 {} / {} 章 · {}",
        chapter.index, book.total_chapters, chapter.title
    ));
    lines.push(format!(" * 源 {}", book.source));
    lines.push(" *".to_string());
    lines.push(" * 这个文件是生成的：正文在注释里，其余是示例代码。".to_string());
    lines.push(" * 阅读器是 novel-tui：搜索、选书、选章节都在它里面。".to_string());
    lines.push(" */".to_string());
    lines.push(String::new());

    // 开场先来一块代码，让文件一打开就像代码文件
    let mut prev = -1;
    block(&mut rng, &mut names, &mut lines, true, &mut prev);

    for (i, p) in chapter.paragraphs.iter().enumerate() {
        // 段与段之间大概率插一块代码
        if i > 0 && rng.chance(75) {
            lines.push(String::new());
            block(&mut rng, &mut names, &mut lines, false, &mut prev);
        }
        lines.push(String::new());
        for line in p.lines() {
            if line.is_empty() {
                lines.push("//".to_string());
            } else {
                lines.push(format!("// {line}"));
            }
        }
    }

    // 收尾：再补 1~3 块，然后一条本章元信息
    let tail = 1 + rng.below(3);
    for _ in 0..tail {
        lines.push(String::new());
        block(&mut rng, &mut names, &mut lines, false, &mut prev);
    }

    lines.push(String::new());
    lines.push("// ── 本章完 ──".to_string());
    lines.push(String::new());
    lines.push("export interface ChapterRef {".to_string());
    lines.push("  index: number;".to_string());
    lines.push("  title: string;".to_string());
    lines.push("}".to_string());
    lines.push(String::new());
    lines.push("export const chapter: ChapterRef = {".to_string());
    lines.push(format!("  index: {},", chapter.index));
    lines.push(format!("  title: {:?},", chapter.title));
    lines.push("};".to_string());

    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// 已用过的标识符，保证不重复声明（重复了 TS 直接编译不过）。
#[derive(Default)]
struct Names {
    used: HashSet<String>,
}

impl Names {
    /// 摇一个没被用过的名字，形如 `resolveChapter`。
    fn take(&mut self, rng: &mut Rng, noun: &str) -> String {
        for _ in 0..64 {
            let verb = *rng.pick(VERBS);
            let name = format!("{verb}{noun}");
            if self.used.insert(name.clone()) {
                return name;
            }
        }
        // 兜底：加序号（正常走不到，池子够大）
        let mut n = 2;
        loop {
            let name = format!("{noun}{n}");
            if self.used.insert(name.clone()) {
                return name;
            }
            n += 1;
        }
    }

    fn noun(&mut self, rng: &mut Rng) -> String {
        let base = *rng.pick(NOUNS);
        let mut c = base.chars();
        let head = c.next().map(|h| h.to_ascii_uppercase()).unwrap_or('X');
        format!("{head}{}", c.as_str())
    }
}

const NOUNS: &[&str] = &[
    "chapter", "source", "book", "cache", "reader", "toc", "page", "rule", "query", "sync",
    "store", "layout", "token", "frame", "buffer", "cursor", "anchor", "syntax", "digest",
    "window",
];

const VERBS: &[&str] = &[
    "load",
    "resolve",
    "parse",
    "apply",
    "flush",
    "fetch",
    "render",
    "merge",
    "scan",
    "commit",
    "encode",
    "decode",
    "collect",
    "normalize",
    "expand",
    "shrink",
    "match",
    "split",
    "join",
    "trim",
    "measure",
    "advance",
];

/// 随机挑一个代码块输出。`prev` 记着上一块是哪一种，避免连着出两块一样的
/// （否则会看到两个几乎相同的 class，一眼假）。
fn block(rng: &mut Rng, names: &mut Names, out: &mut Vec<String>, first: bool, prev: &mut i32) {
    let n = if first { 3 } else { 7 };
    let mut kind = rng.below(n);
    if kind as i32 == *prev && n > 1 {
        kind = (kind + 1) % n;
    }
    *prev = kind as i32;
    match kind {
        0 => interface(rng, names, out),
        1 => constants(rng, names, out),
        2 => function(rng, names, out),
        3 => arrow(rng, names, out),
        4 => async_function(rng, names, out),
        5 => class_block(rng, names, out),
        _ => union_type(rng, names, out),
    }
}

fn interface(rng: &mut Rng, names: &mut Names, out: &mut Vec<String>) {
    let noun = names.noun(rng);
    let suffix = *rng.pick(&["Config", "State", "Options", "Result"]);
    let fields: &[(&str, &str)] = &[
        ("index", "number"),
        ("title", "string"),
        ("enabled", "boolean"),
        ("retry", "number"),
        ("tags", "string[]"),
        ("createdAt", "number"),
    ];
    let n = 2 + rng.below(4);
    out.push(format!("export interface {noun}{suffix} {{"));
    let mut picked = HashSet::new();
    for _ in 0..n {
        let (name, ty) = rng.pick(fields);
        if !picked.insert(*name) {
            continue;
        }
        let optional = if rng.chance(25) { "?" } else { "" };
        out.push(format!("  {name}{optional}: {ty};"));
    }
    out.push("}".to_string());
}

fn constants(rng: &mut Rng, names: &mut Names, out: &mut Vec<String>) {
    let noun = names.noun(rng);
    let name = format!("DEFAULT_{}", noun.to_uppercase());
    let keys = [
        "retry",
        "timeoutMs",
        "prefetch",
        "pageSize",
        "batch",
        "depth",
    ];
    let n = 2 + rng.below(4);
    out.push(format!("export const {name} = {{"));
    let mut picked = HashSet::new();
    for _ in 0..n {
        let k = *rng.pick(&keys);
        if !picked.insert(k) {
            continue;
        }
        out.push(format!("  {k}: {},", 1 + rng.below(64)));
    }
    out.push("} as const;".to_string());
}

fn function(rng: &mut Rng, names: &mut Names, out: &mut Vec<String>) {
    let noun = names.noun(rng);
    let name = names.take(rng, &noun);
    let param = *rng.pick(NOUNS);
    out.push(format!(
        "export function {name}({param}: string): string {{"
    ));
    match rng.below(4) {
        0 => {
            out.push(format!("  const trimmed = {param}.trim();"));
            out.push("  return trimmed.length > 0 ? trimmed : \"\";".to_string());
        }
        1 => {
            out.push(format!("  if (!{param}) return \"\";"));
            out.push(format!("  return {param}.normalize(\"NFC\");"));
        }
        2 => {
            out.push(format!("  const parts = {param}.split(/\\s+/);"));
            out.push("  return parts.filter(Boolean).join(\" \");".to_string());
        }
        _ => {
            out.push("  let acc = \"\";".to_string());
            out.push(format!("  for (const ch of {param}) acc = ch + acc;"));
            out.push("  return acc;".to_string());
        }
    }
    out.push("}".to_string());
}

fn arrow(rng: &mut Rng, names: &mut Names, out: &mut Vec<String>) {
    let noun = names.noun(rng);
    let name = names.take(rng, &noun);
    let lo = rng.below(4);
    let hi = lo + 4 + rng.below(20);
    out.push(format!(
        "export const {name} = (n: number): number => Math.max({lo}, Math.min(n, {hi}));"
    ));
}

fn async_function(rng: &mut Rng, names: &mut Names, out: &mut Vec<String>) {
    let noun = names.noun(rng);
    let name = names.take(rng, &noun);
    let param = *rng.pick(NOUNS);
    out.push(format!(
        "export async function {name}({param}: string): Promise<string> {{"
    ));
    match rng.below(3) {
        0 => {
            out.push(format!("  const res = await fetch({param});"));
            out.push("  return res.text();".to_string());
        }
        1 => {
            out.push(format!("  const res = await fetch({param});"));
            out.push("  if (!res.ok) throw new Error(String(res.status));".to_string());
            out.push("  return res.text();".to_string());
        }
        _ => {
            out.push(format!("  const cached = await caches.open(\"{param}\");"));
            out.push("  const hit = await cached.match(\"/\");".to_string());
            out.push("  return hit ? hit.text() : \"\";".to_string());
        }
    }
    out.push("}".to_string());
}

fn class_block(rng: &mut Rng, names: &mut Names, out: &mut Vec<String>) {
    let noun = names.noun(rng);
    let name = format!("{noun}Cursor");
    let field = *rng.pick(&["offset", "depth", "cursor", "head"]);
    let by = *rng.pick(&["by", "delta", "step"]);
    out.push(format!("export class {name} {{"));
    out.push(format!("  private {field} = 0;"));
    out.push(String::new());
    out.push(format!("  advance({by}: number): void {{"));
    out.push(format!("    this.{field} += {by};"));
    out.push("  }".to_string());
    out.push(String::new());
    out.push("  get position(): number {".to_string());
    out.push(format!("    return this.{field};"));
    out.push("  }".to_string());
    out.push("}".to_string());
}

fn union_type(rng: &mut Rng, names: &mut Names, out: &mut Vec<String>) {
    let noun = names.noun(rng);
    let name = format!("{noun}Phase");
    let variants = ["Idle", "Loading", "Ready", "Failed"];
    let n = 2 + rng.below(3);
    out.push(format!("export enum {name} {{"));
    for v in variants.iter().take(n) {
        out.push(format!("  {v} = {:?},", v.to_lowercase()));
    }
    out.push("}".to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> Book {
        Book {
            name: "夜色渐浓".into(),
            author: "示例作者".into(),
            source: "演示源·站点A".into(),
            total_chapters: 3,
        }
    }

    fn chapter(index: usize) -> Chapter {
        Chapter {
            index,
            title: format!("标题{index}"),
            paragraphs: vec![
                "他推开门，楼道里的声控灯亮了一下。".into(),
                "外面在下雨。雨点打在铁皮雨棚上。\n像有人在楼上挪椅子。".into(),
            ],
        }
    }

    #[test]
    fn is_deterministic_for_the_same_chapter() {
        assert_eq!(render(&book(), &chapter(2)), render(&book(), &chapter(2)));
        // 不同章不同，换书名也不同
        assert_ne!(render(&book(), &chapter(1)), render(&book(), &chapter(2)));
        let mut other = book();
        other.name = "长夜将尽".into();
        assert_ne!(render(&book(), &chapter(2)), render(&other, &chapter(2)));
    }

    #[test]
    fn novel_text_only_appears_in_line_comments() {
        let text = render(&book(), &chapter(1));
        for line in [
            "他推开门，楼道里的声控灯亮了一下。",
            "外面在下雨。雨点打在铁皮雨棚上。",
            "像有人在楼上挪椅子。",
        ] {
            assert!(text.contains(&format!("// {line}")), "缺注释正文 {line:?}");
            // 不能在注释之外的任何地方出现
            for raw in text.lines().filter(|l| !l.trim_start().starts_with("//")) {
                assert!(
                    !raw.contains(line),
                    "正文漏到代码里了: {raw:?}\n---\n{text}"
                );
            }
        }
    }

    #[test]
    fn declares_everything_once() {
        // 重复声明在 TS 里直接编译不过，所以每个 `export`/`private` 名字都要唯一
        for index in 1..=3 {
            let text = render(&book(), &chapter(index));
            let mut seen = HashSet::new();
            for line in text.lines() {
                let name = line
                    .strip_prefix("export interface ")
                    .or_else(|| line.strip_prefix("export class "))
                    .or_else(|| line.strip_prefix("export enum "))
                    .or_else(|| line.strip_prefix("export const "))
                    .or_else(|| line.strip_prefix("export function "))
                    .or_else(|| line.strip_prefix("export async function "))
                    .or_else(|| line.strip_prefix("export type "))
                    .map(|rest| {
                        rest.split(|c: char| !c.is_alphanumeric() && c != '_')
                            .next()
                            .unwrap_or("")
                            .to_string()
                    });
                if let Some(n) = name {
                    assert!(!n.is_empty(), "空声明名: {line:?}");
                    assert!(seen.insert(n.clone()), "重复声明 {n} 在第 {index} 章");
                }
            }
            assert!(
                seen.len() >= 4,
                "第 {index} 章只生成了 {} 个声明",
                seen.len()
            );
        }
    }

    #[test]
    fn produces_a_plausible_code_file() {
        let text = render(&book(), &chapter(1));
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() > 20, "文件太短了：{} 行", lines.len());
        assert!(text.starts_with("/**"), "要以文件头注释开头");
        assert!(text.ends_with("};\n"), "要以声明收尾");
        // 代码行占多数（这是个代码文件，不是纯注释）
        let code = lines
            .iter()
            .filter(|l| !l.trim_start().starts_with("//") && !l.trim().is_empty())
            .count();
        assert!(code >= 8, "代码行只有 {code} 行:\n{text}");
        // 括号配平
        assert_eq!(
            text.matches('{').count(),
            text.matches('}').count(),
            "花括号不配平:\n{text}"
        );
        // 章号在头部和尾部都写到了
        assert!(text.contains("第 1 / 3 章 · 标题1"));
        assert!(text.contains("index: 1,"));
    }

    #[test]
    fn never_emits_two_identical_blocks_in_a_row() {
        // 用文本前几行当块的指纹；相邻两块指纹相同就说明挑重了
        for index in 1..=3 {
            let text = render(&book(), &chapter(index));
            let mut blocks: Vec<String> = Vec::new();
            let mut cur: Vec<&str> = Vec::new();
            for line in text.lines() {
                let code = !line.trim_start().starts_with("//") && !line.trim().is_empty();
                let decl_start = line.starts_with("export ");
                if code && (decl_start || line.starts_with("  ")) {
                    cur.push(line);
                } else if !cur.is_empty() {
                    blocks.push(cur.join(
                        "
",
                    ));
                    cur.clear();
                }
            }
            if !cur.is_empty() {
                blocks.push(cur.join(
                    "
",
                ));
            }
            // 指纹只取第一行（声明行），够区分块种类
            let first: Vec<&str> = blocks.iter().filter_map(|b| b.lines().next()).collect();
            for w in first.windows(2) {
                assert_ne!(
                    w[0], w[1],
                    "第 {index} 章连着两块一样:
{text}"
                );
            }
        }
    }

    #[test]
    fn names_read_like_identifiers() {
        // 别把两个词硬拼成 `normalizeclampToken` 这种
        for index in 1..=3 {
            let text = render(&book(), &chapter(index));
            for line in text.lines() {
                if let Some(rest) = line
                    .strip_prefix("export const ")
                    .or_else(|| line.strip_prefix("export function "))
                    .or_else(|| line.strip_prefix("export async function "))
                {
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    assert!(!name.is_empty());
                    // 出现连续两个小写开头的大写字母段（如 ...clampToken 前一个是小写结尾）
                    let lower_then_upper = name
                        .as_bytes()
                        .windows(2)
                        .any(|w| w[0].is_ascii_lowercase() && w[1].is_ascii_uppercase());
                    if lower_then_upper {
                        // 允许 camelCase，但整名不该超过 26 个字符
                        assert!(name.len() <= 26, "名字太长，像是硬拼的: {name}");
                    }
                }
            }
        }
    }

    #[test]
    fn handles_empty_and_odd_paragraphs() {
        let mut c = chapter(1);
        c.paragraphs = vec!["".into(), "只有一段。".into()];
        let text = render(&book(), &c);
        assert!(text.contains("// 只有一段。"));
        assert_eq!(text.matches('{').count(), text.matches('}').count());

        // 0 段也不该崩
        c.paragraphs = vec![];
        let text = render(&book(), &c);
        assert!(text.contains("export const chapter: ChapterRef"));
    }
}
