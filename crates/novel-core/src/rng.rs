//! 一个极小的确定性伪随机数发生器。
//!
//! 用来生成「看起来随机」的 TS 代码 —— 但**必须可复现**：同一章渲染出的文本
//! 每字节都一样，否则每次写文件都会让编辑器重载、也会让测试没法断言。
//! 所以种子取自（书名 + 章节标题），而不是时间或系统随机数。

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // xorshift 的状态不能为 0
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    /// 用一段文本做种子（FNV-1a）。
    pub fn from_text(parts: &[&str]) -> Self {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for p in parts {
            for b in p.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
            h ^= 0xff;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        Self::new(h)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `0..n`（n 为 0 时返回 0）。
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }

    /// `percent`% 的概率为真。
    pub fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_gives_the_same_sequence() {
        let mut a = Rng::from_text(&["夜色渐浓", "推开门"]);
        let mut b = Rng::from_text(&["夜色渐浓", "推开门"]);
        for _ in 0..64 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        // 换一个字就完全不同
        let mut c = Rng::from_text(&["夜色渐浓", "风灌了进来"]);
        let mut d = Rng::from_text(&["夜色渐浓", "推开门"]);
        assert_ne!(c.next_u64(), d.next_u64());
    }

    #[test]
    fn below_stays_in_range_and_covers_it() {
        let mut r = Rng::new(42);
        let mut seen = [false; 5];
        for _ in 0..500 {
            let v = r.below(5);
            assert!(v < 5);
            seen[v] = true;
        }
        assert!(seen.iter().all(|s| *s), "below(5) 应该覆盖到全部 5 个值");
        assert_eq!(r.below(0), 0, "below(0) 不该 panic");
    }

    #[test]
    fn pick_and_chance_behave() {
        let mut r = Rng::new(7);
        let xs = ["a", "b", "c"];
        for _ in 0..50 {
            assert!(xs.contains(r.pick(&xs)));
        }
        // chance(0) 永远假、chance(100) 永远真
        for _ in 0..50 {
            assert!(!r.chance(0));
            assert!(r.chance(100));
        }
    }

    #[test]
    fn zero_seed_is_not_degenerate() {
        let mut r = Rng::new(0);
        let a = r.next_u64();
        let b = r.next_u64();
        assert_ne!(a, 0);
        assert_ne!(a, b);
    }
}
