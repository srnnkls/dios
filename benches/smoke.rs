use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

const GRANULE_BYTES: usize = 4096;

fn granule_copy(c: &mut Criterion) {
    let src = vec![0xA5_u8; GRANULE_BYTES];
    let mut dst = vec![0_u8; GRANULE_BYTES];
    c.bench_function("granule_copy_4k", |b| {
        b.iter(|| {
            dst.copy_from_slice(black_box(&src));
            black_box(dst.as_slice());
        });
    });
}

criterion_group!(benches, granule_copy);
criterion_main!(benches);
