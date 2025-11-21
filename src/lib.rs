use atomic_wait::{wait, wake_all};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::hint::spin_loop;
use std::mem::align_of;
use std::ops::{Deref, DerefMut};
use std::ptr::{self};
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering}; // 依然需要引入，供 Notifier 内部使用

// === Constants ===
const TAG_MASK: usize = 0b1;
const PTR_MASK: usize = !TAG_MASK;
const LOCKED: usize = 0b1;
const REF_ONE: usize = 1;

pub struct Notifier {
    epoch: AtomicU32,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            epoch: AtomicU32::new(0),
        }
    }

    #[inline(always)]
    pub fn ticket(&self) -> u32 {
        self.epoch.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn wait(&self, expected_ticket: u32) {
        wait(&self.epoch, expected_ticket);
    }

    #[inline(always)]
    pub fn notify_all(&self) {
        self.epoch.fetch_add(1, Ordering::Release);
        wake_all(&self.epoch);
    }
}

#[repr(align(64))]
struct CachePadded<T> {
    value: T,
}

impl<T> Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

// === Data Structures ===

struct Node<T> {
    data: UnsafeCell<T>,
    reader_count: CachePadded<AtomicUsize>,
}

impl<T> Node<T> {
    fn new(data: T) -> Self {
        Self {
            data: UnsafeCell::new(data),
            reader_count: CachePadded {
                value: AtomicUsize::new(0),
            },
        }
    }
}

struct SharedState<T> {
    current: AtomicUsize,
    previous: AtomicPtr<Node<T>>,
    // [Refactor] 使用封装好的 Notifier
    notifier: Notifier,
}

unsafe impl<T: Send + Sync> Send for SharedState<T> {}
unsafe impl<T: Send + Sync> Sync for SharedState<T> {}

impl<T> Drop for SharedState<T> {
    fn drop(&mut self) {
        let curr_val = self.current.load(Ordering::Relaxed);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        if !curr_ptr.is_null() {
            unsafe {
                let _ = Box::from_raw(curr_ptr);
            }
        }
        // previous 不需要手动 drop，因为它存在于 RetroCell 的 garbage/pool 中
    }
}

pub struct Ref<'a, T> {
    node: &'a Node<T>,
}

impl<'a, T> Deref for Ref<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.node.data.get() }
    }
}

impl<'a, T> Drop for Ref<'a, T> {
    fn drop(&mut self) {
        self.node.reader_count.fetch_sub(REF_ONE, Ordering::Release);
    }
}

pub enum ReadResult<'a, T> {
    Success(Ref<'a, T>),
    Blocked(BlockedReader<'a, T>),
}

pub struct BlockedReader<'a, T> {
    shared: &'a SharedState<T>,
}

impl<'a, T> BlockedReader<'a, T> {
    pub fn wait(self) -> ReadResult<'a, T> {
        loop {
            let mut val = self.shared.current.load(Ordering::Acquire);

            // 1. 尝试无锁获取
            if (val & TAG_MASK) == 0 {
                let ptr = (val & PTR_MASK) as *mut Node<T>;
                let node = unsafe { &*ptr };
                node.reader_count.fetch_add(REF_ONE, Ordering::Acquire);

                // Double check
                if self.shared.current.load(Ordering::Acquire) == val {
                    return ReadResult::Success(Ref { node });
                }
                node.reader_count.fetch_sub(REF_ONE, Ordering::Release);
                spin_loop();
                continue;
            }

            // 2. 锁竞争路径：准备睡眠
            // [Refactor] 获取当前票据
            let ticket = self.shared.notifier.ticket();

            // Re-check current before waiting to avoid race (Lost Wakeup)
            val = self.shared.current.load(Ordering::Acquire);
            if (val & TAG_MASK) == 0 {
                continue; // 锁在获取票据的过程中被释放了，重试
            }

            // [Refactor] 语义清晰的等待
            self.shared.notifier.wait(ticket);
        }
    }

    pub fn read_retro(&self) -> Option<Ref<'a, T>> {
        let prev_ptr = self.shared.previous.load(Ordering::Acquire);
        if prev_ptr.is_null() {
            return None;
        }
        let node = unsafe { &*prev_ptr };
        node.reader_count.fetch_add(REF_ONE, Ordering::Acquire);
        Some(Ref { node })
    }
}

#[derive(Clone)]
pub struct Reader<T> {
    shared: Arc<SharedState<T>>,
}

impl<T> Reader<T> {
    pub fn read(&self) -> ReadResult<'_, T> {
        loop {
            let curr_val = self.shared.current.load(Ordering::Acquire);

            if (curr_val & TAG_MASK) == LOCKED {
                return ReadResult::Blocked(BlockedReader {
                    shared: &self.shared,
                });
            }

            let ptr = (curr_val & PTR_MASK) as *mut Node<T>;
            let node = unsafe { &*ptr };
            node.reader_count.fetch_add(REF_ONE, Ordering::Acquire);

            let val_now = self.shared.current.load(Ordering::Acquire);
            if curr_val != val_now {
                node.reader_count.fetch_sub(REF_ONE, Ordering::Release);
                spin_loop();
                continue;
            }
            return ReadResult::Success(Ref { node });
        }
    }
}

pub struct RetroCell<T> {
    shared: Arc<SharedState<T>>,
    garbage: VecDeque<*mut Node<T>>,
    pool: Vec<Box<Node<T>>>,
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
            current: AtomicUsize::new(ptr as usize),
            previous: AtomicPtr::new(ptr::null_mut()),
            // [Refactor] 初始化 Notifier
            notifier: Notifier::new(),
        });

        let writer = RetroCell {
            shared: shared.clone(),
            garbage: VecDeque::new(),
            pool: Vec::new(),
        };
        let reader = Reader { shared };
        (writer, reader)
    }

    fn collect_garbage(&mut self) {
        while self.garbage.len() > 1 {
            if let Some(&ptr) = self.garbage.front() {
                let node = unsafe { &*ptr };
                if node.reader_count.load(Ordering::Acquire) == 0 {
                    self.garbage.pop_front();
                    let node_box = unsafe { Box::from_raw(ptr) };
                    self.pool.push(node_box);
                } else {
                    break;
                }
            }
        }
    }

    pub fn update<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
        T: Clone,
    {
        self.collect_garbage();

        let curr_val = self.shared.current.load(Ordering::Acquire);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        let curr_node = unsafe { &*curr_ptr };

        // 预检查：如果已经有读者，直接跳过昂贵的 swap，走 COW
        let reader_cnt = curr_node.reader_count.load(Ordering::Acquire);

        // === Path A: In-Place ===
        if reader_cnt == 0 {
            let locked_val = curr_val | LOCKED;

            // [优化] 使用 swap 替代 compare_exchange
            // 1. 单一写者保证无需 compare。
            // 2. Ordering::SeqCst 强制建立内存屏障，确保 swap (Store Lock) 先于后续的 reader_count load 执行。
            //    避免 Store 和 Load 被 CPU 重排。
            let _ = self.shared.current.swap(locked_val, Ordering::SeqCst);

            // 再次检查：确保在加锁的时间窗口内没有新的 Reader 闯入
            if curr_node.reader_count.load(Ordering::Acquire) == 0 {
                let result = unsafe { f(&mut *curr_node.data.get()) };

                // 解锁
                self.shared
                    .current
                    .store(curr_val & PTR_MASK, Ordering::Release);

                self.shared.notifier.notify_all();
                return result;
            } else {
                // 失败：有 Reader 进来了
                // 必须解锁 (恢复原状)
                self.shared.current.store(curr_val, Ordering::Release);

                // 必须 notify，因为可能有 Reader 看到 LOCKED 状态去休眠了，现在我们放弃加锁，需要唤醒它们重试
                self.shared.notifier.notify_all();
            }
        }

        // === Path B: COW ===
        // In-Place 失败或条件不满足，执行 Copy-On-Write
        let new_data = unsafe { (*curr_node.data.get()).clone() };
        let mut new_node = if let Some(recycled_node) = self.pool.pop() {
            unsafe { *recycled_node.data.get() = new_data };
            recycled_node.reader_count.store(0, Ordering::Relaxed);
            recycled_node
        } else {
            Box::new(Node::new(new_data))
        };

        let result = f(new_node.data.get_mut());
        let new_ptr = Box::into_raw(new_node);

        self.shared
            .current
            .store(new_ptr as usize, Ordering::Release);
        self.garbage.push_back(curr_ptr);
        self.shared.previous.store(curr_ptr, Ordering::Release);

        result
    }
}

impl<T> Drop for RetroCell<T> {
    fn drop(&mut self) {
        self.collect_garbage();
        while let Some(ptr) = self.garbage.pop_front() {
            unsafe {
                drop(Box::from_raw(ptr));
            }
        }
    }
}
