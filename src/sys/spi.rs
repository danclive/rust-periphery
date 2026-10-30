//! Linux SPI access through the `spidev` character device.
//!
//! [`SPI`] opens `/dev/spidev<bus>.<slave>`; access normally requires `root` or
//! membership in the `spi` group.
//!
//! Clock mode, bit order, word size and clock frequency can be queried and
//! changed at runtime. Full and half duplex transfers are available through
//! [`SPI::transfer`] and [`SPI::transfer_multiple`].

#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_macros)]

use std::cell::Cell;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::os::unix::io::{AsRawFd, RawFd};

// 125.0 MHz   125000000
// 62.5 MHz    62500000
// 31.2 MHz    31200000
// 15.6 MHz    15600000
// 7.8 MHz     7800000
// 3.9 MHz     3900000
// 1953 kHz    1953000
// 976 kHz     976000
// 488 kHz     488000
// 244 kHz     244000
// 122 kHz     122000
// 61 kHz  61000
// 30.5 kHz    30500
// 15.2 kHz    15200
// 7629 Hz     7629

/// An open SPI slave device.
///
/// The underlying file descriptor is closed on drop. The handle is `Send`, so
/// it can be moved to another thread, but not `Sync`: it cannot be shared
/// between threads.
pub struct SPI {
    file: File,
    _not_sync: PhantomData<Cell<()>>,
}

/// SPI clock mode, i.e. the combination of clock polarity (CPOL) and clock
/// phase (CPHA).
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
#[repr(u8)]
pub enum Mode {
    /// CPOL = 0, CPHA = 0.
    Mode0 = 0,
    /// CPOL = 0, CPHA = 1.
    Mode1 = 1,
    /// CPOL = 1, CPHA = 0.
    Mode2 = 2,
    /// CPOL = 1, CPHA = 1.
    Mode3 = 3,
}

/// Order in which the bits of a word are shifted out on the wire.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum BitOrder {
    /// Most significant bit first.
    MsbFirst = 0,
    /// Least significant bit first.
    LsbFirst = 1,
}

/// Slave Select polarities.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Polarity {
    /// Chip select is asserted low (the common case).
    ActiveLow = 0,
    /// Chip select is asserted high.
    ActiveHigh = 1,
}

impl fmt::Display for Polarity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Polarity::ActiveLow => write!(f, "ActiveLow"),
            Polarity::ActiveHigh => write!(f, "ActiveHigh"),
        }
    }
}

impl fmt::Display for BitOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            BitOrder::MsbFirst => write!(f, "MsbFirst"),
            BitOrder::LsbFirst => write!(f, "LsbFirst"),
        }
    }
}

/// A single SPI transfer, as understood by the `SPI_IOC_MESSAGE` ioctl.
///
/// Alias of [`private::spi_ioc_transfer`], which mirrors the C struct of the
/// same name.
pub use private::spi_ioc_transfer as SpidevTransfer;

impl SPI {
    /// Opens `/dev/spidev<bus>.<slave>` and applies `mode`, a word size of 8
    /// bits and `speed_hz`.
    ///
    /// The requested frequency is handed to the kernel as-is; the controller
    /// clamps it to the nearest frequency it can actually generate.
    pub fn new(bus: u8, slave: u8, speed_hz: u32, mode: Mode) -> io::Result<SPI> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/spidev{}.{}", bus, slave))?;

        let spi = SPI {
            file,
            _not_sync: PhantomData,
        };

        spi.set_mode(mode)?;
        spi.set_bits_per_word(8)?;
        spi.set_speed_hz(speed_hz)?;

        Ok(spi)
    }

    /// Returns the current clock mode.
    pub fn mode(&self) -> io::Result<Mode> {
        let mut mode: u8 = 0;

        private::get_mode_u8(self.file.as_raw_fd(), &mut mode)?;

        Ok(match mode & 0x03 {
            0x01 => Mode::Mode1,
            0x02 => Mode::Mode2,
            0x03 => Mode::Mode3,
            _ => Mode::Mode0,
        })
    }

    /// Changes the clock mode, leaving the other mode flags untouched.
    pub fn set_mode(&self, mode: Mode) -> io::Result<()> {
        let mut old_mode: u8 = 0;

        private::get_mode_u8(self.file.as_raw_fd(), &mut old_mode)?;

        // Only the CPOL/CPHA bits are replaced, so that flags such as
        // SPI_CS_HIGH, SPI_3WIRE, SPI_NO_CS or SPI_LOOP survive.
        let new_mode = (old_mode & !(private::SPI_CPOL | private::SPI_CPHA)) | (mode as u8);

        private::set_mode_u8(self.file.as_raw_fd(), &new_mode)?;

        Ok(())
    }

    /// Returns the maximum clock frequency in Hz.
    pub fn speed_hz(&self) -> io::Result<u32> {
        let mut speed_hz: u32 = 0;

        private::get_max_speed_hz(self.file.as_raw_fd(), &mut speed_hz)?;

        Ok(speed_hz)
    }

    /// Sets the maximum clock frequency in Hz.
    pub fn set_speed_hz(&self, speed_hz: u32) -> io::Result<()> {
        private::set_max_speed_hz(self.file.as_raw_fd(), &speed_hz)?;

        Ok(())
    }

    /// Returns the number of bits per word.
    pub fn bits_per_word(&self) -> io::Result<u8> {
        let mut bits_per_word: u8 = 0;

        private::get_bits_per_word(self.file.as_raw_fd(), &mut bits_per_word)?;

        Ok(bits_per_word)
    }

    /// Sets the number of bits per word; 8 is the usual value.
    pub fn set_bits_per_word(&self, size: u8) -> io::Result<()> {
        private::set_bits_per_word(self.file.as_raw_fd(), &size)?;

        Ok(())
    }

    /// Returns the bit order used on the wire.
    pub fn bit_order(&self) -> io::Result<BitOrder> {
        let mut bit_order: u8 = 0;

        private::get_lsb_first(self.file.as_raw_fd(), &mut bit_order)?;

        Ok(match bit_order {
            0 => BitOrder::MsbFirst,
            _ => BitOrder::LsbFirst,
        })
    }

    /// Selects the bit order used on the wire.
    pub fn set_bit_order(&self, bit_order: BitOrder) -> io::Result<()> {
        private::set_lsb_first(self.file.as_raw_fd(), &(bit_order as u8))?;

        Ok(())
    }

    /// Returns the slave-select polarity.
    pub fn ss_polarity(&self) -> io::Result<Polarity> {
        let mut mode: u8 = 0;

        private::get_mode_u8(self.file.as_raw_fd(), &mut mode)?;

        if (mode & private::SPI_CS_HIGH) == 0 {
            return Ok(Polarity::ActiveLow);
        }

        Ok(Polarity::ActiveHigh)
    }

    /// Sets the slave-select polarity, preserving the CPOL/CPHA bits.
    pub fn set_ss_polarity(&self, polarity: Polarity) -> io::Result<()> {
        let mut mode: u8 = 0;

        private::get_mode_u8(self.file.as_raw_fd(), &mut mode)?;

        if polarity == Polarity::ActiveHigh {
            mode |= private::SPI_CS_HIGH;
        } else {
            mode &= !private::SPI_CS_HIGH;
        }

        private::set_mode_u8(self.file.as_raw_fd(), &mode)?;

        Ok(())
    }

    /// Reads into `buffer` using the device's streaming semantics.
    ///
    /// Most devices need a command to be written first; prefer
    /// [`SPI::transfer`] for request/response style peripherals.
    pub fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }

    /// Writes `buffer` using the device's streaming semantics.
    pub fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }

    /// Executes a single transfer.
    ///
    /// When `transfer` carries an `rx_buf`, the kernel writes the received
    /// bytes directly into it before this call returns.
    pub fn transfer(&self, transfer: &mut SpidevTransfer) -> io::Result<()> {
        // The kernel will directly modify the rx_buf of the SpidevTransfer
        // rx_buf if present, so there is no need to do any additional work
        private::spidev_transfer(self.file.as_raw_fd(), transfer)?;

        Ok(())
    }

    /// Executes several transfers as a single, uninterrupted `SPI_IOC_MESSAGE`
    /// operation.
    pub fn transfer_multiple(&self, transfers: &mut [SpidevTransfer]) -> io::Result<()> {
        private::spidev_transfer_buf(self.file.as_raw_fd(), transfers)?;

        Ok(())
    }
}

impl AsRawFd for SPI {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl fmt::Debug for SPI {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SPI")
            .field("mode", &self.mode())
            .field("speed_hz", &self.speed_hz())
            .field("bits_per_word", &self.bits_per_word())
            .field("bit_order", &self.bit_order())
            .field("ss_polarity", &self.ss_polarity())
            .finish()
    }
}

mod private {
    /// Mirrors the `request` parameter type of `libc::ioctl`, which differs
    /// between C libraries (`c_ulong` on gnu/uclibc, `c_int` on musl/android).
    type IoctlNumType = libc::Ioctl;

    /// Clock Phase
    pub const SPI_CPHA: u8 = 0x01;
    /// Clock Polarity
    pub const SPI_CPOL: u8 = 0x02;

    pub const SPI_MODE_0: u8 = 0;
    pub const SPI_MODE_1: u8 = SPI_CPHA;
    pub const SPI_MODE_2: u8 = SPI_CPOL;
    pub const SPI_MODE_3: u8 = SPI_CPOL | SPI_CPHA;

    /// Chipselect Active High?
    pub const SPI_CS_HIGH: u8 = 0x04;
    /// Per-word Bits On Wire
    pub const SPI_LSB_FIRST: u8 = 0x08;
    /// SI/SO Signals Shared
    pub const SPI_3WIRE: u8 = 0x10;
    /// Loopback Mode
    pub const SPI_LOOP: u8 = 0x20;
    /// 1 dev/bus; no chipselect
    pub const SPI_NO_CS: u8 = 0x40;
    /// Slave pulls low to pause
    pub const SPI_READY: u8 = 0x80;

    /// Transmit with 2 wires
    pub const SPI_TX_DUAL: u32 = 0x100;
    /// Transmit with 4 wires
    pub const SPI_TX_QUAD: u32 = 0x200;
    /// Receive with 2 wires
    pub const SPI_RX_DUAL: u32 = 0x400;
    /// Receive with 4 wires
    pub const SPI_RX_QUAD: u32 = 0x800;

    const SPI_IOC_MAGIC: u8 = b'k';
    const SPI_IOC_NR_TRANSFER: u8 = 0;
    const SPI_IOC_NR_MODE: u8 = 1;
    const SPI_IOC_NR_LSB_FIRST: u8 = 2;
    const SPI_IOC_NR_BITS_PER_WORD: u8 = 3;
    const SPI_IOC_NR_MAX_SPEED_HZ: u8 = 4;
    const SPI_IOC_NR_MODE32: u8 = 5;

    const NONE: u8 = 0;
    const READ: u8 = 2;
    const WRITE: u8 = 1;
    const SIZEBITS: u8 = 14;
    const DIRBITS: u8 = 2;

    const NRBITS: IoctlNumType = 8;
    const TYPEBITS: IoctlNumType = 8;

    const NRSHIFT: IoctlNumType = 0;
    const TYPESHIFT: IoctlNumType = NRSHIFT + NRBITS as IoctlNumType;
    const SIZESHIFT: IoctlNumType = TYPESHIFT + TYPEBITS as IoctlNumType;
    const DIRSHIFT: IoctlNumType = SIZESHIFT + SIZEBITS as IoctlNumType;

    const NRMASK: IoctlNumType = (1 << NRBITS) - 1;
    const TYPEMASK: IoctlNumType = (1 << TYPEBITS) - 1;
    const SIZEMASK: IoctlNumType = (1 << SIZEBITS) - 1;
    const DIRMASK: IoctlNumType = (1 << DIRBITS) - 1;

    macro_rules! ioc {
        ($dir:expr, $ty:expr, $nr:expr, $sz:expr) => {
            (($dir as IoctlNumType & DIRMASK) << DIRSHIFT)
                | (($ty as IoctlNumType & TYPEMASK) << TYPESHIFT)
                | (($nr as IoctlNumType & NRMASK) << NRSHIFT)
                | (($sz as IoctlNumType & SIZEMASK) << SIZESHIFT)
        };
    }

    // const SPI_IOC_RD_MODE: IoctlNumType = ioc!(READ, SPI_IOC_MAGIC, SPI_IOC_NR_MODE, mem::size_of::<u8>());

    macro_rules! request_code_none {
        ($ty:expr, $nr:expr) => {
            ioc!(NONE, $ty, $nr, 0)
        };
    }

    macro_rules! request_code_read {
        ($ty:expr, $nr:expr, $sz:expr) => {
            ioc!(READ, $ty, $nr, $sz)
        };
    }

    macro_rules! request_code_write {
        ($ty:expr, $nr:expr, $sz:expr) => {
            ioc!(WRITE, $ty, $nr, $sz)
        };
    }

    macro_rules! request_code_readwrite {
        ($ty:expr, $nr:expr, $sz:expr) => {
            ioc!(READ | WRITE, $ty, $nr, $sz)
        };
    }

    macro_rules! ioctl_read {
        ($(#[$attr:meta])* $name:ident, $ioty:expr, $nr:expr, $ty:ty) => (
            $(#[$attr])*
            pub fn $name(fd: std::os::raw::c_int, data: *mut $ty) -> std::io::Result<std::os::raw::c_int> {
                syscall!(ioctl(fd, request_code_read!($ioty, $nr, std::mem::size_of::<$ty>()) as IoctlNumType, data))
            }
        )
    }

    macro_rules! ioctl_write_ptr {
        ($(#[$attr:meta])* $name:ident, $ioty:expr, $nr:expr, $ty:ty) => (
            $(#[$attr])*
            pub fn $name(fd: std::os::raw::c_int, data: *const $ty) -> std::io::Result<std::os::raw::c_int> {
                syscall!(ioctl(fd, request_code_write!($ioty, $nr, std::mem::size_of::<$ty>()) as IoctlNumType, data))
            }
        )
    }

    macro_rules! ioctl_read_buf {
        ($(#[$attr:meta])* $name:ident, $ioty:expr, $nr:expr, $ty:ty) => (
            $(#[$attr])*
            pub fn $name(fd: std::os::raw::c_int,
                                data: &mut [$ty])
                                -> std::io::Result<std::os::raw::c_int> {
                syscall!(ioctl(fd, request_code_read!($ioty, $nr, data.len() * ::std::mem::size_of::<$ty>()) as IoctlNumType, data))
            }
        )
    }

    macro_rules! ioctl_write_buf {
        ($(#[$attr:meta])* $name:ident, $ioty:expr, $nr:expr, $ty:ty) => (
            $(#[$attr])*
            pub fn $name(fd: std::os::raw::c_int, data: &[$ty]) -> std::io::Result<std::os::raw::c_int> {
                syscall!(ioctl(fd, request_code_write!($ioty, $nr, data.len() * ::std::mem::size_of::<$ty>()) as IoctlNumType, data))
            }
        )
    }

    macro_rules! ioctl_readwrite_buf {
        ($(#[$attr:meta])* $name:ident, $ioty:expr, $nr:expr, $ty:ty) => (
            $(#[$attr])*
            pub fn $name(fd: std::os::raw::c_int,
                                data: &mut [$ty])
                                -> std::io::Result<std::os::raw::c_int> {
                syscall!(ioctl(fd, request_code_readwrite!($ioty, $nr, data.len() * ::std::mem::size_of::<$ty>()) as IoctlNumType, data))
            }
        )
    }

    ioctl_read!(get_mode_u8, SPI_IOC_MAGIC, SPI_IOC_NR_MODE, u8);
    ioctl_write_ptr!(set_mode_u8, SPI_IOC_MAGIC, SPI_IOC_NR_MODE, u8);
    ioctl_read!(get_mode_u32, SPI_IOC_MAGIC, SPI_IOC_NR_MODE32, u32);
    ioctl_write_ptr!(set_mode32, SPI_IOC_MAGIC, SPI_IOC_NR_MODE32, u32);

    ioctl_read!(get_lsb_first, SPI_IOC_MAGIC, SPI_IOC_NR_LSB_FIRST, u8);
    ioctl_write_ptr!(set_lsb_first, SPI_IOC_MAGIC, SPI_IOC_NR_LSB_FIRST, u8);

    ioctl_read!(
        get_bits_per_word,
        SPI_IOC_MAGIC,
        SPI_IOC_NR_BITS_PER_WORD,
        u8
    );
    ioctl_write_ptr!(
        set_bits_per_word,
        SPI_IOC_MAGIC,
        SPI_IOC_NR_BITS_PER_WORD,
        u8
    );

    ioctl_read!(
        get_max_speed_hz,
        SPI_IOC_MAGIC,
        SPI_IOC_NR_MAX_SPEED_HZ,
        u32
    );
    ioctl_write_ptr!(
        set_max_speed_hz,
        SPI_IOC_MAGIC,
        SPI_IOC_NR_MAX_SPEED_HZ,
        u32
    );

    /// Mirrors `struct spi_ioc_transfer` from `linux/spi/spidev.h`.
    ///
    /// Build one with [`Self::read`], [`Self::write`] or [`Self::read_write`],
    /// then hand it to [`crate::sys::spi::SPI::transfer`] or
    /// [`crate::sys::spi::SPI::transfer_multiple`].
    ///
    /// The layout must stay field-for-field compatible with the kernel
    /// definition, including the trailing `tx_nbits`/`rx_nbits`/
    /// `word_delay_usecs` bytes.
    #[allow(non_camel_case_types)]
    #[derive(Debug, Default)]
    #[repr(C)]
    pub struct spi_ioc_transfer<'a, 'b> {
        tx_buf: u64,
        rx_buf: u64,
        len: u32,

        // optional overrides
        /// Per-transfer clock frequency override, in Hz (0 keeps the default).
        pub speed_hz: u32,
        /// Delay after the last bit, in microseconds, before deselecting.
        pub delay_usecs: u16,
        /// Per-transfer word size override (0 keeps the default).
        pub bits_per_word: u8,
        /// Deselect the device before starting the next transfer.
        pub cs_change: u8,
        /// Bits used for transmission: 1 (single), 2 (dual) or 4 (quad).
        pub tx_nbits: u8,
        /// Bits used for reception: 1 (single), 2 (dual) or 4 (quad).
        pub rx_nbits: u8,
        /// Delay between words of the same transfer, in microseconds.
        pub word_delay_usecs: u8,
        pad: u8,

        tx_buf_ref: std::marker::PhantomData<&'a [u8]>,
        rx_buf_ref: std::marker::PhantomData<&'b mut [u8]>,
    }

    impl<'a, 'b> spi_ioc_transfer<'a, 'b> {
        /// Builds a receive-only transfer.
        pub fn read(buff: &'b mut [u8]) -> Self {
            spi_ioc_transfer {
                rx_buf: buff.as_ptr() as *const () as usize as u64,
                len: buff.len() as u32,
                ..Default::default()
            }
        }

        /// Builds a transmit-only transfer.
        pub fn write(buff: &'a [u8]) -> Self {
            spi_ioc_transfer {
                tx_buf: buff.as_ptr() as *const () as usize as u64,
                len: buff.len() as u32,
                ..Default::default()
            }
        }

        /// The `tx_buf` and `rx_buf` must be the same length.
        pub fn read_write(tx_buf: &'a [u8], rx_buf: &'b mut [u8]) -> Self {
            assert_eq!(tx_buf.len(), rx_buf.len());
            spi_ioc_transfer {
                rx_buf: rx_buf.as_ptr() as *const () as usize as u64,
                tx_buf: tx_buf.as_ptr() as *const () as usize as u64,
                len: tx_buf.len() as u32,
                ..Default::default()
            }
        }
    }

    ioctl_write_ptr!(
        spidev_transfer,
        SPI_IOC_MAGIC,
        SPI_IOC_NR_TRANSFER,
        spi_ioc_transfer
    );
    ioctl_write_buf!(
        spidev_transfer_buf,
        SPI_IOC_MAGIC,
        SPI_IOC_NR_TRANSFER,
        spi_ioc_transfer
    );
}
