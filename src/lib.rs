use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::mem::align_of;
use std::ops::Deref;
use std::ptr::{self};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

// === Constants ===
// Low bit used for state tagging
const TAG_MASK: usize = 0b1;        // Bit 0 for lock
const PTR_MASK: usize = !TAG_MASK;  // Remaining bits for pointer
const LOCKED: usize = 0b1;          // Locked state value

// Ref count unit
const REF_ONE: usize = 1;

/// Data Node
struct Node<T> {
    data: UnsafeCell<T>,
    /// Reference count for readers
    reader_count: AtomicUsize,
}

impl<T> Node<T> {
    fn new(data: T) -> Self {
        Self {
            data: UnsafeCell::new(data),
            reader_count: AtomicUsize::new(0),
        }
    }
}

/// Shared internal state
struct SharedState<T> {
    /// Storage: (Raw Pointer | Lock Bit)
    current: AtomicUsize,

    /// Previous Epoch (Snapshot), no Tag
    previous: AtomicPtr<Node<T>>,

    notifier_lock: Mutex<()>,
    notifier_cond: Condvar,
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

/// Result of a read operation
pub enum ReadResult<'a, T> {
    Success(Ref<'a, T>),
    Blocked(BlockedReader<'a, T>),
}

/// Handler for when the writer is active
pub struct BlockedReader<'a, T> {
    shared: &'a SharedState<T>,
    reader: &'a Reader<T>,
}

impl<'a, T> BlockedReader<'a, T> {
    /// Wait for the writer to finish and retry reading
    pub fn wait(self) -> ReadResult<'a, T> {
        let mut guard = self.shared.notifier_lock.lock().unwrap();
        loop {
            let val = self.shared.current.load(Ordering::Acquire);
            // Check Tag Bit
            if (val & TAG_MASK) == 0 {
                drop(guard);
                return self.reader.read();
            }
            guard = self.shared.notifier_cond.wait(guard).unwrap();
        }
    }

    /// Read the previous version of the data (Retro Read)
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
            // 1. Load tagged pointer
            let curr_val = self.shared.current.load(Ordering::Acquire);

            // 2. Optimization: Check lock bit immediately
            // If locked, return Blocked immediately without accessing Node memory
            if (curr_val & TAG_MASK) == LOCKED {
                return ReadResult::Blocked(BlockedReader {
                    shared: &self.shared,
                    reader: self
                });
            }

            let ptr = (curr_val & PTR_MASK) as *mut Node<T>;
            let node = unsafe { &*ptr };

            // 3. Increment ref count
            node.reader_count.fetch_add(REF_ONE, Ordering::Acquire);

            // 4. Double check
            let val_now = self.shared.current.load(Ordering::Acquire);
            if curr_val != val_now {
                // Pointer changed or Lock bit set by writer
                node.reader_count.fetch_sub(REF_ONE, Ordering::Release);
                continue;
            }

            return ReadResult::Success(Ref { node });
        }
    }
}

pub struct RetroCell<T> {
    shared: Arc<SharedState<T>>,
    garbage: VecDeque<*mut Node<T>>,
}

unsafe impl<T: Send + Sync> Send for RetroCell<T> {}

impl<T> RetroCell<T> {
    /// Create a new RetroCell and its associated Reader
    pub fn new(initial: T) -> (Self, Reader<T>)
    where T: Clone 
    {
        // Ensure alignment for tagging
        assert!(align_of::<Node<T>>() >= 2, "Node<T> alignment must be >= 2 for tagging");

        let node = Box::new(Node::new(initial));
        let ptr = Box::into_raw(node);

        let shared = Arc::new(SharedState {
            current: AtomicUsize::new(ptr as usize),
            previous: AtomicPtr::new(ptr::null_mut()),
            notifier_lock: Mutex::new(()),
            notifier_cond: Condvar::new(),
        });

        let writer = RetroCell { shared: shared.clone(), garbage: VecDeque::new() };
        let reader = Reader { shared };
        (writer, reader)
    }

    fn collect_garbage(&mut self) {
        while let Some(&ptr) = self.garbage.front() {
            let node = unsafe { &*ptr };
            if node.reader_count.load(Ordering::Acquire) == 0 {
                self.garbage.pop_front();
                unsafe { drop(Box::from_raw(ptr)); }
            } else {
                break;
            }
        }
    }

    pub fn update<F, R>(&mut self, f: F) -> R
    where F: FnOnce(&mut T) -> R, T: Clone
    {
        self.collect_garbage();

        // Load current value
        let curr_val = self.shared.current.load(Ordering::Acquire);
        let curr_ptr = (curr_val & PTR_MASK) as *mut Node<T>;
        let curr_node = unsafe { &*curr_ptr };

        let reader_cnt = curr_node.reader_count.load(Ordering::Acquire);

        // === Path A: In-Place Optimization ===
        // Condition: No readers, and not locked
        if reader_cnt == 0 {
            // 1. Try to tag current pointer with LOCKED
            let locked_val = curr_val | LOCKED;
            if self.shared.current.compare_exchange(
                curr_val,
                locked_val,
                Ordering::Acquire,
                Ordering::Relaxed
            ).is_ok() {
                // Locked!

                // 2. Double Check readers
                if curr_node.reader_count.load(Ordering::Acquire) == 0 {
                    // Safe to mutate
                    let result = unsafe { f(&mut *curr_node.data.get()) };

                    // Unlock
                    self.shared.current.store(curr_val & PTR_MASK, Ordering::Release);

                    // Notify
                    let _g = self.shared.notifier_lock.lock().unwrap();
                    self.shared.notifier_cond.notify_all();
                    return result;
                } else {
                    // Failed double check, rollback lock
                    self.shared.current.store(curr_val, Ordering::Release);
                }
            }
        }

        // === Path B: COW ===
        let new_data = unsafe { (*curr_node.data.get()).clone() };
        let mut new_node = Box::new(Node::new(new_data));
        let result = f(new_node.data.get_mut());

        let new_ptr = Box::into_raw(new_node);
        let new_val = new_ptr as usize; // Aligned, Tag 0

        // Swap Previous
        let old_prev = self.shared.previous.swap(curr_ptr, Ordering::AcqRel);

        // Store New Current
        self.shared.current.store(new_val, Ordering::Release);

        if !old_prev.is_null() {
            self.garbage.push_back(old_prev);
        }

        result
    }
}

impl<T> Drop for RetroCell<T> {
    fn drop(&mut self) {
        self.collect_garbage();
        // Note: Potential memory leak if readers hold references to garbage nodes indefinitely
    }
}