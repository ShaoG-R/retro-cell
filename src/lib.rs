use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::mem::align_of;
use std::ops::{Deref, DerefMut};
use std::ptr::{self};
use std::sync::atomic::{AtomicPtr, AtomicUsize, AtomicU32, Ordering};
use std::sync::Arc;
use std::hint::spin_loop;
use atomic_wait::{wait, wake_all};

// === Constants ===
const TAG_MASK: usize = 0b1;
const PTR_MASK: usize = !TAG_MASK;
const LOCKED: usize = 0b1;
const REF_ONE: usize = 1;

#[repr(align(64))]
struct CachePadded<T> {
    value: T,
}

impl<T> Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.value }
}

impl<T> DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut T { &mut self.value }
}

struct Node<T> {
    data: UnsafeCell<T>,
    reader_count: CachePadded<AtomicUsize>,
}

impl<T> Node<T> {
    fn new(data: T) -> Self {
        Self {
            data: UnsafeCell::new(data),
            reader_count: CachePadded { value: AtomicUsize::new(0) },
        }
    }
}

struct SharedState<T> {
    current: AtomicUsize,
    // 依然保留 previous 指针供 Reader 读取，但不再使用 swap 维护
    previous: AtomicPtr<Node<T>>, 
    writer_notify: AtomicU32,
}

unsafe impl<T: Send + Sync> Send for SharedState<T> {}
unsafe impl<T: Send + Sync> Sync for SharedState<T> {}

impl<T> Drop for SharedState<T> {
    fn drop(&mut self) {
        let curr_val = self.current.load(Ordering::Relaxed);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        if !curr_ptr.is_null() { unsafe { let _ = Box::from_raw(curr_ptr); } }
        
        // previous 不需要单独 drop，因为它指向的节点一定在 writer 的 garbage 队列或 pool 中
        // 除非 RetroCell 已经 drop 了。为了安全起见，这里置空即可，实际内存在 RetroCell 中管理。
    }
}

// ... (Ref, Deref, Drop, ReadResult, BlockedReader, Reader 保持不变) ...
// ... 为了节省篇幅，省略未变动的 Reader/Ref 代码，逻辑与上一版完全一致 ...

pub struct Ref<'a, T> {
    node: &'a Node<T>,
}
impl<'a, T> Deref for Ref<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target { unsafe { &*self.node.data.get() } }
}
impl<'a, T> Drop for Ref<'a, T> {
    fn drop(&mut self) { self.node.reader_count.fetch_sub(REF_ONE, Ordering::Release); }
}
pub enum ReadResult<'a, T> { Success(Ref<'a, T>), Blocked(BlockedReader<'a, T>), }

pub struct BlockedReader<'a, T> { shared: &'a SharedState<T>, }
impl<'a, T> BlockedReader<'a, T> {
    pub fn wait(self) -> ReadResult<'a, T> {
        loop {
            let mut val = self.shared.current.load(Ordering::Acquire);
            if (val & TAG_MASK) == 0 {
                let ptr = (val & PTR_MASK) as *mut Node<T>;
                let node = unsafe { &*ptr };
                node.reader_count.fetch_add(REF_ONE, Ordering::Acquire);
                if self.shared.current.load(Ordering::Acquire) == val {
                    return ReadResult::Success(Ref { node });
                }
                node.reader_count.fetch_sub(REF_ONE, Ordering::Release);
                spin_loop(); continue;
            }
            let epoch = self.shared.writer_notify.load(Ordering::Acquire);
            val = self.shared.current.load(Ordering::Acquire);
            if (val & TAG_MASK) == 0 { continue; }
            wait(&self.shared.writer_notify, epoch);
        }
    }

    pub fn read_retro(&self) -> Option<Ref<'a, T>> {
        let prev_ptr = self.shared.previous.load(Ordering::Acquire);
        if prev_ptr.is_null() { return None; }
        let node = unsafe { &*prev_ptr };
        node.reader_count.fetch_add(REF_ONE, Ordering::Acquire);
        Some(Ref { node })
    }
}

#[derive(Clone)]
pub struct Reader<T> { shared: Arc<SharedState<T>>, }
impl<T> Reader<T> {
    pub fn read(&self) -> ReadResult<'_, T> {
        loop {
            let curr_val = self.shared.current.load(Ordering::Acquire);
            if (curr_val & TAG_MASK) == LOCKED { return ReadResult::Blocked(BlockedReader { shared: &self.shared }); }
            let ptr = (curr_val & PTR_MASK) as *mut Node<T>;
            let node = unsafe { &*ptr };
            node.reader_count.fetch_add(REF_ONE, Ordering::Acquire);
            let val_now = self.shared.current.load(Ordering::Acquire);
            if curr_val != val_now {
                node.reader_count.fetch_sub(REF_ONE, Ordering::Release);
                spin_loop(); continue;
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
    pub fn new(initial: T) -> (Self, Reader<T>) where T: Clone {
        assert!(align_of::<Node<T>>() >= 2);
        let node = Box::new(Node::new(initial));
        let ptr = Box::into_raw(node);

        let shared = Arc::new(SharedState {
            current: AtomicUsize::new(ptr as usize),
            previous: AtomicPtr::new(ptr::null_mut()),
            writer_notify: AtomicU32::new(0),
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
        // [优化点 2] 
        // 我们必须保留 garbage 队列中的最后一个元素。
        // 因为该元素就是当前的 shared.previous，Reader 可能会访问它。
        // 只有当 update 发生，新的节点被推入 garbage 后，这个节点变成了“前前一个”，才允许被回收。
        while self.garbage.len() > 1 {
            // 只看队首（最老的垃圾）
            if let Some(&ptr) = self.garbage.front() {
                let node = unsafe { &*ptr };
                if node.reader_count.load(Ordering::Acquire) == 0 {
                    self.garbage.pop_front(); // 移除
                    let node_box = unsafe { Box::from_raw(ptr) };
                    self.pool.push(node_box); // 进池
                } else {
                    // 最老的还有人在读，后面的肯定更新，直接退出
                    break;
                }
            }
        }
    }

    pub fn update<F, R>(&mut self, f: F) -> R
    where F: FnOnce(&mut T) -> R, T: Clone
    {
        self.collect_garbage();

        let curr_val = self.shared.current.load(Ordering::Acquire);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        let curr_node = unsafe { &*curr_ptr };
        let reader_cnt = curr_node.reader_count.load(Ordering::Acquire);

        // === Path A: In-Place Optimization ===
        if reader_cnt == 0 {
            let locked_val = curr_val | LOCKED;
            if self.shared.current.compare_exchange(curr_val, locked_val, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                // 再次检查
                if curr_node.reader_count.load(Ordering::Acquire) == 0 {
                    let result = unsafe { f(&mut *curr_node.data.get()) };
                    self.shared.current.store(curr_val & PTR_MASK, Ordering::Release);
                    self.shared.writer_notify.fetch_add(1, Ordering::Release);
                    wake_all(&self.shared.writer_notify);
                    return result;
                } else {
                    // 回滚
                    self.shared.current.store(curr_val, Ordering::Release);
                    self.shared.writer_notify.fetch_add(1, Ordering::Release);
                    wake_all(&self.shared.writer_notify);
                }
            }
        }

        // === Path B: COW with Optimization [优化点 1] ===
        let new_data = unsafe { (*curr_node.data.get()).clone() };
        
        // 从池中获取
        let mut new_node = if let Some(recycled_node) = self.pool.pop() {
            unsafe { *recycled_node.data.get() = new_data };
            recycled_node.reader_count.store(0, Ordering::Relaxed);
            recycled_node
        } else {
            Box::new(Node::new(new_data))
        };

        let result = f(new_node.data.get_mut());

        let new_ptr = Box::into_raw(new_node);
        let new_val = new_ptr as usize;

        // 1. 更新 Current 指针
        self.shared.current.store(new_val, Ordering::Release);

        // 2. 将旧的 current (curr_ptr) 放入 garbage 尾部
        // 此时，curr_ptr 实际上变成了 "previous" 节点
        self.garbage.push_back(curr_ptr);

        // 3. [优化核心] 更新 shared.previous
        // 不再使用 swap(curr_ptr) 这种昂贵操作。
        // 直接使用 store 发布我们刚刚放入 garbage 的那个指针。
        self.shared.previous.store(curr_ptr, Ordering::Release);

        // 注意：此时 garbage 的最后一个元素 == shared.previous
        // collect_garbage 中的 while self.garbage.len() > 1 保证了它不会被回收

        result
    }
}

impl<T> Drop for RetroCell<T> {
    fn drop(&mut self) {
        self.collect_garbage();
        // 处理 garbage 中剩余的节点
        while let Some(ptr) = self.garbage.pop_front() {
             unsafe { drop(Box::from_raw(ptr)); }
        }
    }
}