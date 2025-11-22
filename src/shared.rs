use crate::rt::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use crate::sync::{Notifier, RefCount};
use crate::utils::CachePadded;
use std::cell::UnsafeCell;

// === Constants ===
pub(crate) const TAG_MASK: usize = 0b1;
pub(crate) const PTR_MASK: usize = !TAG_MASK;
pub(crate) const LOCKED: usize = 0b1;

pub(crate) struct Node<T> {
    pub(crate) data: UnsafeCell<T>,

    pub(crate) reader_count: CachePadded<RefCount>,
}

impl<T> Node<T> {
    #[inline(always)]
    pub(crate) fn new(data: T) -> Self {
        Self {
            data: UnsafeCell::new(data),
            reader_count: CachePadded {
                value: RefCount::new(),
            },
        }
    }
}

/// Optimization: Separate 'current' and 'notifier' to prevent cache line thrashing
///
/// 优化：分离 'current' 和 'notifier' 以防止缓存行抖动
pub(crate) struct SharedState<T> {
    // Hot: Frequently accessed by both Writer and Reader
    // Hot: Writer 和 Reader 都会频繁访问
    pub(crate) current: CachePadded<AtomicUsize>,
    // Warm: Accessed only when Blocked Reader and Writer compete
    // Warm: 只有 Blocked Reader 和 Writer 在竞争时访问
    pub(crate) notifier: CachePadded<Notifier>,
    // Cold: Accessed only by Retro Reader and Writer
    // Cold: 只有 Retro Reader 和 Writer 访问
    pub(crate) previous: AtomicPtr<Node<T>>,
}

unsafe impl<T: Send + Sync> Send for SharedState<T> {}
unsafe impl<T: Send + Sync> Sync for SharedState<T> {}

impl<T> Drop for SharedState<T> {
    #[inline(always)]
    fn drop(&mut self) {
        let curr_val = self.current.load(Ordering::Relaxed);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        if !curr_ptr.is_null() {
            unsafe {
                let _ = Box::from_raw(curr_ptr);
            }
        }
    }
}
