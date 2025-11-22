use crate::reader::Reader;
use crate::rt::sync::Arc;
use crate::rt::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use crate::shared::{LOCKED, Node, PTR_MASK, SharedState};
use crate::sync::Notifier;
use crate::utils::CachePadded;
use std::collections::VecDeque;
use std::mem::align_of;
use std::ops::{Deref, DerefMut};
use std::ptr::{self};

/// Guard for in-place writing
///
/// 原地写入的守卫
pub struct InPlaceGuard<'a, T> {
    pub(crate) cell: &'a mut RetroCell<T>,
    pub(crate) locked_val: usize,
}

impl<'a, T> Deref for InPlaceGuard<'a, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        let ptr = (self.locked_val & PTR_MASK) as *mut Node<T>;
        unsafe { &*(*ptr).data.get() }
    }
}

impl<'a, T> DerefMut for InPlaceGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        let ptr = (self.locked_val & PTR_MASK) as *mut Node<T>;
        unsafe { &mut *(*ptr).data.get() }
    }
}

impl<'a, T> Drop for InPlaceGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        self.cell
            .shared
            .current
            .store(self.locked_val & PTR_MASK, Ordering::Release);
        // Wake up readers blocked by the lock
        // 唤醒被锁阻塞的读者
        self.cell.shared.notifier.advance_and_wake();
    }
}

/// Writer that handles congestion
///
/// 处理拥塞的写入者
pub struct CongestedWriter<'a, T> {
    pub(crate) cell: &'a mut RetroCell<T>,
}

impl<'a, T> CongestedWriter<'a, T> {
    pub fn force_in_place(self) -> InPlaceGuard<'a, T> {
        let shared = &self.cell.shared;

        let curr_val = shared.current.load(Ordering::Acquire);
        let locked_val = curr_val | LOCKED;

        // Forcefully acquire the lock
        // 强制获取锁
        shared.current.swap(locked_val, Ordering::AcqRel);

        // Wait for active readers to drain
        // 等待活跃读者排空
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        let curr_node = unsafe { &*curr_ptr };

        curr_node.reader_count.wait_until_zero();

        InPlaceGuard {
            cell: self.cell,
            locked_val: curr_val,
        }
    }

    pub fn perform_cow<F, R>(self, f: F) -> R
    where
        T: Clone,
        F: FnOnce(&mut T) -> R,
    {
        let curr_val = self.cell.shared.current.load(Ordering::Acquire);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        let curr_node = unsafe { &*curr_ptr };

        let new_data = unsafe { (*curr_node.data.get()).clone() };

        let mut new_node = if let Some(recycled_node) = self.cell.pool.pop() {
            unsafe { *recycled_node.data.get() = new_data };
            // Reset RefCount for reuse
            // 重置 RefCount 以复用
            recycled_node.reader_count.reset();
            recycled_node
        } else {
            Box::new(Node::new(new_data))
        };

        let result = f(new_node.data.get_mut());
        let new_ptr = Box::into_raw(new_node);

        let old_val_raw = self
            .cell
            .shared
            .current
            .swap(new_ptr as usize, Ordering::Release);

        let old_ptr = (old_val_raw & PTR_MASK) as *mut Node<T>;
        self.cell.garbage.push_back(old_ptr);
        self.cell.shared.previous.store(old_ptr, Ordering::Release);

        // COW complete. Wake up blocked readers
        // COW 完成。唤醒阻塞的读者
        self.cell.shared.notifier.advance_and_wake();

        result
    }
}

/// Outcome of a write attempt
///
/// 写入尝试的结果
pub enum WriteOutcome<'a, T> {
    InPlace(InPlaceGuard<'a, T>),
    Congested(CongestedWriter<'a, T>),
}

/// A concurrent cell that supports retro-reading
///
/// 支持回溯读取的并发单元
pub struct RetroCell<T> {
    pub(crate) shared: Arc<SharedState<T>>,
    pub(crate) garbage: VecDeque<*mut Node<T>>,
    pub(crate) pool: Vec<Box<Node<T>>>,
}

unsafe impl<T: Send + Sync> Send for RetroCell<T> {}

impl<T> RetroCell<T> {
    /// Create a new RetroCell
    ///
    /// 创建一个新的 RetroCell
    pub fn new(initial: T) -> (Self, Reader<T>)
    where
        T: Clone,
    {
        assert!(align_of::<Node<T>>() >= 2);
        let node = Box::new(Node::new(initial));
        let ptr = Box::into_raw(node);

        let shared = Arc::new(SharedState {
            current: CachePadded {
                value: AtomicUsize::new(ptr as usize),
            },
            notifier: CachePadded {
                value: Notifier::new(),
            },
            previous: AtomicPtr::new(ptr::null_mut()),
        });

        (
            RetroCell {
                shared: shared.clone(),
                garbage: VecDeque::new(),
                pool: Vec::new(),
            },
            Reader { shared },
        )
    }

    #[inline]
    fn collect_garbage(&mut self) {
        while self.garbage.len() > 1 {
            if let Some(&ptr) = self.garbage.front() {
                let node = unsafe { &*ptr };
                // RefCount::count masks the WAITING bit
                // RefCount::count 已屏蔽 WAITING 位
                if node.reader_count.count() == 0 {
                    self.garbage.pop_front();
                    let node_box = unsafe { Box::from_raw(ptr) };
                    self.pool.push(node_box);
                } else {
                    break;
                }
            }
        }
    }

    /// Try to write to the cell
    ///
    /// 尝试写入单元
    pub fn try_write(&mut self) -> WriteOutcome<'_, T> {
        self.collect_garbage();

        let curr_val = self.shared.current.load(Ordering::Acquire);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        let curr_node = unsafe { &*curr_ptr };

        if curr_node.reader_count.count() == 0 {
            let locked_val = curr_val | LOCKED;

            // Optimization: AcqRel performs better on ARM
            // 优化：AcqRel 在 ARM 上性能更佳
            let _ = self.shared.current.swap(locked_val, Ordering::AcqRel);

            if curr_node.reader_count.count() == 0 {
                return WriteOutcome::InPlace(InPlaceGuard {
                    cell: self,
                    locked_val: locked_val,
                });
            } else {
                // Rollback lock on failure
                // 失败时回滚锁
                self.shared.current.store(curr_val, Ordering::Release);
                self.shared.notifier.advance_and_wake();
            }
        }

        WriteOutcome::Congested(CongestedWriter { cell: self })
    }

    /// Perform COW update directly
    ///
    /// 直接执行 COW 更新
    pub fn write_cow<F, R>(&mut self, f: F) -> R
    where
        T: Clone,
        F: FnOnce(&mut T) -> R,
    {
        self.collect_garbage();
        CongestedWriter { cell: self }.perform_cow(f)
    }

    /// Write in-place after locking the latest data (block until locked)
    ///
    /// 锁定最新数据后写入（阻塞直到锁定）
    pub fn write_in_place(&mut self) -> InPlaceGuard<'_, T> {
        self.collect_garbage();
        CongestedWriter { cell: self }.force_in_place()
    }
}

impl<T> Drop for RetroCell<T> {
    #[inline]
    fn drop(&mut self) {
        self.collect_garbage();
        while let Some(ptr) = self.garbage.pop_front() {
            unsafe {
                drop(Box::from_raw(ptr));
            }
        }
    }
}
