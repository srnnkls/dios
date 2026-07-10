//! Operating-error values crossing the driver boundary.

/// A syscall-level IO failure surfaced from the driver.
#[derive(Debug)]
pub struct IoError(std::io::Error);

impl IoError {
    /// Returns the raw OS error code, when the failure carries one.
    #[must_use]
    pub fn raw_os_error(&self) -> Option<i32> {
        self.0.raw_os_error()
    }

    pub(crate) fn from_raw(errno: i32) -> Self {
        Self(std::io::Error::from_raw_os_error(errno))
    }
}

impl From<std::io::Error> for IoError {
    fn from(source: std::io::Error) -> Self {
        Self(source)
    }
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for IoError {}

/// Why a submission was refused: backpressure, never a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubmitError {
    /// The submission queue is full after a flush-retry.
    Full,
    /// The handle's fd generation is stale — closed or reused (INV-11).
    StaleHandle,
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("submission queue full"),
            Self::StaleHandle => f.write_str("stale file handle generation"),
        }
    }
}

impl std::error::Error for SubmitError {}
