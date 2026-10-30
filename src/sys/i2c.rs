//! Linux I2C access through the `i2c-dev` character device.
//!
//! [`I2C`] opens `/dev/i2c-<bus>`; access normally requires `root` or
//! membership in the `i2c` group. Before any transfer the slave address has to
//! be selected with [`I2C::set_slave_address`].

#![allow(dead_code)]

use std::cell::Cell;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind::InvalidData;
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::os::raw::c_ulong;
use std::os::unix::io::{AsRawFd, RawFd};

/// An open I2C bus.
///
/// The underlying file descriptor is closed on drop. The handle is `Send`, so
/// it can be moved to another thread, but not `Sync`: it cannot be shared
/// between threads.
pub struct I2C {
    bus: u8,
    file: File,
    addr_10bit: bool,
    address: u16,
    funcs: Capabilities,
    _not_sync: PhantomData<Cell<()>>,
}

/// Mirrors the `request` parameter type of `libc::ioctl`, which differs between
/// C libraries (`c_ulong` on gnu/uclibc, `c_int` on musl/android).
type IoctlNumType = libc::Ioctl;

// Capabilities returned by REQ_FUNCS
const FUNC_I2C: c_ulong = 0x01;
const FUNC_10BIT_ADDR: c_ulong = 0x02;
const FUNC_PROTOCOL_MANGLING: c_ulong = 0x04;
const FUNC_SMBUS_PEC: c_ulong = 0x08;
const FUNC_NOSTART: c_ulong = 0x10;
const FUNC_SLAVE: c_ulong = 0x20;

/// The functionality mask reported by the adapter (`I2C_FUNCS`).
#[derive(PartialEq, Copy, Clone)]
pub struct Capabilities {
    funcs: c_ulong,
}

impl Capabilities {
    fn new(funcs: c_ulong) -> Capabilities {
        Capabilities { funcs }
    }

    pub(crate) fn i2c(self) -> bool {
        (self.funcs & FUNC_I2C) > 0
    }

    pub(crate) fn slave(self) -> bool {
        (self.funcs & FUNC_SLAVE) > 0
    }

    /// Indicates whether 10-bit addresses are supported.
    pub fn addr_10bit(self) -> bool {
        (self.funcs & FUNC_10BIT_ADDR) > 0
    }

    /// Indicates whether protocol mangling is supported.
    pub(crate) fn protocol_mangling(self) -> bool {
        (self.funcs & FUNC_PROTOCOL_MANGLING) > 0
    }

    /// Indicates whether the NOSTART flag is supported.
    pub(crate) fn nostart(self) -> bool {
        (self.funcs & FUNC_NOSTART) > 0
    }

    /// Indicates whether SMBus Packet Error Checking is supported.
    /// Indicates whether SMBus Packet Error Checking is supported.
    pub fn smbus_pec(self) -> bool {
        (self.funcs & FUNC_SMBUS_PEC) > 0
    }
}

impl fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Capabilities")
            .field("addr_10bit", &self.addr_10bit())
            .finish()
    }
}

// Specifies RDWR segment parameters, mirroring `struct i2c_msg`
#[repr(C)]
#[derive(Debug, PartialEq, Copy, Clone)]
struct RdwrSegment {
    // Slave address
    addr: u16,
    // Segment flags
    flags: u16,
    // Buffer length
    len: u16,
    // Pointer to buffer
    data: usize,
}

// Specifies RDWR request parameters, mirroring `struct i2c_rdwr_ioctl_data`
#[repr(C)]
#[derive(Debug, PartialEq, Copy, Clone)]
struct RdwrRequest {
    // Pointer to an array of segments
    segments: *const RdwrSegment,
    // Number of segments
    nmsgs: u32,
}

impl I2C {
    /// Opens `/dev/i2c-<bus>` and queries the adapter capabilities.
    ///
    /// 10-bit addressing is switched off if the adapter supports it.
    pub fn new(bus: u8) -> io::Result<I2C> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/i2c-{}", bus))?;

        let mut funcs: c_ulong = 0;
        syscall!(ioctl(
            file.as_raw_fd(),
            I2C_FUNCS as IoctlNumType,
            &mut funcs
        ))?;
        let capabilities = Capabilities::new(funcs);

        let mut i2c = I2C {
            bus,
            file,
            addr_10bit: false,
            address: 0,
            funcs: capabilities,
            _not_sync: PhantomData,
        };

        if i2c.funcs.addr_10bit() {
            i2c.set_addr_10bit(false)?;
        }

        Ok(i2c)
    }

    /// Returns the bus number this handle was opened for.
    pub fn bus(&self) -> u8 {
        self.bus
    }

    /// Returns the adapter capabilities.
    pub fn capabilities(&self) -> Capabilities {
        self.funcs
    }

    /// Reads the bus clock frequency in Hz from the device tree node.
    ///
    /// Fails on platforms that do not expose
    /// `/sys/class/i2c-adapter/i2c-<bus>/of_node/clock-frequency`.
    pub fn clock_speed(&self) -> io::Result<u32> {
        let mut buffer = [0u8; 4];

        File::open(format!(
            "/sys/class/i2c-adapter/i2c-{}/of_node/clock-frequency",
            self.bus
        ))?
        .read_exact(&mut buffer)?;

        Ok(u32::from(buffer[3])
            | (u32::from(buffer[2]) << 8)
            | (u32::from(buffer[1]) << 16)
            | (u32::from(buffer[0]) << 24))
    }

    /// Selects the slave address used by all subsequent transfers.
    ///
    /// Returns `InvalidData` for reserved or out-of-range addresses. Note that
    /// this fails with `EBUSY` when a kernel driver already owns the address;
    /// use [`I2C::set_slave_address_force`] to take the address over.
    pub fn set_slave_address(&mut self, slave_address: u16) -> io::Result<()> {
        self.check_slave_address(slave_address)?;

        syscall!(ioctl(
            self.file.as_raw_fd(),
            I2C_SLAVE as IoctlNumType,
            slave_address as c_ulong
        ))?;

        self.address = slave_address;

        Ok(())
    }

    /// Selects the slave address, taking it over from any kernel driver.
    ///
    /// This uses `I2C_SLAVE_FORCE`, which succeeds even when the address is
    /// already bound to a driver. Only use it when you know that driver is not
    /// needed.
    pub fn set_slave_address_force(&mut self, slave_address: u16) -> io::Result<()> {
        self.check_slave_address(slave_address)?;

        syscall!(ioctl(
            self.file.as_raw_fd(),
            I2C_SLAVE_FORCE as IoctlNumType,
            slave_address as c_ulong
        ))?;

        self.address = slave_address;

        Ok(())
    }

    /// Rejects reserved and out-of-range addresses for the current addressing
    /// mode.
    fn check_slave_address(&self, slave_address: u16) -> io::Result<()> {
        if (!self.addr_10bit && ((slave_address >> 3) == 0b1111 || slave_address > 0x7F))
            || (self.addr_10bit && slave_address > 0x03FF)
        {
            return Err(io::Error::new(
                InvalidData,
                format!("Invalid slave address: {:?}", slave_address),
            ));
        }

        Ok(())
    }

    /// Sets the transfer timeout in milliseconds.
    ///
    /// The kernel expects it in units of 10 ms, so the value is rounded down
    /// accordingly; values below 10 ms are raised to one unit.
    pub fn set_timeout(&self, timeout: u32) -> io::Result<()> {
        // Contrary to the i2cdev documentation, this seems to
        // be used as a timeout for (part of?) the I2C transaction.
        // ioctl::set_timeout(self.i2cdev.as_raw_fd(), timeout as c_ulong)?;
        let timeout: c_ulong = if timeout > 0 && timeout < 10 {
            1
        } else {
            (timeout / 10).into()
        };

        syscall!(ioctl(
            self.file.as_raw_fd(),
            I2C_TIMEOUT as IoctlNumType,
            timeout as c_ulong
        ))?;

        Ok(())
    }

    fn set_retries(&self, retries: u32) -> io::Result<()> {
        // Set to private. While i2cdev implements retries, the underlying drivers don't.
        // ioctl::set_retries(self.i2cdev.as_raw_fd(), retries as c_ulong)?;
        syscall!(ioctl(
            self.file.as_raw_fd(),
            I2C_RETRIES as IoctlNumType,
            retries as c_ulong
        ))?;

        Ok(())
    }

    /// Enables or disables 10-bit addressing.
    ///
    /// Fails with `InvalidData` when the adapter reports no 10-bit support.
    pub fn set_addr_10bit(&mut self, addr_10bit: bool) -> io::Result<()> {
        if !self.funcs.addr_10bit() {
            return Err(io::Error::new(
                InvalidData,
                "FeatureNotSupported: addr_10bit".to_string(),
            ));
        }
        syscall!(ioctl(
            self.file.as_raw_fd(),
            I2C_TENBIT as IoctlNumType,
            addr_10bit as c_ulong
        ))?;

        self.addr_10bit = addr_10bit;

        Ok(())
    }

    /// Enables or disables SMBus Packet Error Checking.
    pub fn set_smbus_pec(&self, enable: bool) -> io::Result<()> {
        syscall!(ioctl(
            self.file.as_raw_fd(),
            I2C_PEC as IoctlNumType,
            enable as c_ulong
        ))?;

        Ok(())
    }

    /// Reads into `buffer` from the currently selected slave.
    pub fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }

    /// Writes `buffer` to the currently selected slave.
    pub fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }

    /// Performs a combined write/read transfer with a single STOP condition.
    ///
    /// This is the usual way to read a register: the register address is
    /// written and the value read back in one atomic `I2C_RDWR` operation.
    /// Either buffer may be empty, in which case the transfer degenerates into
    /// a plain write or read; if both are empty the call does nothing.
    pub fn write_read(&self, write_buffer: &[u8], read_buffer: &mut [u8]) -> io::Result<()> {
        if write_buffer.is_empty() && read_buffer.is_empty() {
            return Ok(());
        }

        let segment_write = RdwrSegment {
            addr: self.address,
            flags: if self.addr_10bit { RDWR_FLAG_TEN } else { 0 },
            len: write_buffer.len() as u16,
            data: write_buffer.as_ptr() as usize,
        };

        let segment_read = RdwrSegment {
            addr: self.address,
            flags: if self.addr_10bit {
                RDWR_FLAG_RD | RDWR_FLAG_TEN
            } else {
                RDWR_FLAG_RD
            },
            len: read_buffer.len() as u16,
            data: read_buffer.as_mut_ptr() as usize,
        };

        // The kernel walks `nmsgs` entries, so an empty buffer must not end up
        // as the first segment.
        let segments: [RdwrSegment; 2] = if write_buffer.is_empty() {
            [segment_read, segment_write]
        } else {
            [segment_write, segment_read]
        };
        let nmsgs = if write_buffer.is_empty() || read_buffer.is_empty() {
            1
        } else {
            2
        };

        let mut request = RdwrRequest {
            segments: segments.as_ptr(),
            nmsgs,
        };

        syscall!(ioctl(
            self.file.as_raw_fd(),
            I2C_RDWR as IoctlNumType,
            &mut request
        ))?;

        Ok(())
    }
}

impl AsRawFd for I2C {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl fmt::Debug for I2C {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("I2C")
            .field("bus", &self.bus)
            .field("address", &self.address)
            .field("capabilities", &self.funcs)
            .finish()
    }
}

// from include/uapi/linux/i2c-dev.h
const I2C_RETRIES: u16 = 0x0701;
const I2C_TIMEOUT: u16 = 0x0702;
const I2C_SLAVE: u16 = 0x0703;
const I2C_SLAVE_FORCE: u16 = 0x0706;
const I2C_TENBIT: u16 = 0x0704;
const I2C_FUNCS: u16 = 0x0705;
const I2C_RDWR: u16 = 0x0707;
const I2C_PEC: u16 = 0x0708;
const I2C_SMBUS: u16 = 0x0720;
const I2C_RDRW_IOCTL_MAX_MSGS: u8 = 42;

// NOTE: REQ_RETRIES - Supported in i2cdev, but not used in the underlying drivers
// NOTE: REQ_RDWR - Only a single read operation is supported as the final message (see i2c-bcm2835.c)

const RDWR_FLAG_RD: u16 = 0x0001; // Read operation
const RDWR_FLAG_TEN: u16 = 0x0010; // 10-bit slave address

const RDWR_MSG_MAX: usize = 42; // Maximum messages per RDWR operation
const SMBUS_BLOCK_MAX: usize = 32; // Maximum bytes per block transfer
