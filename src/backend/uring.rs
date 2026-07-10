use crate::driver::Backend;

#[derive(Debug)]
pub(crate) struct Uring;

impl Uring {
    pub(crate) const KIND: Backend = Backend::Uring;
}
