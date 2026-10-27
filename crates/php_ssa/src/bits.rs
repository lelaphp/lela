//! `zend_bitset`: a set of small numbers in words of 64, as the DFG and
//! the SSA construction lay one down per block, each of one length.

/// A set over `0..len`, `zend_bitset` with its length carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bits {
    words: Vec<u64>,
}

impl Bits {
    /// The empty set over `0..len`.
    pub(crate) fn new(len: usize) -> Bits {
        Bits {
            words: vec![0; len.div_ceil(64)],
        }
    }

    pub(crate) fn incl(&mut self, n: usize) {
        if let Some(word) = self.words.get_mut(n / 64) {
            *word |= 1 << (n % 64);
        }
    }

    pub(crate) fn excl(&mut self, n: usize) {
        if let Some(word) = self.words.get_mut(n / 64) {
            *word &= !(1 << (n % 64));
        }
    }

    pub(crate) fn has(&self, n: usize) -> bool {
        self.words
            .get(n / 64)
            .is_some_and(|word| word & (1 << (n % 64)) != 0)
    }

    pub(crate) fn clear(&mut self) {
        self.words.fill(0);
    }

    /// `zend_bitset_union`: `self |= other`.
    pub(crate) fn union(&mut self, other: &Bits) {
        for (word, more) in self.words.iter_mut().zip(&other.words) {
            *word |= more;
        }
    }

    /// `zend_bitset_union_with_intersection(self, self, a, b)`:
    /// `self |= a & b`.
    pub(crate) fn union_with_intersection(&mut self, a: &Bits, b: &Bits) {
        for ((word, a), b) in self.words.iter_mut().zip(&a.words).zip(&b.words) {
            *word |= a & b;
        }
    }

    /// `zend_bitset_union_with_difference(self, a, b, c)`:
    /// `self = a | (b & !c)`.
    pub(crate) fn set_union_with_difference(&mut self, a: &Bits, b: &Bits, c: &Bits) {
        for (((word, a), b), c) in self
            .words
            .iter_mut()
            .zip(&a.words)
            .zip(&b.words)
            .zip(&c.words)
        {
            *word = a | (b & !c);
        }
    }

    /// `zend_bitset_subset(self, other)`: every member of `self` is one
    /// of `other`.
    pub(crate) fn is_subset(&self, other: &Bits) -> bool {
        self.words
            .iter()
            .zip(&other.words)
            .all(|(word, more)| word & !more == 0)
    }

    /// `zend_bitset_last`: the greatest member.
    pub(crate) fn last(&self) -> Option<usize> {
        let (at, word) = self
            .words
            .iter()
            .enumerate()
            .rev()
            .find(|(_, word)| **word != 0)?;
        Some(at * 64 + 63 - word.leading_zeros() as usize)
    }

    /// `zend_bitset_first`: the least member.
    pub(crate) fn first(&self) -> Option<usize> {
        let (at, word) = self
            .words
            .iter()
            .enumerate()
            .find(|(_, word)| **word != 0)?;
        Some(at * 64 + word.trailing_zeros() as usize)
    }

    /// The members, greatest first: `ZEND_BITSET_REVERSE_FOREACH`.
    pub(crate) fn rev_members(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().rev().flat_map(|(at, word)| {
            (0..64)
                .rev()
                .filter(move |bit| word & (1 << bit) != 0)
                .map(move |bit| at * 64 + bit)
        })
    }
}

/// `WHILE_WORKLIST`'s order: pass after pass over the set, each word
/// read once as the pass reaches it and its members popped lowest first,
/// until a pass pops none. A member added behind the pass, or into the
/// word it is in, waits for the next pass.
pub(crate) struct Passes {
    word: usize,
    snapshot: u64,
    base: usize,
    idle: bool,
}

impl Passes {
    pub(crate) fn new() -> Passes {
        Passes {
            word: 0,
            snapshot: 0,
            base: 0,
            idle: true,
        }
    }

    /// The next member, taken out of `set`.
    pub(crate) fn next(&mut self, set: &mut Bits) -> Option<usize> {
        while self.snapshot == 0 {
            if self.word >= set.words.len() {
                if self.idle {
                    return None;
                }
                self.idle = true;
                self.word = 0;
                continue;
            }
            self.snapshot = set.words.get(self.word).copied().unwrap_or(0);
            self.base = self.word * 64;
            self.word += 1;
        }
        let n = self.base + self.snapshot.trailing_zeros() as usize;
        self.snapshot &= self.snapshot - 1;
        set.excl(n);
        self.idle = false;
        Some(n)
    }
}

#[cfg(test)]
mod tests {
    use super::Bits;

    #[test]
    fn members_and_bounds() {
        let mut set = Bits::new(130);
        for n in [0, 63, 64, 129] {
            set.incl(n);
        }
        assert_eq!(set.first(), Some(0));
        assert_eq!(set.last(), Some(129));
        assert_eq!(set.rev_members().collect::<Vec<_>>(), [129, 64, 63, 0]);
        set.excl(129);
        assert_eq!(set.last(), Some(64));
        let mut other = Bits::new(130);
        other.incl(64);
        assert!(other.is_subset(&set));
        assert!(!set.is_subset(&other));
        let mut passes = super::Passes::new();
        let mut popped = Vec::new();
        while let Some(n) = passes.next(&mut set) {
            // 70 joins a word the pass has yet to read and is taken in it;
            // 1 joins the word it read, and waits for the next.
            if n == 63 {
                set.incl(70);
                set.incl(1);
            }
            popped.push(n);
        }
        assert_eq!(popped, [0, 63, 64, 70, 1]);
    }
}
