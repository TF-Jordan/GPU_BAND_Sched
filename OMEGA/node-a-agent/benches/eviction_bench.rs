use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use node_a_agent::clock_eviction::ClockEvictor;

// ─── CLOCK — hot path : mark_present O(1) ─────────────────────────────────────

fn bench_mark_present(c: &mut Criterion) {
    c.bench_function("clock/mark_present", |b| {
        let ev = ClockEvictor::new(16384, 0);
        let mut pid = 0u64;
        b.iter(|| {
            ev.mark_present(black_box(pid % 16384));
            pid += 1;
        });
    });
}

fn bench_mark_accessed(c: &mut Criterion) {
    let ev = ClockEvictor::new(1024, 0);
    for i in 0..1024u64 {
        ev.mark_present(i);
    }

    c.bench_function("clock/mark_accessed", |b| {
        let mut pid = 0u64;
        b.iter(|| {
            ev.mark_accessed(black_box(pid % 1024));
            pid += 1;
        });
    });
}

// ─── CLOCK — sélection de victimes (vary ring size) ──────────────────────────

fn bench_select_victims(c: &mut Criterion) {
    let mut group = c.benchmark_group("clock/select_victims");

    for ring_size in [64usize, 256, 1024, 4096] {
        let ev = ClockEvictor::new(ring_size, 0);
        for i in 0..ring_size as u64 {
            ev.mark_present(i);
            // Mettre tous les bits à 0 pour qu'ils soient éligibles à l'éviction
            if let Some(mut m) = ev.meta.get_mut(&i) {
                m.access_bit = false;
            }
        }
        group.bench_with_input(
            BenchmarkId::from_parameter(ring_size),
            &ring_size,
            |b, &sz| {
                b.iter(|| {
                    let victims = ev.select_victims(black_box(sz / 4));
                    // Re-injecter pour garder l'anneau stable
                    for pid in victims {
                        ev.mark_present(pid);
                        if let Some(mut m) = ev.meta.get_mut(&pid) {
                            m.access_bit = false;
                        }
                    }
                });
            },
        );
    }
    group.finish();
}

// ─── CLOCK — contention multi-thread ─────────────────────────────────────────

fn bench_concurrent_mark_present(c: &mut Criterion) {
    use std::sync::Arc;
    let ev = Arc::new(ClockEvictor::new(4096, 0));

    c.bench_function("clock/mark_present_concurrent_4threads", |b| {
        b.iter(|| {
            let handles: Vec<_> = (0..4)
                .map(|t| {
                    let ev = ev.clone();
                    std::thread::spawn(move || {
                        for i in 0..256u64 {
                            ev.mark_present(black_box(t * 256 + i));
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
        });
    });
}

criterion_group!(
    benches,
    bench_mark_present,
    bench_mark_accessed,
    bench_select_victims,
    bench_concurrent_mark_present,
);
criterion_main!(benches);
