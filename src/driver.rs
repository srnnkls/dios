use crate::backend;

/// A diagnostic probe of the compiled backend. Op routing binds to the
/// cfg-selected concrete type (AD-1), never by matching this at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// Portable backend: `submit` enqueues, `poll` runs the syscall inline.
    Eager,
    /// Linux `io_uring` backend.
    Uring,
}

#[derive(Debug)]
pub struct Driver(backend::Impl);

impl Driver {
    /// The backend selected for the target platform.
    pub const BACKEND: Backend = backend::Impl::KIND;
}
