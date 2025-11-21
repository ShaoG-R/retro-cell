use retro_cell::{ReadResult, RetroCell, WriteOutcome};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

// Helper for simple updates that mimics the old update behavior:
// Try InPlace if possible (no readers), otherwise fallback to COW.
fn simple_update<T: Clone>(cell: &mut RetroCell<T>, f: impl FnOnce(&mut T)) {
    match cell.write() {
        WriteOutcome::InPlace(mut guard) => f(&mut guard),
        WriteOutcome::Congested(writer) => writer.perform_cow(f),
    }
}

#[test]
fn test_basic_usage() {
    let (mut writer, reader) = RetroCell::new(10);

    // Initial read
    if let ReadResult::Success(guard) = reader.read() {
        assert_eq!(*guard, 10);
    } else {
        panic!("Should read successfully");
    }

    // Update
    simple_update(&mut writer, |val| *val = 20);

    // Read after update
    if let ReadResult::Success(guard) = reader.read() {
        assert_eq!(*guard, 20);
    } else {
        panic!("Should read successfully");
    }
}

#[test]
fn test_inplace_update() {
    let (mut writer, reader) = RetroCell::new(100);

    let addr_1;
    {
        let guard = match reader.read() {
            ReadResult::Success(g) => g,
            _ => panic!("read failed"),
        };
        addr_1 = &*guard as *const i32;
    } // guard dropped, reader_count should be 0

    // Should be in-place because no readers
    match writer.write() {
        WriteOutcome::InPlace(mut guard) => {
            *guard = 101;
        }
        WriteOutcome::Congested(_) => panic!("Should be InPlace update when no readers are active"),
    }

    let addr_2;
    {
        let guard = match reader.read() {
            ReadResult::Success(g) => g,
            _ => panic!("read failed"),
        };
        assert_eq!(*guard, 101);
        addr_2 = &*guard as *const i32;
    }

    // In-place means addresses should be equal
    assert_eq!(addr_1, addr_2, "Should have performed in-place update");
}

#[test]
fn test_cow_update() {
    let (mut writer, reader) = RetroCell::new(200);

    let guard1 = match reader.read() {
        ReadResult::Success(g) => g,
        _ => panic!("read failed"),
    };
    let addr_1 = &*guard1 as *const i32;

    // guard1 is held, so reader_count > 0. Update should trigger Congested.
    match writer.write() {
        WriteOutcome::Congested(w) => {
            w.perform_cow(|val| *val = 201);
        }
        WriteOutcome::InPlace(_) => {
            panic!("Should be Congested (COW) update when readers are active")
        }
    }

    let guard2 = match reader.read() {
        ReadResult::Success(g) => g,
        _ => panic!("read failed"),
    };
    let addr_2 = &*guard2 as *const i32;

    assert_eq!(*guard1, 200); // Old value preserved
    assert_eq!(*guard2, 201); // New value visible

    assert_ne!(addr_1, addr_2, "Should have performed COW update");
}

#[test]
fn test_blocked_reader() {
    let (mut writer, reader) = RetroCell::new(300);

    let barrier = Arc::new(Barrier::new(2));
    let barrier_c = barrier.clone();

    let t = thread::spawn(move || {
        // We want to ensure InPlace update to block readers
        match writer.write() {
            WriteOutcome::InPlace(mut guard) => {
                *guard = 301;
                barrier_c.wait(); // Wait for main thread to try reading
                thread::sleep(Duration::from_millis(100));
            }
            WriteOutcome::Congested(_) => {
                panic!("Writer should not be congested here (no readers)")
            }
        }
    });

    // Wait for writer to enter the closure
    barrier.wait();
    // Writer is now sleeping inside the update closure with LOCKED bit set.

    match reader.read() {
        ReadResult::Success(_) => {
            panic!("Should have encountered Blocked because writer is locked in in-place update");
        }
        ReadResult::Blocked(handler) => {
            // Previous is null/empty initially
            let old = handler.read_retro();
            assert!(old.is_none());

            // Wait for update to finish
            let res = handler.wait();
            if let ReadResult::Success(g) = res {
                assert_eq!(*g, 301);
            } else {
                panic!("Wait should return success");
            }
        }
    }

    t.join().unwrap();
}

#[test]
fn test_read_retro_during_cow() {
    let (mut writer, reader) = RetroCell::new(10);

    // Step 1: COW update to establish a previous version
    {
        let _g = reader.read(); // Hold read to force COW
        match writer.write() {
            WriteOutcome::Congested(w) => w.perform_cow(|v| *v = 20),
            WriteOutcome::InPlace(_) => panic!("Should be Congested"),
        }
    }
    // Now: Current = 20 (Node B), Previous = 10 (Node A).

    // Step 2: In-Place update that hangs
    let barrier = Arc::new(Barrier::new(2));
    let barrier_c = barrier.clone();

    let t = thread::spawn(move || {
        match writer.write() {
            WriteOutcome::InPlace(mut g) => {
                *g = 30;
                barrier_c.wait(); // Wait for reader
                thread::sleep(Duration::from_millis(100));
            }
            WriteOutcome::Congested(_) => panic!("Should be InPlace"),
        }
    });

    barrier.wait();

    // Now writer is locked on Node B.
    match reader.read() {
        ReadResult::Blocked(handler) => {
            // read_retro should return the *previous* valid node (Node A = 10)
            let old_guard = handler.read_retro().expect("Should have old value");
            assert_eq!(*old_guard, 10);

            if let ReadResult::Success(new_g) = handler.wait() {
                assert_eq!(*new_g, 30);
            }
        }
        _ => panic!("Should be Blocked"),
    }

    t.join().unwrap();
}

#[test]
fn test_concurrency_stress() {
    let (mut writer, reader) = RetroCell::new(0usize);
    let reader_cnt = 10;
    let mut handles = vec![];
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));

    for _ in 0..reader_cnt {
        let r = reader.clone();
        let run = running.clone();
        handles.push(thread::spawn(move || {
            while run.load(std::sync::atomic::Ordering::Relaxed) {
                match r.read() {
                    ReadResult::Success(g) => {
                        let _ = *g;
                    }
                    ReadResult::Blocked(h) => {
                        if let ReadResult::Success(g) = h.wait() {
                            let _ = *g;
                        }
                    }
                }
                thread::yield_now();
            }
        }));
    }

    for i in 1..1000 {
        simple_update(&mut writer, |v| *v = i);
        if i % 100 == 0 {
            thread::yield_now();
        }
    }

    running.store(false, std::sync::atomic::Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
}
