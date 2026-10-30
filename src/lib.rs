//! A Rust library for peripheral I/O (GPIO, PWM, SPI, I2C) on Linux.
//!
//! The crate talks to the kernel interfaces directly, so no C library or
//! cross-compilation setup is needed:
//!
//! | Bus  | Kernel interface                     | Requires                  |
//! |------|--------------------------------------|---------------------------|
//! | GPIO | `/sys/class/gpio` (sysfs, deprecated) | `CONFIG_GPIO_SYSFS`      |
//! | PWM  | `/sys/class/pwm` (sysfs)              | `CONFIG_PWM_SYSFS`       |
//! | SPI  | `/dev/spidev<bus>.<cs>` (`spidev`)    | `CONFIG_SPI_SPIDEV`      |
//! | I2C  | `/dev/i2c-<bus>` (`i2c-dev`)          | `CONFIG_I2C_CHARDEV`     |
//!
//! Access normally requires `root` or membership in the corresponding `gpio`,
//! `pwm`, `spi` or `i2c` group.
//!
//! # Examples
//!
//! Driving a GPIO line:
//!
//! ```no_run
//! use periphery::sys::gpio::{Direction, Pin, Value};
//!
//! # fn main() -> std::io::Result<()> {
//! let pin = Pin::new(17);
//! pin.export()?;
//! pin.set_direction(Direction::Out)?;
//! pin.set_value(Value::High)?;
//! # Ok(())
//! # }
//! ```
//!
//! Reading an I2C register:
//!
//! ```no_run
//! use periphery::sys::i2c::I2C;
//!
//! # fn main() -> std::io::Result<()> {
//! let mut i2c = I2C::new(1)?;
//! i2c.set_slave_address(0x68)?;
//!
//! let mut value = [0u8; 1];
//! i2c.write_read(&[0x75], &mut value)?; // WHO_AM_I
//! println!("id = {:#04x}", value[0]);
//! # Ok(())
//! # }
//! ```
//!
//! A full duplex SPI transfer:
//!
//! ```no_run
//! use periphery::sys::spi::{Mode, SpidevTransfer, SPI};
//!
//! # fn main() -> std::io::Result<()> {
//! let spi = SPI::new(0, 0, 1_000_000, Mode::Mode0)?;
//!
//! let mut read = [0u8; 2];
//! let mut transfer = SpidevTransfer::read_write(&[0x9F, 0x00], &mut read);
//! spi.transfer(&mut transfer)?;
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

#[cfg(not(target_os = "linux"))]
compile_error!(
    "periphery only supports Linux: it talks to /sys and /dev directly and has no \
     implementation for other platforms"
);

pub mod sys;
