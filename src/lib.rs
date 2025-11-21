use atomic_wait::{wait, wake_all};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::hint::spin_loop;
use std::mem::align_of;
use std::ops::{Deref, DerefMut};
use std::ptr::{self};
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

// === Constants ===
const TAG_MASK: usize = 0b1;
const PTR_MASK: usize = !TAG_MASK;
const LOCKED: usize = 0b1;

// === Notifier (保持不变) ===
#[derive(Debug)]
pub struct Notifier {
    inner: AtomicU32,
}

impl Notifier {
    pub fn new() -> Self {
        Self { inner: AtomicU32::new(0) }
    }

    #[inline(always)]
    pub fn retain(&self) {
        self.inner.fetch_add(1, Ordering::Acquire);
    }

    #[inline(always)]
    pub fn release(&self) {
        let prev = self.inner.fetch_sub(1, Ordering::Release);
        if prev == 1 { self.wake(); }
    }

    #[inline(always)]
    pub fn wait_until_zero(&self) {
        loop {
            let val = self.inner.load(Ordering::Acquire);
            if val == 0 { return; }
            wait(&self.inner, val);
        }
    }

    #[inline(always)]
    pub fn count(&self) -> u32 {
        self.inner.load(Ordering::Acquire)
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
        self.inner.fetch_add(1, Ordering::Release);
        self.wake();
    }
    
    #[inline(always)]
    fn wake(&self) {
        let ptr = &self.inner as *const AtomicU32;
        wake_all(ptr);
    }
}

// === Data Structures ===

#[repr(align(64))]
struct CachePadded<T> {
    value: T,
}

impl<T> Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.value }
}

struct Node<T> {
    data: UnsafeCell<T>,
    reader_count: CachePadded<Notifier>,
}

impl<T> Node<T> {
    fn new(data: T) -> Self {
        Self {
            data: UnsafeCell::new(data),
            reader_count: CachePadded { value: Notifier::new() },
        }
    }
}

struct SharedState<T> {
    current: AtomicUsize,
    previous: AtomicPtr<Node<T>>,
    notifier: Notifier,
}

unsafe impl<T: Send + Sync> Send for SharedState<T> {}
unsafe impl<T: Send + Sync> Sync for SharedState<T> {}

impl<T> Drop for SharedState<T> {
    fn drop(&mut self) {
        let curr_val = self.current.load(Ordering::Relaxed);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        if !curr_ptr.is_null() {
            unsafe { let _ = Box::from_raw(curr_ptr); }
        }
    }
}

// === Reader Implementation (保持不变) ===

pub struct Ref<'a, T> {
    node: &'a Node<T>,
}

impl<'a, T> Deref for Ref<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target { unsafe { &*self.node.data.get() } }
}

impl<'a, T> Drop for Ref<'a, T> {
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
    pub fn wait(self) -> ReadResult<'a, T> {
        loop {
            let mut val = self.shared.current.load(Ordering::Acquire);

            if (val & TAG_MASK) == 0 {
                let ptr = (val & PTR_MASK) as *mut Node<T>;
                let node = unsafe { &*ptr };
                node.reader_count.retain();

                if self.shared.current.load(Ordering::Acquire) == val {
                    return ReadResult::Success(Ref { node });
                }
                node.reader_count.release();
                spin_loop();
                continue;
            }

            let ticket = self.shared.notifier.ticket();
            val = self.shared.current.load(Ordering::Acquire);
            if (val & TAG_MASK) == 0 { continue; }
            self.shared.notifier.wait_ticket(ticket);
        }
    }

    pub fn read_retro(&self) -> Option<Ref<'a, T>> {
        let prev_ptr = self.shared.previous.load(Ordering::Acquire);
        if prev_ptr.is_null() { return None; }
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
        loop {
            let curr_val = self.shared.current.load(Ordering::Acquire);
            if (curr_val & TAG_MASK) == LOCKED {
                return ReadResult::Blocked(BlockedReader { shared: &self.shared });
            }
            let ptr = (curr_val & PTR_MASK) as *mut Node<T>;
            let node = unsafe { &*ptr };
            node.reader_count.retain();
            
            let val_now = self.shared.current.load(Ordering::Acquire);
            if curr_val != val_now {
                node.reader_count.release();
                spin_loop();
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
    fn deref(&self) -> &T {
        let ptr = (self.locked_val & PTR_MASK) as *mut Node<T>;
        unsafe { &*(*ptr).data.get() }
    }
}

impl<'a, T> DerefMut for InPlaceGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        let ptr = (self.locked_val & PTR_MASK) as *mut Node<T>;
        unsafe { &mut *(*ptr).data.get() }
    }
}

impl<'a, T> Drop for InPlaceGuard<'a, T> {
    fn drop(&mut self) {
        self.cell.shared.current.store(self.locked_val & PTR_MASK, Ordering::Release);
        self.cell.shared.notifier.advance_and_wake();
    }
}

pub struct CongestedWriter<'a, T> {
    cell: &'a mut RetroCell<T>,
}

impl<'a, T> CongestedWriter<'a, T> {
    // [Refactor] 优化：单写者无需 Loop CAS，直接 swap 上锁
    pub fn force_in_place(self) -> InPlaceGuard<'a, T> {
        let shared = &self.cell.shared;
        
        // 1. Load current
        let curr_val = shared.current.load(Ordering::Acquire);
        
        // 2. Force Lock via Swap
        // 由于我们拥有 &mut self，current 的指针部分不会变，只有我们能改。
        // 直接构造带锁的值并 swap。
        let locked_val = curr_val | LOCKED;
        shared.current.swap(locked_val, Ordering::SeqCst);

        // 3. Wait for readers to drain
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        let curr_node = unsafe { &*curr_ptr };
        curr_node.reader_count.wait_until_zero();

        InPlaceGuard {
            cell: self.cell,
            locked_val: curr_val, // 此时 locked_val 等于刚 swap 进去的值
        }
    }

    pub fn perform_cow<F, R>(self, f: F) -> R
    where 
        T: Clone,
        F: FnOnce(&mut T) -> R
    {
        let curr_val = self.cell.shared.current.load(Ordering::Acquire);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        let curr_node = unsafe { &*curr_ptr };
        
        let new_data = unsafe { (*curr_node.data.get()).clone() };

        let mut new_node = if let Some(recycled_node) = self.cell.pool.pop() {
            unsafe { *recycled_node.data.get() = new_data };
            recycled_node.reader_count.inner.store(0, Ordering::Relaxed);
            recycled_node
        } else {
            Box::new(Node::new(new_data))
        };

        let result = f(new_node.data.get_mut());
        let new_ptr = Box::into_raw(new_node);

        let old_val_raw = self.cell.shared.current.swap(new_ptr as usize, Ordering::Release);
        
        let old_ptr = (old_val_raw & PTR_MASK) as *mut Node<T>;
        self.cell.garbage.push_back(old_ptr);
        self.cell.shared.previous.store(old_ptr, Ordering::Release);
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
    where T: Clone {
        assert!(align_of::<Node<T>>() >= 2);
        let node = Box::new(Node::new(initial));
        let ptr = Box::into_raw(node);

        let shared = Arc::new(SharedState {
            current: AtomicUsize::new(ptr as usize),
            previous: AtomicPtr::new(ptr::null_mut()),
            notifier: Notifier::new(),
        });

        (
            RetroCell { shared: shared.clone(), garbage: VecDeque::new(), pool: Vec::new() },
            Reader { shared }
        )
    }

    fn collect_garbage(&mut self) {
        while self.garbage.len() > 1 {
            if let Some(&ptr) = self.garbage.front() {
                let node = unsafe { &*ptr };
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

    // [Refactor] 使用 swap 优化的 write 方法
    pub fn write(&mut self) -> WriteOutcome<'_, T> {
        self.collect_garbage();

        // 1. 获取当前指针
        let curr_val = self.shared.current.load(Ordering::Acquire);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        let curr_node = unsafe { &*curr_ptr };

        // 2. 乐观检查：如果没有读者，我们尝试 In-Place
        if curr_node.reader_count.count() == 0 {
            let locked_val = curr_val | LOCKED;
            
            // 3. 核心优化：直接 Swap，无需 Compare-Exchange
            // 理由：单写者保证了此时没有其他线程会修改 current 的值，我们拥有绝对的写入权。
            // 使用 SeqCst 确保 Store (设置锁) 和后续的 Load (检查读者) 不会乱序。
            let _ = self.shared.current.swap(locked_val, Ordering::SeqCst);

            // 4. Double Check
            // 必须再次检查，防止在我们 swap 加锁的极短窗口内有新读者进入
            if curr_node.reader_count.count() == 0 {
                return WriteOutcome::InPlace(InPlaceGuard {
                    cell: self,
                    locked_val: locked_val,
                });
            } else {
                // 失败：有读者闯入。
                // 必须解锁 (恢复原值)
                self.shared.current.store(curr_val, Ordering::Release);
                // 唤醒可能因看到我们刚加的锁而沉睡的读者
                self.shared.notifier.advance_and_wake();
            }
        }

        WriteOutcome::Congested(CongestedWriter { cell: self })
    }
}

impl<T> Drop for RetroCell<T> {
    fn drop(&mut self) {
        self.collect_garbage();
        while let Some(ptr) = self.garbage.pop_front() {
            unsafe { drop(Box::from_raw(ptr)); }
        }
    }
}