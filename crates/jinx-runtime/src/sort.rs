// builtins.sort, as Nix 2.27 and later implement it: a stable peeksort (a
// natural mergesort) with insertion sort for short ranges, from
// src/libutil/include/nix/util/sort.hh. nixpkgs has comparators that are
// not strict weak orderings (`a: b: true`, `<=`), and for those the exact
// sequence of comparisons decides the output, so this file transcribes the
// C++ statement by statement.
//
// Indices stand in for iterators: a half-open range [begin, end) over
// `items`. The comparator can fail (it is Nix code), and the first error
// stops the sort.

use crate::value::Result;

const INSERTION_SORT_THRESHOLD: usize = 16;

struct Sorter<'a, T, F> {
    items: &'a mut [T],
    buffer: Vec<T>,
    less: F,
}

impl<T: Clone, F: FnMut(&T, &T) -> Result<bool>> Sorter<'_, T, F> {
    fn less(&mut self, a: usize, b: usize) -> Result<bool> {
        (self.less)(&self.items[a], &self.items[b])
    }

    // Merge the sorted runs [begin, middle) and [middle, end) in place
    // through the buffer. Stable: when neither side is less than the
    // other, the left wins.
    fn merge(&mut self, begin: usize, middle: usize, end: usize) -> Result<()> {
        self.buffer.clear();
        self.buffer.extend_from_slice(&self.items[begin..end]);
        let wm = middle - begin;
        let we = end - begin;
        let (mut out, mut left, mut right) = (begin, 0, wm);
        while left != wm && right != we {
            let take_left = !(self.less)(&self.buffer[right], &self.buffer[left])?;
            if take_left {
                self.items[out] = self.buffer[left].clone();
                left += 1;
            } else {
                self.items[out] = self.buffer[right].clone();
                right += 1;
            }
            out += 1;
        }
        while left != wm {
            self.items[out] = self.buffer[left].clone();
            left += 1;
            out += 1;
        }
        while right != we {
            self.items[out] = self.buffer[right].clone();
            right += 1;
            out += 1;
        }
        Ok(())
    }

    fn insertion_sort(&mut self, begin: usize, end: usize) -> Result<()> {
        if begin == end {
            return Ok(());
        }
        for current in begin + 1..end {
            let mut point = current;
            while point != begin && self.less(point, point - 1)? {
                self.items.swap(point, point - 1);
                point -= 1;
            }
        }
        Ok(())
    }

    // `negate` turns the strictly decreasing searches into the weakly
    // increasing ones, as the C++ does with std::not_fn.
    fn cmp(&mut self, a: usize, b: usize, negate: bool) -> Result<bool> {
        Ok(self.less(a, b)? != negate)
    }

    // Maximal i <= end such that [begin, i) is strictly decreasing.
    fn decreasing_prefix(&mut self, mut begin: usize, end: usize, negate: bool) -> Result<usize> {
        if begin == end {
            return Ok(begin);
        }
        while begin + 1 != end && self.cmp(begin + 1, begin, negate)? {
            begin += 1;
        }
        Ok(begin + 1)
    }

    // Minimal i >= begin such that [i, end) is strictly decreasing.
    fn decreasing_suffix(&mut self, begin: usize, mut end: usize, negate: bool) -> Result<usize> {
        if begin == end {
            return Ok(end);
        }
        while end - 1 > begin && self.cmp(end - 1, end - 2, negate)? {
            end -= 1;
        }
        Ok(end - 1)
    }

    fn peeksort(
        &mut self,
        begin: usize,
        end: usize,
        left_run_end: usize,
        right_run_begin: usize,
    ) -> Result<()> {
        if left_run_end == end || right_run_begin == begin {
            return Ok(());
        }
        let length = end - begin;
        if length <= INSERTION_SORT_THRESHOLD {
            return self.insertion_sort(begin, end);
        }
        let middle = begin + length / 2;
        if middle <= left_run_end {
            self.peeksort(left_run_end, end, left_run_end + 1, right_run_begin)?;
            return self.merge(begin, left_run_end, end);
        }
        if middle >= right_run_begin {
            self.peeksort(begin, right_run_begin, left_run_end, right_run_begin - 1)?;
            return self.merge(begin, right_run_begin, end);
        }
        let (i, j);
        if !self.less(middle, middle - 1)? {
            i = self.decreasing_suffix(left_run_end, middle, true)?;
            j = self.decreasing_prefix(middle - 1, right_run_begin, true)?;
        } else {
            i = self.decreasing_suffix(left_run_end, middle, false)?;
            j = self.decreasing_prefix(middle - 1, right_run_begin, false)?;
            self.items[i..j].reverse();
        }
        if i == begin && j == end {
            return Ok(());
        }
        if middle - i < j - middle {
            self.peeksort(begin, i, left_run_end, i - 1)?;
            self.peeksort(i, end, j, right_run_begin)?;
            self.merge(begin, i, end)
        } else {
            self.peeksort(begin, j, left_run_end, i)?;
            self.peeksort(j, end, j + 1, right_run_begin)?;
            self.merge(begin, j, end)
        }
    }
}

// Sort `items` in place with the same sequence of comparisons as Nix.
pub fn peeksort<T: Clone>(items: &mut [T], less: impl FnMut(&T, &T) -> Result<bool>) -> Result<()> {
    let length = items.len();
    let mut s = Sorter {
        items,
        buffer: Vec::with_capacity(length),
        less,
    };
    if length <= 1 {
        return Ok(());
    }
    if length == 2 {
        if s.less(1, 0)? {
            s.items.swap(0, 1);
        }
        return Ok(());
    }
    s.peeksort(0, length, 0, length)
}
