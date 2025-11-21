use retro_cell::{ReadResult, RetroCell, WriteOutcome};
use std::sync::{atomic::{AtomicUsize, Ordering}, Arc, Barrier};
use std::thread;
use std::time::Duration;

// ============================================================================
// 1. Basic Tests
// ============================================================================

#[test]
fn test_basic_read_write_inplace() {
    let (mut cell, reader) = RetroCell::new(42);
    assert_eq!(*reader.read(), 42);

    // With no active readers, try_write should give InPlaceGuard
    match cell.try_write() {
        WriteOutcome::InPlace(mut guard) => {
            *guard = 100;
        }
        WriteOutcome::Congested(_) => panic!("Should be in-place when no readers"),
    }
    assert_eq!(*reader.read(), 100);
}

#[test]
fn test_basic_cow() {
    let (mut cell, reader) = RetroCell::new(vec![1, 2]);
    
    // Cow write
    cell.write_cow(|v| v.push(3));
    
    assert_eq!(*reader.read(), vec![1, 2, 3]);
}

// ============================================================================
// 2. Advanced Tests (COW, In-Place, Retro)
// ============================================================================

#[test]
fn test_write_cow_congested() {
    let (mut cell, reader) = RetroCell::new(10);
    
    // Hold a read reference to force congestion
    let ref1 = reader.read();
    assert_eq!(*ref1, 10);

    // try_write should return Congested because ref1 is active
    match cell.try_write() {
        WriteOutcome::Congested(writer) => {
            writer.perform_cow(|v| *v = 20);
        }
        WriteOutcome::InPlace(_) => panic!("Should be congested"),
    }

    // New reader sees new value
    assert_eq!(*reader.read(), 20);
    // Old reader still sees old value
    assert_eq!(*ref1, 10);
}

#[test]
fn test_force_in_place_blocking() {
    let (mut cell, reader) = RetroCell::new(0);
    let reader_clone = reader.clone();

    let t = thread::spawn(move || {
        let _r = reader_clone.read();
        thread::sleep(Duration::from_millis(100));
    });

    // Give thread time to acquire read lock
    thread::sleep(Duration::from_millis(20));

    // This should block until thread releases read lock
    let start = std::time::Instant::now();
    let mut guard = cell.write_in_place();
    *guard = 1;
    let duration = start.elapsed();

    assert!(duration >= Duration::from_millis(50), "Should have blocked");
    drop(guard);
    t.join().unwrap();
    assert_eq!(*reader.read(), 1);
}

#[test]
fn test_read_retro_when_locked() {
    let (mut cell, reader) = RetroCell::new(10);
    
    // Initial COW update to populate 'previous'
    cell.write_cow(|v| *v = 20); 
    // current=20, previous=10
    
    let reader_clone = reader.clone();
    let barrier = Arc::new(Barrier::new(2));
    let b_clone = barrier.clone();

    let t = thread::spawn(move || {
        b_clone.wait(); // Wait until main thread locks
        match reader_clone.try_read() {
            ReadResult::Blocked(blocked) => {
                // Can read retro (previous value)
                if let Some(val) = blocked.read_retro() {
                    assert_eq!(*val, 10);
                } else {
                    panic!("Should have retro value");
                }
                
                // Wait for lock to release
                let val = blocked.wait();
                assert_eq!(*val, 30);
            }
            ReadResult::Success(_) => panic!("Should be blocked"),
        }
    });

    // Lock in place
    let mut guard = cell.write_in_place();
    barrier.wait();
    thread::sleep(Duration::from_millis(50));
    *guard = 30;
    drop(guard);
    
    t.join().unwrap();
}

// ============================================================================
// 3. Concurrency Tests
// ============================================================================

#[test]
fn test_concurrent_reads_and_cow_writes() {
    let (mut cell, reader) = RetroCell::new(0);
    let thread_count = 10;
    let iterations = 100;
    let mut handles = vec![];

    for _ in 0..thread_count {
        let r = reader.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..iterations {
                let val = r.read();
                // Value should be monotonically increasing (or equal)
                // but since we are reading concurrently, we can't strictly assert previous < current
                // just ensure it's readable.
                assert!(*val >= 0);
                thread::sleep(Duration::from_micros(10));
            }
        }));
    }

    for i in 0..iterations {
        cell.write_cow(|v| *v = i);
        thread::sleep(Duration::from_micros(10));
    }

    for h in handles {
        h.join().unwrap();
    }
}

// ============================================================================
// 4. Deadlock Tests
// ============================================================================

#[test]
fn test_deadlock_reader_holds_writer_waits() {
    // Ensure that if a reader holds a ref, writer waiting for in-place doesn't cause a deadlock
    // provided the reader eventually releases it.
    let (mut cell, reader) = RetroCell::new(0);
    let reader2 = reader.clone();
    
    let barrier = Arc::new(Barrier::new(2));
    let b2 = barrier.clone();

    let t = thread::spawn(move || {
        let _r = reader2.read();
        b2.wait(); // Signal lock acquired
        thread::sleep(Duration::from_millis(50));
        // drop(_r) happens at end of scope
    });

    barrier.wait();
    let mut guard = cell.write_in_place();
    *guard = 1;
    
    t.join().unwrap();
}

// ============================================================================
// 5. Boundary/Edge Cases
// ============================================================================

#[derive(Clone)]
struct Tracked {
    _id: usize,
    counter: Arc<AtomicUsize>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.counter.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn test_garbage_collection() {
    let drop_count = Arc::new(AtomicUsize::new(0));
    let (mut cell, _reader) = RetroCell::new(Tracked { 
        _id: 0, 
        counter: drop_count.clone() 
    });

    // Create many updates
    for i in 1..=100 {
        cell.write_cow(|t| {
            // Create new value
            *t = Tracked { _id: i, counter: drop_count.clone() };
        });
    }

    // Do one more write to trigger GC
    cell.write_cow(|_| {});

    // We created: 1 initial + 100 updates = 101 values.
    // The current value is alive. The previous value is kept (for retro reads).
    // So 1 current + 1 previous = 2 alive.
    // Expected drops: 101 - 2 = 99.
    
    let dropped = drop_count.load(Ordering::SeqCst);
    assert!(dropped >= 90, "Expected ~99 drops, got {}", dropped);
}

#[test]
fn test_no_retro_available() {
    let (_cell, reader) = RetroCell::new(1);
    // No updates yet, so no previous value
    assert!(reader.read_retro().is_none());
}
