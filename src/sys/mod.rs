//! Kernel interfaces for peripheral I/O.
//!
//! Each submodule maps to one kernel subsystem and opens its device node or
//! sysfs directory lazily, when the corresponding type is constructed.

// Helper macro to execute a system call that returns an `io::Result`.
//
// `macro_rules!` are resolved textually, so this must stay above the module
// declarations below: the submodules can only use it if it is defined first.
macro_rules! syscall {
    ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
        let res = unsafe { libc::$fn($($arg, )*) };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

pub mod gpio;
pub mod gpiochip;
pub mod i2c;
pub mod pwm;
pub mod spi;
