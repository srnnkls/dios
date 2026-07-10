//! The backend seam: op routing binds to `Impl` at compile time (AD-1).

#[cfg(not(target_os = "linux"))]
pub(crate) mod eager;
#[cfg(target_os = "linux")]
pub(crate) mod uring;

pub(crate) type Impl = std::cfg_select! {
    target_os = "linux" => uring::Uring,
    _ => eager::Eager,
};
