use atomic_wait::{wait, wake_all, wake_one};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::hint::spin_loop;
use std::mem::align_of;
use std::ops::{Deref, DerefMut};
use std::ptr::{self};
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

// === Constants ===
const TAG_MASK: usize = 0b1;
const PTR_MASK: usize = !TAG_MASK;
const LOCKED: usize = 0b1;

// === RefCount (新实现: 针对节点生命周期的专用轻量级通知器) ===
// 优化点：利用高位标记是否有 Writer 在等待，避免 Reader 无意义的 wake 系统调用。
#[derive(Debug)]
struct RefCount {
    // Bits 0-30: 引用计数
    // Bit 31: WAITING 标记 (表示有 Writer 正在 wait_until_zero)
    state: AtomicU32,
}

const WAITING_BIT: u32 = 1 << 31;
const COUNT_MASK: u32 = !WAITING_BIT;

impl RefCount {
    #[inline(always)]
    fn new() -> Self {
        Self {
            state: AtomicU32::new(0),
        }
    }

    #[inline(always)]
    fn retain(&self) {
        // 仅增加计数，不触碰 Waiting 位
        self.state.fetch_add(1, Ordering::Acquire);
    }

    #[inline(always)]
    fn release(&self) {
        // 优化关键：fetch_sub 返回旧值
        let prev = self.state.fetch_sub(1, Ordering::Release);
        // 如果旧值是 (1 | WAITING_BIT)，说明这是最后一个 Reader，且有 Writer 在等
        if prev == (1 | WAITING_BIT) {
            self.wake();
        }
    }

    // 仅供 Writer 使用，等待读者归零
    #[inline(never)] // 冷路径，防止内联膨胀
    fn wait_until_zero(&self) {
        let mut spin_count = 0;
        loop {
            let val = self.state.load(Ordering::Acquire);
            // 没有任何读者，直接返回
            if (val & COUNT_MASK) == 0 {
                return;
            }

            // 如果没有设置 Waiting 位，尝试设置它
            if (val & WAITING_BIT) == 0 {
                // 尝试 CAS: val -> val | WAITING_BIT
                if self
                    .state
                    .compare_exchange_weak(
                        val,
                        val | WAITING_BIT,
                        Ordering::Relaxed, // CAS 失败重试即可，无需强顺序
                        Ordering::Relaxed,
                    )
                    .is_err()
                {
                    continue; // CAS 失败，重读状态
                }
            }

            // 再次检查（防止在设置位之后 reader 刚好走光）
            let val_now = self.state.load(Ordering::Acquire);
            if (val_now & COUNT_MASK) == 0 {
                return;
            }

            // 自旋优化：先 spin 几次，不要立即 syscall
            if spin_count < 20 {
                spin_loop();
                spin_count += 1;
                continue;
            }

            // 确实需要睡眠，等待唤醒
            wait(&self.state, val_now | WAITING_BIT);
        }
    }

    // 重置状态（当节点被回收复用时）
    #[inline(always)]
    fn reset(&self) {
        self.state.store(0, Ordering::Relaxed);
    }

    #[inline(always)]
    fn wake(&self) {
        let ptr = &self.state as *const AtomicU32;
        // 只需要唤醒一个等待者（唯一的 Writer）
        wake_one(ptr);
    }

    #[inline(always)]
    fn count(&self) -> u32 {
        self.state.load(Ordering::Acquire) & COUNT_MASK
    }
}

// === Ticket Notifier (保持原有逻辑，用于全局锁等待) ===
#[derive(Debug)]
pub struct Notifier {
    inner: AtomicU32,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            inner: AtomicU32::new(0),
        }
    }

    #[inline(always)]
    pub fn ticket(&self) -> u32 {
        self.inner.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn wait_ticket(&self, expected: u32) {
        wait(&self.inner, expected);
    }

    #[inline(always)]
    pub fn advance_and_wake(&self) {
        // 使用 Release 确保之前的内存修改对醒来的线程可见
        self.inner.fetch_add(1, Ordering::Release);
        self.wake_all();
    }

    #[inline(always)]
    fn wake_all(&self) {
        let ptr = &self.inner as *const AtomicU32;
        wake_all(ptr);
    }
}

// === Data Structures ===

// 简单的指数退避工具
struct Backoff {
    step: u32,
}
impl Backoff {
    #[inline(always)]
    fn new() -> Self {
        Self { step: 0 }
    }
    #[inline(always)]
    fn snooze(&mut self) {
        if self.step < 10 {
            spin_loop();
        } else {
            std::thread::yield_now();
        }
        // 饱和计数，防止溢出
        if self.step < 20 {
            self.step += 1;
        }
    }
}

#[repr(align(64))]
struct CachePadded<T> {
    value: T,
}

impl<T> Deref for CachePadded<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        &self.value
    }
}

struct Node<T> {
    data: UnsafeCell<T>,
    // 使用新的 RefCount 替代 Notifier
    reader_count: CachePadded<RefCount>,
}

impl<T> Node<T> {
    #[inline(always)]
    fn new(data: T) -> Self {
        Self {
            data: UnsafeCell::new(data),
            reader_count: CachePadded {
                value: RefCount::new(),
            },
        }
    }
}

// 优化：将 current 与 notifier 隔离，防止 Writer 更新 current 时抖动 notifier 所在的缓存行
struct SharedState<T> {
    // Hot: Writer 和 Reader 都会频繁访问
    current: CachePadded<AtomicUsize>,
    // Warm: 只有 Blocked Reader 和 Writer 在竞争时访问
    notifier: CachePadded<Notifier>,
    // Cold: 只有 Retro Reader 和 Writer 访问
    previous: AtomicPtr<Node<T>>,
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

// === Reader Implementation ===

pub struct Ref<'a, T> {
    node: &'a Node<T>,
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
    shared: &'a SharedState<T>,
}

impl<'a, T> BlockedReader<'a, T> {
    #[cold] // 标记为冷路径，优化分支预测
    pub fn wait(self) -> ReadResult<'a, T> {
        let mut backoff = Backoff::new();
        loop {
            let mut val = self.shared.current.load(Ordering::Acquire);

            if (val & TAG_MASK) == 0 {
                let ptr = (val & PTR_MASK) as *mut Node<T>;
                let node = unsafe { &*ptr };
                node.reader_count.retain();

                // Validation
                if self.shared.current.load(Ordering::Acquire) == val {
                    return ReadResult::Success(Ref { node });
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
    shared: Arc<SharedState<T>>,
}

impl<T> Reader<T> {
    pub fn read(&self) -> ReadResult<'_, T> {
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
}

// === Writer Implementation ===

pub struct InPlaceGuard<'a, T> {
    cell: &'a mut RetroCell<T>,
    locked_val: usize,
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
    cell: &'a mut RetroCell<T>,
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

    pub fn write(&mut self) -> WriteOutcome<'_, T> {
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
