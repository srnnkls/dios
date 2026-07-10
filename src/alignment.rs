/// A byte alignment: a non-zero power of two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Alignment(u32);

impl Alignment {
    /// Parses `bytes`, returning `None` unless it is a non-zero power of two.
    #[must_use]
    pub const fn new(bytes: u32) -> Option<Self> {
        if bytes == 0 {
            return None;
        }
        if !bytes.is_power_of_two() {
            return None;
        }
        Some(Self(bytes))
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Checks that `offset` is a whole multiple of this alignment.
    ///
    /// # Errors
    ///
    /// Returns [`Unaligned`] carrying `offset` and this alignment otherwise.
    pub fn check(self, offset_bytes: u64) -> Result<(), Unaligned> {
        debug_assert!(self.0.is_power_of_two());
        if offset_bytes & (u64::from(self.0) - 1) == 0 {
            Ok(())
        } else {
            Err(Unaligned {
                offset_bytes,
                alignment: self,
            })
        }
    }
}

/// An offset that failed an [`Alignment::check`], carrying both operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Unaligned {
    offset_bytes: u64,
    alignment: Alignment,
}

impl Unaligned {
    #[must_use]
    pub fn offset_bytes(self) -> u64 {
        self.offset_bytes
    }

    #[must_use]
    pub fn alignment(self) -> Alignment {
        self.alignment
    }
}

impl std::fmt::Display for Unaligned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "offset {} is not aligned to {} bytes",
            self.offset_bytes,
            self.alignment.get()
        )
    }
}

impl std::error::Error for Unaligned {}
