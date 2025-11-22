use crate::rt::sync::Arc;
use crate::rt::sync::atomic::Ordering;
use crate::shared::{LOCKED, Node, PTR_MASK, SharedState, TAG_MASK};
use crate::utils::Backoff;
use std::ops::Deref;

/// RAII guard for reading values
///
/// 用于读取值的 RAII 守卫
pub struct Ref<'a, T> {
    pub(crate) node: &'a Node<T>,
}

impl<'a, T> Deref for Ref<'a, T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.node.data.get() }
    }
}

impl<'a, T> Drop for Ref<'a, T> {
    #[inline(always)]
    fn drop(&mut self) {
        self.node.reader_count.release();
    }
}

/// Result of a non-blocking read attempt
///
/// 非阻塞读取尝试的结果
pub enum ReadResult<'a, T> {
    Success(Ref<'a, T>),
    Blocked(BlockedReader<'a, T>),
}

/// A reader that is blocked by a writer
///
/// 被写入者阻塞的读取者
pub struct BlockedReader<'a, T> {
    pub(crate) shared: &'a SharedState<T>,
}

impl<'a, T> BlockedReader<'a, T> {
    #[cold]
    // Mark as cold path to optimize branch prediction
    // 标记为冷路径，优化分支预测
    pub fn wait(self) -> Ref<'a, T> {
        let mut backoff = Backoff::new();
        loop {
            let mut val = self.shared.current.load(Ordering::Acquire);

            if (val & TAG_MASK) == 0 {
                let ptr = (val & PTR_MASK) as *mut Node<T>;
                let node = unsafe { &*ptr };
                node.reader_count.retain();

                // Validate consistency
                // 验证一致性
                if self.shared.current.load(Ordering::Acquire) == val {
                    return Ref { node };
                }
                node.reader_count.release();
                backoff.snooze();
                continue;
            }

            let ticket = self.shared.notifier.ticket();
            val = self.shared.current.load(Ordering::Acquire);

            // If lock is released after getting ticket, retry immediately
            // 获取 ticket 后若锁释放，立即重试
            if (val & TAG_MASK) == 0 {
                continue;
            }

            self.shared.notifier.wait_ticket(ticket);
        }
    }

    #[inline]
    pub fn read_retro(&self) -> Option<Ref<'a, T>> {
        let prev_ptr = self.shared.previous.load(Ordering::Acquire);
        if prev_ptr.is_null() {
            return None;
        }
        let node = unsafe { &*prev_ptr };
        node.reader_count.retain();
        Some(Ref { node })
    }
}

/// Reader for accessing the data
///
/// 用于访问数据的读取者
#[derive(Clone)]
pub struct Reader<T> {
    pub(crate) shared: Arc<SharedState<T>>,
}

impl<T> Reader<T> {
    /// Try to read the current value without blocking
    ///
    /// 尝试非阻塞地读取当前值
    pub fn try_read(&self) -> ReadResult<'_, T> {
        let mut backoff = Backoff::new();
        loop {
            let curr_val = self.shared.current.load(Ordering::Acquire);
            if (curr_val & TAG_MASK) == LOCKED {
                return ReadResult::Blocked(BlockedReader {
                    shared: &self.shared,
                });
            }
            let ptr = (curr_val & PTR_MASK) as *mut Node<T>;
            let node = unsafe { &*ptr };

            // Optimistically increment reader count
            // 乐观增加读者计数
            node.reader_count.retain();

            // Verify if the pointer changed during the process
            // 验证过程中指针是否发生变化
            let val_now = self.shared.current.load(Ordering::Acquire);
            if curr_val != val_now {
                node.reader_count.release();
                backoff.snooze();
                continue;
            }
            return ReadResult::Success(Ref { node });
        }
    }

    /// Read the latest data (block until available)
    ///
    /// 读取最新数据（阻塞直到可用）
    #[inline]
    pub fn read(&self) -> Ref<'_, T> {
        match self.try_read() {
            ReadResult::Success(r) => r,
            ReadResult::Blocked(blocked) => blocked.wait(),
        }
    }

    /// Read historical data (if available)
    ///
    /// 读取历史数据（如果有）
    #[inline]
    pub fn read_retro(&self) -> Option<Ref<'_, T>> {
        let prev_ptr = self.shared.previous.load(Ordering::Acquire);
        if prev_ptr.is_null() {
            return None;
        }
        let node = unsafe { &*prev_ptr };
        node.reader_count.retain();
        Some(Ref { node })
    }
}
