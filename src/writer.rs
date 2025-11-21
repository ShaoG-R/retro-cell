use crate::reader::Reader;
use crate::shared::{Node, SharedState, LOCKED, PTR_MASK};
use crate::sync::Notifier;
use crate::utils::CachePadded;
use std::collections::VecDeque;
use std::mem::align_of;
use std::ops::{Deref, DerefMut};
use std::ptr::{self};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;

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
        // 解锁
        self.cell
            .shared
            .current
            .store(self.locked_val & PTR_MASK, Ordering::Release);
        // 唤醒被锁挡住的 Reader
        self.cell.shared.notifier.advance_and_wake();
    }
}

pub struct CongestedWriter<'a, T> {
    pub(crate) cell: &'a mut RetroCell<T>,
}

impl<'a, T> CongestedWriter<'a, T> {
    pub fn force_in_place(self) -> InPlaceGuard<'a, T> {
        let shared = &self.cell.shared;

        let curr_val = shared.current.load(Ordering::Acquire);
        let locked_val = curr_val | LOCKED;

        // 强制加锁
        shared.current.swap(locked_val, Ordering::AcqRel);

        // 等待读者排空
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        let curr_node = unsafe { &*curr_ptr };

        // 这里会使用新的 RefCount wait 逻辑，只有这里会设置 WAITING 位
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
            // 复用时必须重置 RefCount
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

        // COW 完成，唤醒可能被旧锁（如果有的话）或并发操作阻塞的读者
        self.cell.shared.notifier.advance_and_wake();

        result
    }
}

pub enum WriteOutcome<'a, T> {
    InPlace(InPlaceGuard<'a, T>),
    Congested(CongestedWriter<'a, T>),
}

pub struct RetroCell<T> {
    pub(crate) shared: Arc<SharedState<T>>,
    pub(crate) garbage: VecDeque<*mut Node<T>>,
    pub(crate) pool: Vec<Box<Node<T>>>,
}

unsafe impl<T: Send + Sync> Send for RetroCell<T> {}

impl<T> RetroCell<T> {
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
        // 垃圾回收逻辑保持不变
        while self.garbage.len() > 1 {
            if let Some(&ptr) = self.garbage.front() {
                let node = unsafe { &*ptr };
                // RefCount::count 已经屏蔽了 WAITING 位，直接用即可
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

    pub fn try_write(&mut self) -> WriteOutcome<'_, T> {
        self.collect_garbage();

        let curr_val = self.shared.current.load(Ordering::Acquire);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        let curr_node = unsafe { &*curr_ptr };

        if curr_node.reader_count.count() == 0 {
            let locked_val = curr_val | LOCKED;

            // 优化：使用 AcqRel 代替 SeqCst，在 ARM 上性能更好
            let _ = self.shared.current.swap(locked_val, Ordering::AcqRel);

            if curr_node.reader_count.count() == 0 {
                return WriteOutcome::InPlace(InPlaceGuard {
                    cell: self,
                    locked_val: locked_val,
                });
            } else {
                // 失败回滚
                self.shared.current.store(curr_val, Ordering::Release);
                self.shared.notifier.advance_and_wake();
            }
        }

        WriteOutcome::Congested(CongestedWriter { cell: self })
    }

    /// 直接执行 COW 更新
    pub fn write_cow<F, R>(&mut self, f: F) -> R
    where
        T: Clone,
        F: FnOnce(&mut T) -> R,
    {
        self.collect_garbage();
        CongestedWriter { cell: self }.perform_cow(f)
    }

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
