use crate::driver::Backend;

#[derive(Debug)]
pub(crate) struct Eager;

impl Eager {
    pub(crate) const KIND: Backend = Backend::Eager;
}
