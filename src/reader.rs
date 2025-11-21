use crate::shared::{Node, SharedState, LOCKED, PTR_MASK, TAG_MASK};
use crate::utils::Backoff;
use std::ops::Deref;
use std::sync::atomic::Ordering;
use std::sync::Arc;

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

pub enum ReadResult<'a, T> {
    Success(Ref<'a, T>),
    Blocked(BlockedReader<'a, T>),
}

pub struct BlockedReader<'a, T> {
    pub(crate) shared: &'a SharedState<T>,
}

impl<'a, T> BlockedReader<'a, T> {
    #[cold] // 标记为冷路径，优化分支预测
    pub fn wait(self) -> Ref<'a, T> {
        let mut backoff = Backoff::new();
        loop {
            let mut val = self.shared.current.load(Ordering::Acquire);

            if (val & TAG_MASK) == 0 {
                let ptr = (val & PTR_MASK) as *mut Node<T>;
                let node = unsafe { &*ptr };
                node.reader_count.retain();

                // Validation
                if self.shared.current.load(Ordering::Acquire) == val {
                    return Ref { node };
                }
                // Validation Failed
                node.reader_count.release();
                backoff.snooze();
                continue;
            }

            let ticket = self.shared.notifier.ticket();
            val = self.shared.current.load(Ordering::Acquire);

            // 如果在获取 ticket 后锁被释放了，不要睡，直接重试
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

#[derive(Clone)]
pub struct Reader<T> {
    pub(crate) shared: Arc<SharedState<T>>,
}

impl<T> Reader<T> {
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

            // 乐观引用
            node.reader_count.retain();

            // 验证指针是否改变
            let val_now = self.shared.current.load(Ordering::Acquire);
            if curr_val != val_now {
                node.reader_count.release();
                backoff.snooze(); // 使用优化后的退避
                continue;
            }
            return ReadResult::Success(Ref { node });
        }
    }

    /// 读取最新数据（阻塞直到可用）
    #[inline]
    pub fn read(&self) -> Ref<'_, T> {
        match self.try_read() {
            ReadResult::Success(r) => r,
            ReadResult::Blocked(blocked) => blocked.wait(),
        }
    }

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
