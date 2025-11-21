use retro_cell::{ReadResult, RetroCell};
use std::thread;
use std::time::Duration;

#[test]
fn test_mixed_read_write_deadlock_repro() {
    let (mut writer, reader) = RetroCell::new(vec![0u32; 64]);
    let num_readers = 4;
    let ratio = 20;
    // Use enough iterations to trigger the race, but not take forever
    let iters = 50_000;
    let write_iters = (iters * num_readers as u64) / ratio as u64;

    let (tx, rx) = std::sync::mpsc::channel();

    let t = thread::spawn(move || {
        thread::scope(|s| {
            let w = &mut writer;
            if write_iters > 0 {
                s.spawn(move || {
                    for _ in 0..write_iters {
                        w.update(|v| v[0] = v[0].wrapping_add(1));
                    }
                });
            }

            for _ in 0..num_readers {
                let r = reader.clone();
                s.spawn(move || {
                    for _ in 0..iters {
                        match r.read() {
                            ReadResult::Success(_ref) => {
                                std::hint::black_box(_ref);
                            }
                            ReadResult::Blocked(blocked) => {
                                std::hint::black_box(blocked.wait());
                            }
                        }
                    }
                });
            }
        });
        tx.send(()).unwrap();
    });

    // Wait for thread with timeout
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(_) => t.join().unwrap(),
        Err(_) => panic!("Deadlock detected! Test did not finish in 30 seconds."),
    }
}
