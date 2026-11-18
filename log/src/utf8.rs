//! Whether slices of one text heap are UTF-8, at a cost linear in the heap however many rows name it (Q-651).

/// Answers `std::str::from_utf8(&heap[a..b]).is_ok()` for many slices of one heap: directly until the bytes checked
/// that way pass the heap's length, then from a table built once in one pass over the heap.
#[derive(Debug, Default)]
pub struct HeapText {
    direct: usize,
    table: Option<Table>,
}

/// Per heap byte: `bad`, a byte no valid UTF-8 slice may hold (an invalid lead, or a continuation byte outside any
/// valid character), and `inside`, a continuation byte of a valid character. A slice is UTF-8 exactly when it holds
/// no bad byte and neither its start nor its end falls inside a character.
#[derive(Debug)]
struct Table {
    bad: Vec<u64>,
    inside: Vec<u64>,
    /// Bad bytes before each 64-byte word.
    bad_before: Vec<u32>,
}

fn bit(words: &[u64], i: usize) -> bool {
    words.get(i / 64).is_some_and(|w| w >> (i % 64) & 1 == 1)
}

fn set(words: &mut [u64], i: usize) {
    words[i / 64] |= 1 << (i % 64);
}

impl Table {
    fn new(heap: &[u8]) -> Self {
        let words = heap.len() / 64 + 1;
        let mut t = Table {
            bad: vec![0; words],
            inside: vec![0; words],
            bad_before: Vec::with_capacity(words + 1),
        };
        let mut i = 0;
        while i < heap.len() {
            let b = heap[i];
            let len = match b {
                0x00..=0x7F => 1,
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => 0,
            };
            let valid = len > 0
                && heap
                    .get(i..i + len)
                    .is_some_and(|c| std::str::from_utf8(c).is_ok());
            if valid {
                for j in i + 1..i + len {
                    set(&mut t.inside, j);
                }
                i += len;
            } else {
                set(&mut t.bad, i);
                i += 1;
            }
        }
        let mut n = 0u32;
        for w in &t.bad {
            t.bad_before.push(n);
            n += w.count_ones();
        }
        t.bad_before.push(n);
        t
    }

    fn bad_below(&self, i: usize) -> u32 {
        let (w, r) = (i / 64, i % 64);
        let partial = self
            .bad
            .get(w)
            .map_or(0, |x| (x & ((1u64 << r) - 1)).count_ones());
        self.bad_before[w] + partial
    }

    fn is_utf8(&self, a: usize, b: usize) -> bool {
        a == b
            || (self.bad_below(b) == self.bad_below(a)
                && !bit(&self.inside, a)
                && !bit(&self.inside, b))
    }
}

impl HeapText {
    /// Whether `heap[a..b]` is UTF-8; `a ≤ b ≤ heap.len()`, and `heap` is the same on every call.
    pub fn is_utf8(&mut self, heap: &[u8], a: usize, b: usize) -> bool {
        if self.table.is_none() && self.direct + (b - a) <= heap.len() {
            self.direct += b - a;
            return std::str::from_utf8(&heap[a..b]).is_ok();
        }
        self.table
            .get_or_insert_with(|| Table::new(heap))
            .is_utf8(a, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_checks_stop_at_the_heaps_length() {
        let heap = "€".repeat(85).into_bytes();
        let mut h = HeapText::default();
        assert!(h.is_utf8(&heap, 0, 255));
        assert!(h.table.is_none());
        assert!(h.is_utf8(&heap, 3, 255));
        assert!(h.table.is_some());
        assert!(!h.is_utf8(&heap, 1, 255));
        assert_eq!(h.direct, 255);
    }
}
