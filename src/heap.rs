//! The garbage-collected heap: one arena per kind of cell, addressed by
//! 32-bit indices. Objects never move (indices are stable), which keeps
//! handles, `ArrayBuffer` data and snapshots simple. Collection is precise
//! mark-sweep and only runs at interpreter safepoints, never inside an
//! allocation.

use alloc::vec::Vec;

/// A slot of an arena: a live cell or a link in the free list.
enum Slot<T> {
    Used(T),
    Free(u32),
}

const NO_FREE: u32 = u32::MAX;

pub struct Arena<T> {
    slots: Vec<Slot<T>>,
    marks: Vec<u64>,
    free_head: u32,
    live: usize,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena { slots: Vec::new(), marks: Vec::new(), free_head: NO_FREE, live: 0 }
    }
}

impl<T> Arena<T> {
    pub fn alloc(&mut self, v: T) -> u32 {
        self.live += 1;
        if self.free_head != NO_FREE {
            let i = self.free_head;
            match core::mem::replace(&mut self.slots[i as usize], Slot::Used(v)) {
                Slot::Free(next) => self.free_head = next,
                Slot::Used(_) => unreachable!("free list points at a live cell"),
            }
            return i;
        }
        let i = self.slots.len() as u32;
        self.slots.push(Slot::Used(v));
        if self.slots.len() > self.marks.len() * 64 {
            self.marks.push(0);
        }
        i
    }

    #[inline]
    pub fn get(&self, i: u32) -> &T {
        match &self.slots[i as usize] {
            Slot::Used(v) => v,
            Slot::Free(_) => panic!("use of a freed heap cell #{i}"),
        }
    }

    #[inline]
    pub fn get_mut(&mut self, i: u32) -> &mut T {
        match &mut self.slots[i as usize] {
            Slot::Used(v) => v,
            Slot::Free(_) => panic!("use of a freed heap cell #{i}"),
        }
    }

    /// Two distinct cells mutably at once.
    pub fn get2_mut(&mut self, a: u32, b: u32) -> (&mut T, &mut T) {
        assert_ne!(a, b);
        let (lo, hi, swap) = if a < b { (a, b, false) } else { (b, a, true) };
        let (left, right) = self.slots.split_at_mut(hi as usize);
        let x = match &mut left[lo as usize] {
            Slot::Used(v) => v,
            Slot::Free(_) => panic!("use of a freed heap cell"),
        };
        let y = match &mut right[0] {
            Slot::Used(v) => v,
            Slot::Free(_) => panic!("use of a freed heap cell"),
        };
        if swap { (y, x) } else { (x, y) }
    }

    pub fn is_live(&self, i: u32) -> bool {
        matches!(self.slots.get(i as usize), Some(Slot::Used(_)))
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Marks cell `i`; returns true if it was not marked before.
    #[inline]
    pub fn mark(&mut self, i: u32) -> bool {
        let (w, b) = ((i / 64) as usize, i % 64);
        let m = &mut self.marks[w];
        if *m & (1 << b) != 0 {
            false
        } else {
            *m |= 1 << b;
            true
        }
    }

    #[inline]
    pub fn is_marked(&self, i: u32) -> bool {
        self.marks[(i / 64) as usize] & (1 << (i % 64)) != 0
    }

    /// Frees every unmarked cell (calling `on_free` with it) and clears the
    /// marks. Returns how many cells were freed.
    pub fn sweep(&mut self, mut on_free: impl FnMut(u32, T)) -> usize {
        let mut freed = 0;
        for i in (0..self.slots.len() as u32).rev() {
            if self.is_marked(i) {
                continue;
            }
            if let Slot::Used(_) = self.slots[i as usize] {
                let old = core::mem::replace(&mut self.slots[i as usize], Slot::Free(self.free_head));
                self.free_head = i;
                freed += 1;
                if let Slot::Used(v) = old {
                    on_free(i, v);
                }
            }
        }
        for m in self.marks.iter_mut() {
            *m = 0;
        }
        self.live -= freed;
        freed
    }

    /// Iterates over live cells.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        self.slots.iter().enumerate().filter_map(|(i, s)| match s {
            Slot::Used(v) => Some((i as u32, v)),
            Slot::Free(_) => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_mark_sweep() {
        let mut a: Arena<i32> = Arena::default();
        let x = a.alloc(1);
        let y = a.alloc(2);
        let z = a.alloc(3);
        a.mark(x);
        a.mark(z);
        let mut freed = Vec::new();
        assert_eq!(a.sweep(|i, v| freed.push((i, v))), 1);
        assert_eq!(freed, [(y, 2)]);
        assert!(!a.is_live(y));
        let w = a.alloc(4);
        assert_eq!(w, y);
        assert_eq!(*a.get(w), 4);
        assert_eq!(a.len(), 3);
    }
}
