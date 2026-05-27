use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use node_bc_store::metrics::StoreMetrics;
use node_bc_store::protocol::{Message, Opcode, PAGE_SIZE};
use node_bc_store::store::{PageKey, PageStore};

// ─── Store en RAM ─────────────────────────────────────────────────────────────

fn bench_store_put(c: &mut Criterion) {
    let store = PageStore::new(Arc::new(StoreMetrics::default()));
    let data = vec![0x42u8; PAGE_SIZE];

    c.bench_function("store/put_4k", |b| {
        let mut pid = 0u64;
        b.iter(|| {
            store
                .put(PageKey::new(1, pid), black_box(data.clone()))
                .unwrap();
            pid += 1;
        });
    });
}

fn bench_store_get_hit(c: &mut Criterion) {
    let store = PageStore::new(Arc::new(StoreMetrics::default()));
    let data = vec![0x42u8; PAGE_SIZE];
    let key = PageKey::new(1, 0);
    store.put(key.clone(), data).unwrap();

    c.bench_function("store/get_hit", |b| {
        b.iter(|| black_box(store.get(&key)));
    });
}

fn bench_store_get_miss(c: &mut Criterion) {
    let store = PageStore::new(Arc::new(StoreMetrics::default()));
    let key = PageKey::new(99, 99999);

    c.bench_function("store/get_miss", |b| {
        b.iter(|| black_box(store.get(&key)));
    });
}

// ─── Protocole — sérialisation / décompression ───────────────────────────────

fn bench_protocol_serialize_put(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let data = vec![0x42u8; PAGE_SIZE];
    let msg = Message::put_page(1, 0, data);

    c.bench_function("protocol/serialize_put_4k", |b| {
        b.iter(|| {
            let mut buf = Vec::with_capacity(20 + PAGE_SIZE);
            rt.block_on(async { black_box(&msg).write_to(&mut buf).await.unwrap() });
        });
    });
}

fn bench_protocol_compression_zero_page(c: &mut Criterion) {
    let data = vec![0u8; PAGE_SIZE]; // page zéro — très compressible
    let msg = Message::put_page(1, 0, data);

    c.bench_function("protocol/lz4_compress_zero_page", |b| {
        b.iter(|| {
            black_box(black_box(&msg).try_compress());
        });
    });
}

fn bench_protocol_compression_by_ratio(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/lz4_compress");

    let scenarios: &[(&str, u8)] = &[("zero_fill", 0x00), ("text_like", 0x41), ("mixed", 0x7F)];

    for (name, fill) in scenarios {
        let data = vec![*fill; PAGE_SIZE];
        let msg = Message::put_page(1, 0, data);
        group.throughput(Throughput::Bytes(PAGE_SIZE as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), name, |b, _| {
            b.iter(|| {
                black_box(black_box(&msg).try_compress());
            });
        });
    }
    group.finish();
}

// ─── Batch PUT — assemblage de trames ────────────────────────────────────────

fn bench_batch_put_serialize(c: &mut Criterion) {
    use node_bc_store::protocol::BatchPutRequest;
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("protocol/batch_put_serialize");
    for n in [4usize, 8, 16, 32] {
        let mut req = BatchPutRequest::new(1);
        for i in 0..n as u64 {
            req.push(i, vec![0x42u8; PAGE_SIZE]);
        }
        group.throughput(Throughput::Bytes((n * PAGE_SIZE) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut buf = Vec::new();
                rt.block_on(async { black_box(&req).write_to(&mut buf).await.unwrap() });
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_store_put,
    bench_store_get_hit,
    bench_store_get_miss,
    bench_protocol_serialize_put,
    bench_protocol_compression_zero_page,
    bench_protocol_compression_by_ratio,
    bench_batch_put_serialize,
);
criterion_main!(benches);
