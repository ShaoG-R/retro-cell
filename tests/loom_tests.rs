#![cfg(feature = "loom")]

use loom::model::Builder;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;
use retro_cell::{ReadResult, RetroCell, WriteOutcome};

#[test]
fn test_concurrent_read_write_cow() {
    loom::model(|| {
        let (mut cell, reader) = RetroCell::new(0usize);

        let t1 = thread::spawn({
            let reader = reader.clone();
            move || {
                let guard = reader.read();
                let val = *guard;
                assert!(val == 0 || val == 1);
            }
        });

        cell.write_cow(|val| *val = 1);

        t1.join().unwrap();
    });
}

#[test]
fn test_concurrent_read_write_in_place() {
    loom::model(|| {
        let (mut cell, reader) = RetroCell::new(0usize);

        let t1 = thread::spawn({
            let reader = reader.clone();
            move || {
                let guard = reader.read();
                let val = *guard;
                assert!(val == 0 || val == 1);
            }
        });

        let mut guard = cell.write_in_place();
        *guard = 1;
        drop(guard);

        t1.join().unwrap();
    });
}

#[test]
fn test_mutex_concurrent_writes() {
    let mut builder = Builder::new();
    builder.preemption_bound = Some(4);
    builder.check(|| {
        let (cell, reader) = RetroCell::new(0usize);
        let cell = Arc::new(Mutex::new(cell));

        let t1 = thread::spawn({
            let cell = cell.clone();
            move || {
                let mut guard = cell.lock().unwrap();
                guard.write_cow(|val| *val += 1);
            }
        });

        let t2 = thread::spawn({
            let cell = cell.clone();
            move || {
                let mut guard = cell.lock().unwrap();
                // Use in-place for variety
                let mut ig = guard.write_in_place();
                *ig += 1;
            }
        });

        let t3 = thread::spawn({
            let reader = reader.clone();
            move || {
                let val = *reader.read();
                assert!(val <= 2);
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();
        t3.join().unwrap();

        assert_eq!(*reader.read(), 2);
    });
}

#[test]
fn test_blocked_reader() {
    loom::model(|| {
        let (mut cell, reader) = RetroCell::new(0usize);

        let t1 = thread::spawn({
            let reader = reader.clone();
            move || {
                let val = *reader.read();
                assert!(val == 0 || val == 1);
            }
        });

        // Lock and hold
        let mut guard = cell.write_in_place();
        *guard = 1;
        thread::yield_now(); // Give reader a chance to hit the lock
        drop(guard);

        t1.join().unwrap();
    });
}

#[test]
fn test_retro_read() {
    loom::model(|| {
        let (mut cell, reader) = RetroCell::new(0usize);

        cell.write_cow(|val| *val = 1);

        let retro = reader.read_retro();
        assert!(retro.is_some());
        assert_eq!(*retro.unwrap(), 0);

        let current = reader.read();
        assert_eq!(*current, 1);
    });
}

#[test]
fn test_try_write_success() {
    loom::model(|| {
        let (mut cell, reader) = RetroCell::new(0usize);

        match cell.try_write() {
            WriteOutcome::InPlace(mut guard) => {
                *guard = 10;
            }
            WriteOutcome::Congested(_) => {
                panic!("Should be in-place as there are no readers");
            }
        }

        assert_eq!(*reader.read(), 10);
    });
}

#[test]
fn test_try_write_congested() {
    loom::model(|| {
        let (mut cell, reader) = RetroCell::new(0usize);
        let flag = Arc::new(AtomicUsize::new(0));

        let t1 = thread::spawn({
            let reader = reader.clone();
            let flag = flag.clone();
            move || {
                let _guard = reader.read();
                flag.store(1, Ordering::Release);
                // Wait for main thread to check
                while flag.load(Ordering::Acquire) != 2 {
                    thread::yield_now();
                }
                drop(_guard);
            }
        });

        // Wait for reader to acquire
        while flag.load(Ordering::Acquire) != 1 {
            thread::yield_now();
        }

        // Now try_write should fail to lock in-place because reader count > 0
        match cell.try_write() {
            WriteOutcome::Congested(_) => {
                // Expected
            }
            WriteOutcome::InPlace(_) => {
                panic!("Should be congested due to active reader");
            }
        }

        // Signal reader to finish
        flag.store(2, Ordering::Release);

        t1.join().unwrap();
    });
}

#[test]
fn test_concurrent_churn() {
    let mut builder = Builder::new();
    builder.preemption_bound = Some(2);
    builder.check(|| {
        let (mut cell, reader) = RetroCell::new(0usize);

        let t1 = thread::spawn({
            let reader = reader.clone();
            move || {
                for _ in 0..2 {
                    let val = *reader.read();
                    assert!(val <= 2);
                }
            }
        });

        for i in 1..3 {
            cell.write_cow(|val| *val = i);
        }

        t1.join().unwrap();
    });
}

#[test]
fn test_try_read_blocked() {
    loom::model(|| {
        let (mut cell, reader) = RetroCell::new(0usize);
        let flag = Arc::new(AtomicUsize::new(0));

        let t1 = thread::spawn({
            let reader = reader.clone();
            let flag = flag.clone();
            move || {
                // Wait for writer to lock
                while flag.load(Ordering::Acquire) != 1 {
                    thread::yield_now();
                }

                match reader.try_read() {
                    ReadResult::Blocked(_) => {
                        // Expected
                    }
                    ReadResult::Success(_) => {
                        panic!("Should be blocked");
                    }
                }

                flag.store(2, Ordering::Release);
            }
        });

        let guard = cell.write_in_place();
        flag.store(1, Ordering::Release);

        // Wait for reader to check
        while flag.load(Ordering::Acquire) != 2 {
            thread::yield_now();
        }
        drop(guard);

        t1.join().unwrap();
    });
}
