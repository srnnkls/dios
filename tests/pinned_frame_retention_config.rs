use dios::{Pool, PoolBuildError, PoolBuilder, PoolConfigError};

const GRANULE: u32 = 4096;

fn builder(frame_count: u32) -> PoolBuilder {
    Pool::builder()
        .frame_count(frame_count)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
}

#[test]
fn rejects_unrepresentable_retained_frame_budget() {
    let result = builder(4).max_retained_frames(u32::MAX).build();
    let Err(error) = result else {
        panic!("an unrepresentable retained-frame budget must be rejected");
    };

    let PoolBuildError::Configuration(error) = error else {
        panic!("representability rejection must be a configuration error");
    };
    assert_eq!(
        format!("{error:?}"),
        format!(
            "RetentionUnrepresentable {{ requested: {}, limit: {} }}",
            u32::MAX,
            1_u32 << 31
        )
    );
}

#[test]
fn max_reader_retry_bound_has_constraint_neutral_display() {
    let result = builder(4).max_concurrent_readers(u32::MAX).build();
    let Err(PoolBuildError::Configuration(error)) = result else {
        panic!("the retry bound must reject an unrepresentable reader count");
    };
    let PoolConfigError::RetentionUnrepresentable { requested, limit } = &error else {
        panic!("the reader-count rejection must report retention representability");
    };
    assert_eq!((*requested, *limit), (u32::MAX, u32::MAX - 1));
    assert_eq!(
        error.to_string(),
        format!(
            "retention-capacity request {} exceeds the representable limit {}",
            u32::MAX,
            u32::MAX - 1
        )
    );
}

#[test]
fn retained_frame_budget_augments_the_frame_watermark() {
    let result = builder(5).max_retained_frames(2).build();
    let Err(error) = result else {
        panic!("five frames are below the augmented watermark of six");
    };

    assert!(matches!(
        error,
        PoolBuildError::Configuration(PoolConfigError::BelowWatermark {
            frame_count: 5,
            watermark: 6,
        })
    ));
}

#[test]
fn default_zero_retained_frame_budget_preserves_the_existing_watermark() {
    builder(4)
        .build()
        .expect("four frames satisfy the zero-budget watermark");
}
