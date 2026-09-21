//! `zend_sort` (`Z/Zend/zend_sort.c`), written once: the hybrid every
//! `zend_hash_sort` runs, and the one a native runs over an index table
//! of its own. The order it leaves a stretch of equal elements in, and
//! the order it leaves anything in under a comparison that is not
//! transitive, is php's for the same input, which is what `sort()`
//! over mixed types and `array_unique` under `SORT_REGULAR` observe; a
//! stable sort would leave another order and a different set.

use std::cmp::Ordering;

/// `zend_sort` (`Z/Zend/zend_sort.c:248`), libc++'s hybrid as php
/// carries it: insertion sort at sixteen elements or fewer, else a
/// pivot chosen by a three-element network, five past a thousand,
/// swapped to the second slot and partitioned, the smaller side
/// recursed and the larger looped. The element order this leaves is
/// php's for any comparison, transitive or not: a `usort` comparator
/// answering `1` reads every pair as greater and leaves the walk alone,
/// here past a thousand elements where the pivot is the five-element
/// network's, and `sort()` under `SORT_REGULAR` over twenty mixed
/// values takes the partition over `zend_compare`'s answers, which are
/// not transitive there.
///
/// ```php
/// $big = []; for ($i = 0; $i < 1100; $i++) $big[] = $i; usort($big, fn($a, $b) => 1); $sum = 0; foreach ($big as $at => $v) $sum += $at * $v; echo json_encode([$big[0], $big[1], $big[16], $big[550], $big[1099], $sum]), "\n";
/// $mixed = [1, "1", 2, 2.0, "a", "A", null, "", false, true, "0", 0, "b", 3, "3", "c", "B", 0.0, " 1", "1 "]; sort($mixed); echo json_encode($mixed), "\n";
/// ```
/// ```expect
/// [960,832,1088,551,0,368635105]
/// [null,"",false,"0",0,0,1,"1"," 1","1 ",2,2,3,"3","A","a",true,"B","b","c"]
/// ```
pub fn zend_sort<T>(v: &mut [T], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    let mut base = 0;
    let mut n = v.len();
    loop {
        if n <= 16 {
            insert_sort(&mut v[base..base + n], cmp);
            return;
        }
        let start = base;
        let end = base + n;
        let offset = n >> 1;
        let mut pivot = start + offset;
        if n >> 10 != 0 {
            let delta = offset >> 1;
            sort5(
                v,
                [start, start + delta, pivot, pivot + delta, end - 1],
                cmp,
            );
        } else {
            sort3(v, [start, pivot, end - 1], cmp);
        }
        v.swap(start + 1, pivot);
        pivot = start + 1;
        let mut i = pivot + 1;
        let mut j = end - 1;
        'partition: loop {
            while greater(v, pivot, i, cmp) {
                i += 1;
                if i == j {
                    break 'partition;
                }
            }
            j -= 1;
            if j == i {
                break 'partition;
            }
            while greater(v, j, pivot, cmp) {
                j -= 1;
                if j == i {
                    break 'partition;
                }
            }
            v.swap(i, j);
            i += 1;
            if i == j {
                break 'partition;
            }
        }
        v.swap(pivot, i - 1);
        if (i - 1) - start < end - i {
            zend_sort(&mut v[start..i - 1], cmp);
            base = i;
            n = end - i;
        } else {
            zend_sort(&mut v[i..end], cmp);
            n = i - start - 1;
        }
    }
}

/// `cmp(a, b) > 0`.
fn greater<T>(v: &[T], a: usize, b: usize, cmp: &mut dyn FnMut(&T, &T) -> Ordering) -> bool {
    cmp(&v[a], &v[b]) == Ordering::Greater
}

/// `zend_insert_sort` (`Z/Zend/zend_sort.c:84`): the networks up to
/// five elements, and past them an insertion sort that walks the first
/// six back one slot at a time and the rest two at a time.
fn insert_sort<T>(v: &mut [T], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    match v.len() {
        0 | 1 => {}
        2 => sort2(v, 0, 1, cmp),
        3 => sort3(v, [0, 1, 2], cmp),
        4 => sort4(v, [0, 1, 2, 3], cmp),
        5 => sort5(v, [0, 1, 2, 3, 4], cmp),
        len => {
            let sentry = 6;
            for i in 1..sentry {
                let mut j = i - 1;
                if !greater(v, j, i, cmp) {
                    continue;
                }
                while j != 0 {
                    j -= 1;
                    if !greater(v, j, i, cmp) {
                        j += 1;
                        break;
                    }
                }
                slide(v, i, j);
            }
            for i in sentry..len {
                let mut j = i - 1;
                if !greater(v, j, i, cmp) {
                    continue;
                }
                loop {
                    j -= 2;
                    if !greater(v, j, i, cmp) {
                        j += 1;
                        if !greater(v, j, i, cmp) {
                            j += 1;
                        }
                        break;
                    }
                    if j == 0 {
                        break;
                    }
                    if j == 1 {
                        j -= 1;
                        if greater(v, i, j, cmp) {
                            j += 1;
                        }
                        break;
                    }
                }
                slide(v, i, j);
            }
        }
    }
}

/// The swap loop that moves the element at `from` down to `to`.
fn slide<T>(v: &mut [T], from: usize, to: usize) {
    let mut k = from;
    while k > to {
        v.swap(k, k - 1);
        k -= 1;
    }
}

/// `zend_sort_2`.
fn sort2<T>(v: &mut [T], a: usize, b: usize, cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    if greater(v, a, b, cmp) {
        v.swap(a, b);
    }
}

/// `zend_sort_3`.
fn sort3<T>(v: &mut [T], [a, b, c]: [usize; 3], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    if !greater(v, a, b, cmp) {
        if !greater(v, b, c, cmp) {
            return;
        }
        v.swap(b, c);
        if greater(v, a, b, cmp) {
            v.swap(a, b);
        }
        return;
    }
    if !greater(v, c, b, cmp) {
        v.swap(a, c);
        return;
    }
    v.swap(a, b);
    if greater(v, b, c, cmp) {
        v.swap(b, c);
    }
}

/// `zend_sort_4`, over the four positions `at`.
fn sort4<T>(v: &mut [T], at: [usize; 4], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    sort3(v, [at[0], at[1], at[2]], cmp);
    if greater(v, at[2], at[3], cmp) {
        v.swap(at[2], at[3]);
        if greater(v, at[1], at[2], cmp) {
            v.swap(at[1], at[2]);
            if greater(v, at[0], at[1], cmp) {
                v.swap(at[0], at[1]);
            }
        }
    }
}

/// `zend_sort_5`, over the five positions `at`.
fn sort5<T>(v: &mut [T], at: [usize; 5], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    sort4(v, [at[0], at[1], at[2], at[3]], cmp);
    if greater(v, at[3], at[4], cmp) {
        v.swap(at[3], at[4]);
        if greater(v, at[2], at[3], cmp) {
            v.swap(at[2], at[3]);
            if greater(v, at[1], at[2], cmp) {
                v.swap(at[1], at[2]);
                if greater(v, at[0], at[1], cmp) {
                    v.swap(at[0], at[1]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::zend_sort;

    /// A transitive comparison sorts, whichever arm the length takes:
    /// the networks, the insertion sort and the partition.
    #[test]
    fn every_arm_sorts_a_transitive_comparison() {
        for len in [0usize, 1, 2, 3, 4, 5, 6, 7, 16, 17, 40, 1025] {
            let mut v: Vec<usize> = (0..len).map(|i| (i * 7919) % (len.max(1) * 3)).collect();
            let mut sorted = v.clone();
            sorted.sort_unstable();
            zend_sort(&mut v, &mut |a, b| a.cmp(b));
            assert_eq!(v, sorted, "len {len}");
        }
    }
}
