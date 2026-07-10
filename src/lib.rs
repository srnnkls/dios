//! Completion-based async direct-IO driver and userspace frame pool.

mod alignment;
mod backend;
mod driver;
mod error;

#[cfg(feature = "bench")]
pub mod bench;

pub use alignment::{Alignment, Unaligned};
pub use driver::{Backend, Driver};
pub use error::{IoError, SubmitError};
