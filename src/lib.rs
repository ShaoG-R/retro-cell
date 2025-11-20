use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::mem::align_of;
use std::ops::{Deref, DerefMut};
use std::ptr::{self};
use std::sync::atomic::{AtomicPtr, AtomicUsize, AtomicU32, Ordering};
use std::sync::Arc;
use std::hint::spin_loop; // [Improvement 4] 引入自旋提示
use atomic_wait::{wait, wake_all};

// === Constants ===
const TAG_MASK: usize = 0b1;
const PTR_MASK: usize = !TAG_MASK;
const LOCKED: usize = 0b1;
const REF_ONE: usize = 1;

// === [Improvement 1] Cache Padding ===
// 强制对齐到 64 字节，防止 False Sharing
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

/// Data Node
struct Node<T> {
    data: UnsafeCell<T>,
    /// [Improvement 1] 使用 CachePadded 隔离热点
    /// data (被 Writer 写) 和 reader_count (被 Reader 写) 现在处于不同的缓存行
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

/// Shared internal state
struct SharedState<T> {
    current: AtomicUsize,
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

        let prev = self.previous.load(Ordering::Relaxed);
        if !prev.is_null() { unsafe { let _ = Box::from_raw(prev); } }
    }
}

/// Read Guard (RAII)
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
            
            // Check if unlocked
            if (val & TAG_MASK) == 0 {
                let ptr = (val & PTR_MASK) as *mut Node<T>;
                let node = unsafe { &*ptr };
                node.reader_count.fetch_add(REF_ONE, Ordering::Acquire);

                if self.shared.current.load(Ordering::Acquire) == val {
                    return ReadResult::Success(Ref { node });
                }
                node.reader_count.fetch_sub(REF_ONE, Ordering::Release);
                
                // [Improvement 4] 失败重试时，让出 CPU 时间片
                spin_loop();
                continue;
            }

            let epoch = self.shared.writer_notify.load(Ordering::Acquire);
            val = self.shared.current.load(Ordering::Acquire);
            if (val & TAG_MASK) == 0 {
                 continue;
            }

            wait(&self.shared.writer_notify, epoch);
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
                
                // [Improvement 4] 减少竞争时的总线压力
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
    /// [Improvement 2] 对象池：复用已回收的节点内存
    pool: Vec<Box<Node<T>>>,
}

unsafe impl<T: Send + Sync> Send for RetroCell<T> {}

impl<T> RetroCell<T> {
    pub fn new(initial: T) -> (Self, Reader<T>)
    where T: Clone 
    {
        assert!(align_of::<Node<T>>() >= 2, "Node alignment < 2");

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
            pool: Vec::new(), // [Improvement 2] 初始化池
        };
        let reader = Reader { shared };
        (writer, reader)
    }

    fn collect_garbage(&mut self) {
        while let Some(&ptr) = self.garbage.front() {
            let node = unsafe { &*ptr };
            if node.reader_count.load(Ordering::Acquire) == 0 {
                self.garbage.pop_front();
                
                // [Improvement 2] 不要 drop，而是重置并放入池中
                // 注意：此时 Node 里的 data 仍然是旧数据，会在下一次重用时被覆盖(drop)
                let node_box = unsafe { Box::from_raw(ptr) };
                self.pool.push(node_box);
            } else {
                break;
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
            if self.shared.current.compare_exchange(
                curr_val,
                locked_val,
                Ordering::Acquire,
                Ordering::Relaxed
            ).is_ok() {
                if curr_node.reader_count.load(Ordering::Acquire) == 0 {
                    let result = unsafe { f(&mut *curr_node.data.get()) };
                    self.shared.current.store(curr_val & PTR_MASK, Ordering::Release);
                    self.shared.writer_notify.fetch_add(1, Ordering::Release);
                    wake_all(&self.shared.writer_notify);
                    return result;
                } else {
                    self.shared.current.store(curr_val, Ordering::Release);
                    self.shared.writer_notify.fetch_add(1, Ordering::Release);
                    wake_all(&self.shared.writer_notify);
                }
            }
        }

        // === Path B: COW with Object Pooling [Improvement 2] ===
        let new_data = unsafe { (*curr_node.data.get()).clone() };
        
        let mut new_node = if let Some(recycled_node) = self.pool.pop() {
            // 重用节点：需要覆盖旧数据
            // *recycled_node.data.get() = new_data; 这会自动 drop 旧数据
            unsafe { *recycled_node.data.get() = new_data };
            // 重置引用计数（虽然进池前应该是0，但安全起见）
            recycled_node.reader_count.store(0, Ordering::Relaxed);
            recycled_node
        } else {
            // 池为空，分配新内存
            Box::new(Node::new(new_data))
        };

        // 在新节点上应用修改
        let result = f(new_node.data.get_mut());

        let new_ptr = Box::into_raw(new_node);
        let new_val = new_ptr as usize;

        let old_prev = self.shared.previous.swap(curr_ptr, Ordering::AcqRel);
        self.shared.current.store(new_val, Ordering::Release);

        if !old_prev.is_null() {
            self.garbage.push_back(old_prev);
        }

        result
    }
}

impl<T> Drop for RetroCell<T> {
    fn drop(&mut self) {
        // 清理 garbage 队列
        self.collect_garbage();
        // 此时 pool 中的节点会自动被 Drop 释放，包含其中的 data
        // garbage 中剩余的节点（仍有 reader 的）也会被漏掉（和原版行为一致，存在 Leak 风险，
        // 真正的生产环境需要在 Drop 时自旋等待所有 reader 退出或者泄漏这部分内存）
    }
}