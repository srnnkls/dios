//! RC2 vector ownership, ordered transfer, and reserved-slot lifetime contracts.

#![cfg(feature = "mock")]

use std::path::Path;

use dios::DirectIo;
use dios::driver::{CompletionBatch, OpKind, OpToken, SubmitError};
use dios::testing::{DriverObservation, Injected, MockDriver, MockRingDriver, ReadFrameIdx};

const FRAME_BYTES: u32 = 4096;

fn mock(capacity: u32) -> MockDriver {
    MockDriver::builder()
        .seed(0x00D5_7EED)
        .queue_capacity(capacity)
        .frames(4)
        .frame_bytes(FRAME_BYTES)
        .retry_bound(3)
        .build()
}

fn frames<const COUNT: usize>(indices: [u32; COUNT]) -> [ReadFrameIdx; COUNT] {
    indices.map(ReadFrameIdx::new)
}

fn drain(driver: &MockDriver, expected: usize) -> Vec<(OpToken, u32)> {
    let mut batch = CompletionBatch::with_capacity(expected);
    let mut completed = Vec::with_capacity(expected);
    for _ in 0..64 {
        driver.poll(&mut batch);
        for completion in &batch {
            assert_eq!(completion.kind(), OpKind::Read);
            completed.push((
                completion.token(),
                completion.result().expect("read succeeds"),
            ));
        }
        if completed.len() == expected {
            return completed;
        }
        assert!(completed.len() < expected, "one completion per submission");
    }
    panic!("bounded mock polls did not drain the accepted reads");
}

fn copied(driver: &MockDriver, frame: u32) -> [u8; 4096] {
    let mut bytes = [0; 4096];
    assert_eq!(
        driver.copy_frame(ReadFrameIdx::new(frame), &mut bytes),
        4096
    );
    bytes
}

#[test]
fn vector_destinations_preserve_slice_order_and_exact_short_prefix_after_slot_reuse() {
    let driver = mock(2);
    let file = driver
        .open(Path::new("vector-order"), DirectIo::Disabled)
        .expect("mock file opens");
    driver.seed_page(&file, 1, 0xB2);
    driver.seed_page(&file, 2, 0xC3);
    driver.seed_page(&file, 3, 0xD4);

    let mut previous = None;
    for _ in 0..3 {
        let first = driver
            .submit_read_vector(&file, &frames([2, 0]), 4096)
            .expect("one vector occupies one operation slot");
        let second = driver
            .submit_read_vector(&file, &frames([3, 1]), 8192)
            .expect("another slot cannot invalidate the first vector's destinations");
        let completed = drain(&driver, 2);
        assert!(completed.contains(&(first, 8192)));
        assert!(completed.contains(&(second, 8192)));
        assert_ne!(
            Some(first),
            previous,
            "reused slots mint a fresh generation"
        );
        previous = Some(first);
        assert_eq!(copied(&driver, 2), [0xB2; 4096]);
        assert_eq!(copied(&driver, 0), [0xC3; 4096]);
        assert_eq!(copied(&driver, 3), [0xC3; 4096]);
        assert_eq!(copied(&driver, 1), [0xD4; 4096]);
    }

    driver.seed_page(&file, 1, 0x11);
    driver.seed_page(&file, 2, 0x22);
    driver.seed_page(&file, 3, 0x33);
    driver.inject_next(Injected::Short(4609));
    let partial = driver
        .submit_read_vector(&file, &frames([2, 0, 3]), 4096)
        .expect("three destinations still occupy one operation slot");
    assert_eq!(drain(&driver, 1), vec![(partial, 4609)]);

    assert_eq!(copied(&driver, 2), [0x11; 4096]);
    let middle = copied(&driver, 0);
    assert_eq!(&middle[..513], &[0x22; 513]);
    assert_eq!(&middle[513..], &[0xC3; 3583]);
    assert_eq!(copied(&driver, 3), [0xC3; 4096]);
    assert_eq!(copied(&driver, 1), [0xD4; 4096]);
}

#[test]
fn refusal_returns_every_frame_and_partial_completion_holds_slot_and_file_until_drop() {
    let driver = mock(1);
    let file = driver
        .open(Path::new("vector-lease"), DirectIo::Disabled)
        .expect("mock file opens");
    let file_id = file.file_id();
    let blocker = driver
        .submit_read(&file, ReadFrameIdx::new(3), 0)
        .expect("the only operation slot is occupied");
    assert!(matches!(
        driver.submit_read_vector(&file, &frames([2, 0]), 0),
        Err(SubmitError::Full)
    ));
    assert_eq!(drain(&driver, 1), vec![(blocker, 4096)]);

    driver.inject_next(Injected::Short(4609));
    let vector = driver
        .submit_read_vector(&file, &frames([2, 0]), 0)
        .expect("refusal returned both destination claims");
    let mut batch = CompletionBatch::with_capacity(1);
    assert_eq!(driver.poll(&mut batch), 1);
    let completion = batch.iter().next().expect("one partial completion");
    assert_eq!(completion.token(), vector);
    assert_eq!(completion.result().expect("positive prefix"), 4609);
    assert!(matches!(
        driver.submit_read(&file, ReadFrameIdx::new(1), 0),
        Err(SubmitError::Full)
    ));
    driver.close(file);
    assert!(
        !driver.is_closed(file_id),
        "a held continuation retains the original logical file operation"
    );
    drop(batch);
    assert!(
        driver.is_closed(file_id),
        "dropping the lease aborts its tail"
    );

    let replacement = driver
        .open(Path::new("vector-reuse"), DirectIo::Disabled)
        .expect("retired file storage is reusable");
    let reused = driver
        .submit_read_vector(&replacement, &frames([0, 2]), 0)
        .expect("lease drop returns its slot and every destination");
    assert_ne!(vector, reused, "only a new logical read bumps generation");
    assert_eq!(drain(&driver, 1), vec![(reused, 8192)]);
    for frame in [0, 2] {
        let point = driver
            .submit_read(&replacement, ReadFrameIdx::new(frame), 0)
            .expect("final vector completion releases each frame for point reads");
        assert_eq!(drain(&driver, 1), vec![(point, 4096)]);
    }
}

#[test]
fn ring_teardown_finishes_vector_retries_before_retiring_its_file() {
    let driver = MockRingDriver::builder()
        .seed(0x00D5_7EED)
        .queue_capacity(2)
        .frames(4)
        .frame_bytes(FRAME_BYTES)
        .retry_bound(3)
        .build();
    let file = driver
        .open(Path::new("vector-teardown"), DirectIo::Disabled)
        .expect("mock file opens");
    let file_id = file.file_id();
    let observation = driver.observe();
    driver.inject_for_next_submit(&[Injected::Eintr, Injected::Eagain]);
    driver
        .submit_read_vector(&file, &frames([2, 0, 3]), 0)
        .expect("one vector is admitted with bounded transient retries");
    driver
        .submit_read(&file, ReadFrameIdx::new(1), 0)
        .expect("a point read uses the remaining operation slot");
    driver.close(file);
    assert!(!driver.is_closed(file_id), "accepted reads retain the file");

    drop(driver);

    assert_eq!(observation.ops_in_flight(), 0);
    assert_eq!(
        observation.reaped(),
        2,
        "one terminal result per logical read"
    );
    assert_eq!(
        observation.retired(),
        1,
        "retire follows the final retry CQE"
    );
}
