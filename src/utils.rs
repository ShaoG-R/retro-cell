use crate::rt::hint::spin_loop;
use std::ops::Deref;

/// Simple exponential backoff utility
///
/// 简单的指数退避工具
pub(crate) struct Backoff {
    step: u32,
}
impl Backoff {
    #[inline(always)]
    pub(crate) fn new() -> Self {
        Self { step: 0 }
    }
    #[inline(always)]
    pub(crate) fn snooze(&mut self) {
        if self.step < 10 {
            spin_loop();
        } else {
            crate::rt::thread::yield_now();
        }
        // Saturating increment
        // 饱和递增
        if self.step < 20 {
            self.step += 1;
        }
    }
}

/// Padding to avoid false sharing
///
/// 防止伪共享的填充
#[repr(align(64))]
pub(crate) struct CachePadded<T> {
    pub(crate) value: T,
}

impl<T> Deref for CachePadded<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        &self.value
    }
}
