#[cfg(windows)]
mod specifics {
    extern crate winapi;
    pub use winapi::shared::ws2def::{AF_INET, AF_INET6};
}

#[cfg(not(windows))]
mod specifics {
    pub use libc::{AF_INET, AF_INET6};
}

pub use specifics::*;