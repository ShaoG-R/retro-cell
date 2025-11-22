use arc_swap::ArcSwap;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use retro_cell::{ReadResult, RetroCell};
use std::hint::black_box;
use std::sync::{Arc, Barrier, Mutex, RwLock};
use std::thread;

const DATA_SIZE: usize = 64;
type Data = Vec<u32>;

fn create_data() -> Data {
    vec![0_u32; DATA_SIZE]
}

#[inline(always)]
fn do_work(data: &Data) {
    black_box(data);
}

// --- Benchmarks ---

fn bench_primitive_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("Primitive Creation");

    group.bench_function("RetroCell", |b| {
        b.iter(|| {
            let (writer, reader) = RetroCell::new(create_data());
            black_box((writer, reader));
        })
    });

    group.bench_function("ArcSwap", |b| {
        b.iter(|| {
            let s = ArcSwap::new(Arc::new(create_data()));
            black_box(s);
        })
    });

    group.bench_function("RwLock", |b| {
        b.iter(|| {
            let l = RwLock::new(create_data());
            black_box(l);
        })
    });

    group.finish();
}

fn bench_single_thread_ops(c: &mut Criterion) {
    let mut group = c.benchmark_group("Single Thread Ops");

    // Read
    group.bench_function("Read/RetroCell", |b| {
        let (_writer, reader) = RetroCell::new(create_data());
        b.iter(|| match reader.try_read() {
            ReadResult::Success(r) => do_work(&r),
            ReadResult::Blocked(blocked) => do_work(&blocked.wait()),
        })
    });

    group.bench_function("Read/ArcSwap", |b| {
        let s = ArcSwap::new(Arc::new(create_data()));
        b.iter(|| {
            let guard = s.load();
            do_work(&guard);
        })
    });

    group.bench_function("Read/RwLock", |b| {
        let l = RwLock::new(create_data());
        b.iter(|| {
            let guard = l.read().unwrap();
            do_work(&guard);
        })
    });

    // Write
    group.bench_function("Write/RetroCell", |b| {
        let (mut writer, _reader) = RetroCell::new(create_data());
        b.iter(|| {
            writer.write_cow(|v| {
                v[0] = v[0].wrapping_add(1);
            });
        })
    });

    group.bench_function("Write/ArcSwap", |b| {
        let s = ArcSwap::new(Arc::new(create_data()));
        b.iter(|| {
            let mut new_val = (**s.load()).clone();
            new_val[0] = new_val[0].wrapping_add(1);
            s.store(Arc::new(new_val));
        })
    });

    group.bench_function("Write/RwLock", |b| {
        let l = RwLock::new(create_data());
        b.iter(|| {
            let mut guard = l.write().unwrap();
            guard[0] = guard[0].wrapping_add(1);
        })
    });

    group.finish();
}

// Helper for running concurrent benchmarks
fn run_concurrent_bench<W, R, WOp, ROp>(
    b: &mut criterion::Bencher,
    writers: usize,
    readers: usize,
    setup: impl Fn() -> (W, R),
    writer_op: WOp,
    reader_op: ROp,
) where
    W: Send + Sync + 'static + Clone,
    R: Send + Sync + 'static + Clone,
    WOp: Fn(&W) + Send + Sync + Copy + 'static,
    ROp: Fn(&R) + Send + Sync + Copy + 'static,
{
    let (writer, reader) = setup();
    let total_threads = writers + readers;

    b.iter_custom(|iters| {
        let start_barrier = Arc::new(Barrier::new(total_threads + 1));
        let end_barrier = Arc::new(Barrier::new(total_threads + 1));
        let mut handles = Vec::with_capacity(total_threads);

        // Writers
        for _ in 0..writers {
            let w = writer.clone();
            let sb = start_barrier.clone();
            let eb = end_barrier.clone();
            handles.push(thread::spawn(move || {
                sb.wait();
                for _ in 0..iters {
                    writer_op(&w);
                }
                eb.wait();
            }));
        }

        // Readers
        for _ in 0..readers {
            let r = reader.clone();
            let sb = start_barrier.clone();
            let eb = end_barrier.clone();
            handles.push(thread::spawn(move || {
                sb.wait();
                for _ in 0..iters {
                    reader_op(&r);
                }
                eb.wait();
            }));
        }

        start_barrier.wait();
        let start = std::time::Instant::now();
        end_barrier.wait();
        let elapsed = start.elapsed();

        for h in handles {
            h.join().unwrap();
        }
        elapsed
    })
}

// Scenario 1: Blocking Write, Blocking Read
// This tests the "In-Place" write path for RetroCell, comparable to RwLock.
fn bench_blocking_write_blocking_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("BlockingWrite_BlockingRead");
    
    // Scenarios: 1W/1R, 1W/4R, 2W/2R, 4W/4R
    let scenarios = [(1, 1), (1, 4), (2, 2), (4, 4)];

    for (writers, readers) in scenarios {
        let label = format!("{}W/{}R", writers, readers);
        group.throughput(Throughput::Elements((writers + readers) as u64));

        // RetroCell
        group.bench_with_input(BenchmarkId::new("RetroCell", &label), &(writers, readers), |b, &(_w, _r)| {
            run_concurrent_bench(
                b, writers, readers,
                || {
                    let (writer, reader) = RetroCell::new(create_data());
                    (Arc::new(Mutex::new(writer)), reader)
                },
                |w| {
                    let mut guard = w.lock().unwrap();
                    let mut g = guard.write_in_place();
                    g[0] = g[0].wrapping_add(1);
                },
                |r| {
                    let val = r.read();
                    do_work(&val);
                }
            )
        });

        // RwLock
        group.bench_with_input(BenchmarkId::new("RwLock", &label), &(writers, readers), |b, &(_w, _r)| {
            run_concurrent_bench(
                b, writers, readers,
                || {
                    let l = Arc::new(RwLock::new(create_data()));
                    (l.clone(), l)
                },
                |l| {
                    let mut guard = l.write().unwrap();
                    guard[0] = guard[0].wrapping_add(1);
                },
                |l| {
                    let guard = l.read().unwrap();
                    do_work(&guard);
                }
            )
        });
    }
    group.finish();
}

// Scenario 2: Non-blocking Write, Blocking Read
// This tests the "COW" write path for RetroCell, comparable to ArcSwap.
fn bench_nonblocking_write_blocking_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("NonBlockingWrite_BlockingRead");
    
    let scenarios = [(1, 1), (1, 4), (2, 2), (4, 4)];

    for (writers, readers) in scenarios {
        let label = format!("{}W/{}R", writers, readers);
        group.throughput(Throughput::Elements((writers + readers) as u64));

        // RetroCell
        group.bench_with_input(BenchmarkId::new("RetroCell", &label), &(writers, readers), |b, &(_w, _r)| {
            run_concurrent_bench(
                b, writers, readers,
                || {
                    let (writer, reader) = RetroCell::new(create_data());
                    (Arc::new(Mutex::new(writer)), reader)
                },
                |w| {
                    // Use explicit write_cow to force non-blocking path
                    w.lock().unwrap().write_cow(|v| {
                        v[0] = v[0].wrapping_add(1);
                    });
                },
                |r| {
                    let val = r.read();
                    do_work(&val);
                }
            )
        });

        // ArcSwap
        group.bench_with_input(BenchmarkId::new("ArcSwap", &label), &(writers, readers), |b, &(_w, _r)| {
            run_concurrent_bench(
                b, writers, readers,
                || {
                    let s = Arc::new(ArcSwap::new(Arc::new(create_data())));
                    (s.clone(), s)
                },
                |s| {
                    let mut new_val = (**s.load()).clone();
                    new_val[0] = new_val[0].wrapping_add(1);
                    s.store(Arc::new(new_val));
                },
                |s| {
                    do_work(&s.load());
                }
            )
        });
    }
    group.finish();
}

// Scenario 3: Non-blocking Write, Non-blocking Read
// This tests wait-free/lock-free paths.
fn bench_nonblocking_write_nonblocking_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("NonBlockingWrite_NonBlockingRead");
    
    let scenarios = [(1, 1), (1, 4), (2, 2), (4, 4)];

    for (writers, readers) in scenarios {
        let label = format!("{}W/{}R", writers, readers);
        group.throughput(Throughput::Elements((writers + readers) as u64));

        // RetroCell
        group.bench_with_input(BenchmarkId::new("RetroCell", &label), &(writers, readers), |b, &(_w, _r)| {
            run_concurrent_bench(
                b, writers, readers,
                || {
                    let (writer, reader) = RetroCell::new(create_data());
                    (Arc::new(Mutex::new(writer)), reader)
                },
                |w| {
                    w.lock().unwrap().write_cow(|v| {
                        v[0] = v[0].wrapping_add(1);
                    });
                },
                |r| {
                    // Spin until read success to simulate non-blocking attempt loop
                    loop {
                        if let ReadResult::Success(val) = r.try_read() {
                            do_work(&val);
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            )
        });

        // ArcSwap
        group.bench_with_input(BenchmarkId::new("ArcSwap", &label), &(writers, readers), |b, &(_w, _r)| {
            run_concurrent_bench(
                b, writers, readers,
                || {
                    let s = Arc::new(ArcSwap::new(Arc::new(create_data())));
                    (s.clone(), s)
                },
                |s| {
                    let mut new_val = (**s.load()).clone();
                    new_val[0] = new_val[0].wrapping_add(1);
                    s.store(Arc::new(new_val));
                },
                |s| {
                    do_work(&s.load());
                }
            )
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_primitive_creation,
    bench_single_thread_ops,
    bench_blocking_write_blocking_read,
    bench_nonblocking_write_blocking_read,
    bench_nonblocking_write_nonblocking_read
);
criterion_main!(benches);
