use arc_swap::ArcSwap;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use retro_cell::{ReadResult, RetroCell, WriteOutcome};
use std::hint::black_box;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

const DATA_SIZE: usize = 64;

fn create_data() -> Vec<u32> {
    vec![0_u32; DATA_SIZE]
}

fn write_data(outcome: WriteOutcome<'_, Vec<u32>>) {
    match outcome {
        WriteOutcome::InPlace(mut g) => {
            g[0] = g[0].wrapping_add(1);
        }
        WriteOutcome::Congested(w) => {
            w.perform_cow(|v| {
                v[0] = v[0].wrapping_add(1);
            });
        }
    }
}

fn bench_new(c: &mut Criterion) {
    let mut group = c.benchmark_group("New");

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

fn bench_single_thread_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("SingleThreadRead");

    group.bench_function("RetroCell", |b| {
        let (_writer, reader) = RetroCell::new(create_data());
        b.iter(|| match reader.try_read() {
            ReadResult::Success(_ref) => {
                black_box(_ref);
            }
            ReadResult::Blocked(blocked) => {
                black_box(blocked.wait());
            }
        })
    });

    group.bench_function("ArcSwap", |b| {
        let s = ArcSwap::new(Arc::new(create_data()));
        b.iter(|| {
            let guard = s.load();
            black_box(&guard);
        })
    });

    group.bench_function("RwLock", |b| {
        let l = RwLock::new(create_data());
        b.iter(|| {
            let guard = l.read().unwrap();
            black_box(&*guard);
        })
    });

    group.finish();
}

fn bench_single_thread_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("SingleThreadWrite");

    group.bench_function("RetroCell", |b| {
        let (mut writer, _reader) = RetroCell::new(create_data());
        b.iter(|| write_data(writer.try_write()));
    });

    group.bench_function("ArcSwap", |b| {
        let s = ArcSwap::new(Arc::new(create_data()));
        b.iter(|| {
            let mut new_val = (**s.load()).clone();
            new_val[0] = new_val[0].wrapping_add(1);
            s.store(Arc::new(new_val));
        })
    });

    group.bench_function("RwLock", |b| {
        let l = RwLock::new(create_data());
        b.iter(|| {
            let mut guard = l.write().unwrap();
            guard[0] = guard[0].wrapping_add(1);
        })
    });

    group.finish();
}

fn bench_multi_thread_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("MultiThreadRead");

    for threads in [2, 4, 8] {
        group.bench_with_input(BenchmarkId::new("RetroCell", threads), &threads, |b, &t| {
            let (_writer, reader) = RetroCell::new(create_data());
            b.iter_custom(|iters| {
                let start = std::time::Instant::now();
                thread::scope(|s| {
                    for _ in 0..t {
                        let r = reader.clone();
                        s.spawn(move || {
                            for _ in 0..iters {
                                let _ = black_box(r.read());
                            }
                        });
                    }
                });
                start.elapsed()
            })
        });

        group.bench_with_input(BenchmarkId::new("ArcSwap", threads), &threads, |b, &t| {
            let s = Arc::new(ArcSwap::new(Arc::new(create_data())));
            b.iter_custom(|iters| {
                let start = std::time::Instant::now();
                thread::scope(|_s| {
                    for _ in 0..t {
                        let s_clone = s.clone();
                        _s.spawn(move || {
                            for _ in 0..iters {
                                black_box(s_clone.load());
                            }
                        });
                    }
                });
                start.elapsed()
            })
        });

        group.bench_with_input(BenchmarkId::new("RwLock", threads), &threads, |b, &t| {
            let l = Arc::new(RwLock::new(create_data()));
            b.iter_custom(|iters| {
                let start = std::time::Instant::now();
                thread::scope(|s| {
                    for _ in 0..t {
                        let l_clone = l.clone();
                        s.spawn(move || {
                            for _ in 0..iters {
                                let v = l_clone.read().unwrap();
                                black_box(&v);
                            }
                        });
                    }
                });
                start.elapsed()
            })
        });
    }
    group.finish();
}

fn bench_mixed_ratio(c: &mut Criterion) {
    let mut group = c.benchmark_group("MixedReadWrite");
    // Ratios: 20:1, 10:1, 4:1
    // 1 writer thread, 4 reader threads.
    // We maintain ratio by adjusting the number of writes relative to reads.
    let ratios = [20, 10, 4];
    let num_readers = 4;

    for ratio in ratios {
        group.bench_with_input(
            BenchmarkId::new("RetroCell", format!("{}:1", ratio)),
            &ratio,
            |b, &r| {
                let (mut writer, reader) = RetroCell::new(create_data());
                b.iter_custom(|iters| {
                    let write_iters = (iters * num_readers as u64) / r as u64;
                    let start = std::time::Instant::now();
                    thread::scope(|s| {
                        let w = &mut writer;
                        if write_iters > 0 {
                            s.spawn(move || {
                                for _ in 0..write_iters {
                                    write_data(w.try_write());
                                }
                            });
                        }

                        for _ in 0..num_readers {
                            let r = reader.clone();
                            s.spawn(move || {
                                for _ in 0..iters {
                                    match r.try_read() {
                                        ReadResult::Success(_ref) => {
                                            black_box(_ref);
                                        }
                                        ReadResult::Blocked(blocked) => {
                                            black_box(blocked.wait());
                                        }
                                    }
                                }
                            });
                        }
                    });
                    start.elapsed()
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("ArcSwap", format!("{}:1", ratio)),
            &ratio,
            |b, &r| {
                let s = Arc::new(ArcSwap::new(Arc::new(create_data())));
                b.iter_custom(|iters| {
                    let write_iters = (iters * num_readers as u64) / r as u64;
                    let start = std::time::Instant::now();
                    thread::scope(|_s| {
                        if write_iters > 0 {
                            let s_clone = s.clone();
                            _s.spawn(move || {
                                for _ in 0..write_iters {
                                    let mut new_val = (**s_clone.load()).clone();
                                    new_val[0] = new_val[0].wrapping_add(1);
                                    s_clone.store(Arc::new(new_val));
                                }
                            });
                        }

                        for _ in 0..num_readers {
                            let s_clone = s.clone();
                            _s.spawn(move || {
                                for _ in 0..iters {
                                    black_box(s_clone.load());
                                }
                            });
                        }
                    });
                    start.elapsed()
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("RwLock", format!("{}:1", ratio)),
            &ratio,
            |b, &r| {
                let l = Arc::new(RwLock::new(create_data()));
                b.iter_custom(|iters| {
                    let write_iters = (iters * num_readers as u64) / r as u64;
                    let start = std::time::Instant::now();
                    thread::scope(|_s| {
                        if write_iters > 0 {
                            let l_clone = l.clone();
                            _s.spawn(move || {
                                for _ in 0..write_iters {
                                    let mut guard = l_clone.write().unwrap();
                                    guard[0] = guard[0].wrapping_add(1);
                                }
                            });
                        }

                        for _ in 0..num_readers {
                            let l_clone = l.clone();
                            _s.spawn(move || {
                                for _ in 0..iters {
                                    let v = l_clone.read().unwrap();
                                    black_box(&*v);
                                }
                            });
                        }
                    });
                    start.elapsed()
                })
            },
        );
    }
    group.finish();
}

fn bench_multi_writer_multi_reader(c: &mut Criterion) {
    let mut group = c.benchmark_group("MultiWriterMultiReader");
    // (Ratio Name, Readers, Writers)
    let scenarios = [("4:1", 16, 4), ("1:1", 4, 4), ("1:4", 4, 16)];

    for (name, num_readers, num_writers) in scenarios {
        group.bench_with_input(BenchmarkId::new("RetroCell", name), &name, |b, &_| {
            let (writer, reader) = RetroCell::new(create_data());
            let writer = Arc::new(Mutex::new(writer));

            b.iter_custom(|iters| {
                let start = std::time::Instant::now();
                thread::scope(|s| {
                    // Writers
                    for _ in 0..num_writers {
                        let w = writer.clone();
                        s.spawn(move || {
                            for _ in 0..iters {
                                write_data(w.lock().unwrap().try_write());
                            }
                        });
                    }
                    // Readers
                    for _ in 0..num_readers {
                        let r = reader.clone();
                        s.spawn(move || {
                            for _ in 0..iters {
                                match r.try_read() {
                                    ReadResult::Success(_ref) => {
                                        black_box(_ref);
                                    }
                                    ReadResult::Blocked(blocked) => {
                                        black_box(blocked.wait());
                                    }
                                }
                            }
                        });
                    }
                });
                start.elapsed()
            })
        });

        group.bench_with_input(BenchmarkId::new("ArcSwap", name), &name, |b, &_| {
            let s = Arc::new(ArcSwap::new(Arc::new(create_data())));

            b.iter_custom(|iters| {
                let start = std::time::Instant::now();
                thread::scope(|s_scope| {
                    // Writers
                    for _ in 0..num_writers {
                        let s_clone = s.clone();
                        s_scope.spawn(move || {
                            for _ in 0..iters {
                                let mut new_val = (**s_clone.load()).clone();
                                new_val[0] = new_val[0].wrapping_add(1);
                                s_clone.store(Arc::new(new_val));
                            }
                        });
                    }
                    // Readers
                    for _ in 0..num_readers {
                        let s_clone = s.clone();
                        s_scope.spawn(move || {
                            for _ in 0..iters {
                                black_box(s_clone.load());
                            }
                        });
                    }
                });
                start.elapsed()
            })
        });

        group.bench_with_input(BenchmarkId::new("RwLock", name), &name, |b, &_| {
            let l = Arc::new(RwLock::new(create_data()));

            b.iter_custom(|iters| {
                let start = std::time::Instant::now();
                thread::scope(|s| {
                    // Writers
                    for _ in 0..num_writers {
                        let l_clone = l.clone();
                        s.spawn(move || {
                            for _ in 0..iters {
                                let mut guard = l_clone.write().unwrap();
                                guard[0] = guard[0].wrapping_add(1);
                            }
                        });
                    }
                    // Readers
                    for _ in 0..num_readers {
                        let l_clone = l.clone();
                        s.spawn(move || {
                            for _ in 0..iters {
                                let v = l_clone.read().unwrap();
                                black_box(&*v);
                            }
                        });
                    }
                });
                start.elapsed()
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_new,
    bench_single_thread_read,
    bench_single_thread_write,
    bench_multi_thread_read,
    bench_mixed_ratio,
    bench_multi_writer_multi_reader
);
criterion_main!(benches);
