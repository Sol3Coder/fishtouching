//! 目录缓存。
//!
//! 目录是「拉一次、之后很多次都要用」的东西：站点慢的时候每次打开一本书都要
//! 等好几秒，而目录一天也变不了几章。所以缓到本地，打开第二遍就是秒开。
//!
//! 缓存**不保证最新** —— 目录里的章节只会往后追加，所以旧目录不会错，只是
//! 可能少几章。超过 [`TOC_TTL_SECS`] 就重拉；用户按 `R` 也能强制重拉。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crawl::Chapter;
use crate::R;

/// 缓存放在 `.novel/` 下的哪个子目录。
pub const TOC_DIR: &str = "toc";

/// 缓存有效期。
///
/// 一章一章追更的人一周内肯定会按 `R` 重拉；而「打开一本几个月前看过的书」
/// 用一周前的目录完全够。设成 7 天是这两个场景的折中。
pub const TOC_TTL_SECS: u64 = 7 * 24 * 3600;

/// 一条目录缓存。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedToc {
    pub source: String,
    pub url: String,
    /// Unix 秒。
    pub fetched_at: u64,
    /// 书源目录规则的指纹 —— 规则改了，同一本书的旧目录未必还成立。
    pub rule_hash: u64,
    pub chapters: Vec<Chapter>,
}

impl CachedToc {
    /// 过期了没有。
    pub fn is_stale(&self, now: u64) -> bool {
        now.saturating_sub(self.fetched_at) >= TOC_TTL_SECS
    }
}

/// 现在几点（Unix 秒）。
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 书源规则的指纹。
///
/// 用**序列化后的规则文本**算，不用 `Debug` 输出 —— 后者会被随便一次重构改掉，
/// 于是所有人的缓存集体失效。用文本则只有真改了规则才变。
pub fn rule_hash(rule: &impl Serialize) -> u64 {
    let text = serde_json::to_string(rule).unwrap_or_default();
    fnv1a(&text)
}

/// FNV-1a。
///
/// 自己写 6 行而不是引 `md5`/`sha2`：这里只要「不同输入几乎不碰撞」，
/// 不要密码学强度。而且必须**跨版本稳定** —— 用 `DefaultHasher` 的话
/// 换个 rustc 哈希就变了，缓存全失效（无害，但很莫名）。
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 缓存文件名：书源名 + 书 URL + 规则指纹。
///
/// 带上规则指纹：改了目录规则之后，旧缓存不该再被命中（那会读到按旧规则
/// 切出来的章名/链接）。
pub fn key(source: &str, url: &str, rule_hash: u64) -> String {
    format!(
        "{:016x}",
        fnv1a(&format!("{source}\u{0}{url}\u{0}{rule_hash}"))
    )
}

/// 缓存目录（`<base>/toc`）。
///
/// **只调一次** —— `load`/`store` 收的就是这个函数的返回值，不再自己套一遍。
/// （之前两边各套一次，缓存写到了 `.novel/toc/toc/`：文件确实写了，
/// 但永远读不中，等于没有缓存。）
pub fn dir(base: &Path) -> PathBuf {
    base.join(TOC_DIR)
}

/// 读一条缓存。`toc_dir` 是 [`dir`] 的返回值。
///
/// 坏了、缺了都当没有 —— **不报错**，缓存不该让打开一本书失败。
pub fn load(toc_dir: &Path, key: &str) -> Option<CachedToc> {
    let text = std::fs::read_to_string(toc_dir.join(format!("{key}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写一条缓存。`toc_dir` 是 [`dir`] 的返回值。
///
/// 写失败只是「下次还得重拉」，不该影响这次阅读。
pub fn store(toc_dir: &Path, key: &str, toc: &CachedToc) -> R<()> {
    std::fs::create_dir_all(toc_dir)
        .map_err(|e| crate::Error::Net(format!("建不了缓存目录 {}：{e}", toc_dir.display())))?;
    let text = serde_json::to_string(toc)
        .map_err(|e| crate::Error::Net(format!("目录缓存序列化失败：{e}")))?;
    std::fs::write(toc_dir.join(format!("{key}.json")), text)
        .map_err(|e| crate::Error::Net(format!("写不了目录缓存：{e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("novel-cache-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn toc(n: usize) -> Vec<Chapter> {
        (1..=n)
            .map(|i| Chapter {
                title: format!("第 {i} 章"),
                url: format!("http://x/{i}.html"),
            })
            .collect()
    }

    #[test]
    fn round_trips() {
        let d = dir(&tmp("rt"));
        let k = key("鬼吹灯", "http://x/book/1", 42);
        assert!(load(&d, &k).is_none(), "还没存就该没有");

        let entry = CachedToc {
            source: "鬼吹灯".into(),
            url: "http://x/book/1".into(),
            fetched_at: now(),
            rule_hash: 42,
            chapters: toc(3),
        };
        store(&d, &k, &entry).unwrap();
        assert_eq!(load(&d, &k).unwrap(), entry);
    }

    #[test]
    fn the_key_separates_books_and_rules() {
        // 不同书不能撞；同一本改规则也要换键
        let a = key("源", "http://x/1", 1);
        let b = key("源", "http://x/2", 1);
        let c = key("源", "http://x/1", 2);
        let d = key("另一个源", "http://x/1", 1);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        // 同输入必须同输出（缓存要能跨进程命中）
        assert_eq!(a, key("源", "http://x/1", 1));
    }

    /// **回归测试**：`dir()` 只该被调一次。
    ///
    /// 之前 `load`/`store` 各自又套了一遍，缓存写到了 `.novel/toc/toc/` ——
    /// 文件确实写了、行为看着正常，但**永远读不中**，等于没有缓存。
    #[test]
    fn the_cache_dir_is_not_doubled() {
        let base = tmp("nodouble");
        let d = dir(&base);
        assert_eq!(d, base.join("toc"));
        let k = key("源", "http://x/1", 1);
        store(
            &d,
            &k,
            &CachedToc {
                source: "源".into(),
                url: "http://x/1".into(),
                fetched_at: now(),
                rule_hash: 1,
                chapters: toc(1),
            },
        )
        .expect("存得进去");
        assert!(load(&d, &k).is_some(), "存完必须读得回来");
        assert!(
            !base.join("toc").join("toc").exists(),
            "不能在 toc 里再建一个 toc"
        );
    }

    #[test]
    fn a_corrupt_cache_file_reads_as_missing() {
        let d = dir(&tmp("corrupt"));
        let k = key("源", "http://x/1", 1);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("{k}.json")), "{ 这不是 JSON").unwrap();
        assert!(load(&d, &k).is_none(), "坏缓存当没有，不能让打开书失败");
    }

    #[test]
    fn staleness_is_a_clock_not_a_flag() {
        let now = 1_000_000_000;
        let mut e = CachedToc {
            source: String::new(),
            url: String::new(),
            fetched_at: now,
            rule_hash: 0,
            chapters: vec![],
        };
        assert!(!e.is_stale(now));
        assert!(!e.is_stale(now + TOC_TTL_SECS - 1));
        assert!(e.is_stale(now + TOC_TTL_SECS));
        // 时间倒流（用户改了系统时间）不该算过期
        assert!(!e.is_stale(now - 10_000));
        e.fetched_at = now;
        assert!(!e.is_stale(now));
    }

    #[test]
    fn the_rule_hash_follows_the_rule_text() {
        let a = rule_hash(&serde_json::json!({"a": 1}));
        assert_eq!(
            a,
            rule_hash(&serde_json::json!({"a": 1})),
            "同规则必须同指纹"
        );
        assert_ne!(a, rule_hash(&serde_json::json!({"a": 2})));
    }
}
