use dios::IoError;
use dios::driver::{Alignment, Backend, Driver, SubmitError, Unaligned};

#[cfg(target_os = "macos")]
#[test]
fn macos_selects_the_eager_backend() {
    assert_eq!(Driver::BACKEND, Backend::Eager);
    assert_ne!(Driver::BACKEND, Backend::Uring);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_selects_the_uring_backend() {
    assert_eq!(Driver::BACKEND, Backend::Uring);
    assert_ne!(Driver::BACKEND, Backend::Eager);
}

#[test]
fn io_error_carries_the_operating_failure() {
    let enospc = std::io::Error::from_raw_os_error(28);
    let err = IoError::from(enospc);
    assert_eq!(err.raw_os_error(), Some(28));
}

#[test]
fn submit_error_full_is_a_backpressure_variant() {
    let full = SubmitError::Full;
    assert!(matches!(full, SubmitError::Full));
}

#[test]
fn alignment_parses_powers_of_two() {
    for accepted in [1u32, 512, 1024, 4096] {
        let Some(align) = Alignment::new(accepted) else {
            panic!("{accepted} is a power of two and must parse");
        };
        assert_eq!(align.get(), accepted);
    }
}

#[test]
fn alignment_rejects_zero_and_non_powers_of_two() {
    for rejected in [0u32, 3, 4095, 6000] {
        assert!(
            Alignment::new(rejected).is_none(),
            "{rejected} is not a valid sector/granule alignment"
        );
    }
}

#[test]
fn alignment_check_accepts_multiples_of_the_alignment() {
    let Some(align) = Alignment::new(4096u32) else {
        panic!("valid alignment");
    };
    for aligned in [0u64, 4096, 8192, 4096 * 1000] {
        assert!(align.check(aligned).is_ok(), "{aligned} is 4096-aligned");
    }

    let Some(align_one) = Alignment::new(1u32) else {
        panic!("valid alignment");
    };
    for offset in [0u64, 1, 2, 3, 123_456_789] {
        assert!(align_one.check(offset).is_ok(), "{offset} is 1-aligned");
    }
}

#[test]
fn alignment_check_rejects_unaligned_offsets_with_a_typed_error() {
    let Some(align) = Alignment::new(4096u32) else {
        panic!("valid alignment");
    };
    let Err(err): Result<(), Unaligned> = align.check(4095u64) else {
        panic!("4095 is not 4096-aligned and must be rejected");
    };
    assert_eq!(err.offset_bytes(), 4095u64);
    assert_eq!(err.alignment().get(), 4096u32);

    for unaligned in [1u64, 511, 4097, 8191] {
        assert!(
            align.check(unaligned).is_err(),
            "{unaligned} is not 4096-aligned"
        );
    }
}
