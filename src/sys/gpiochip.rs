//! Linux GPIO access through the `gpiochip` character device.
//!
//! This is the interface modern kernels expect. [`Chip`] opens
//! `/dev/gpiochip<N>` and [`Chip::request_lines`] claims lines through
//! `GPIO_V2_GET_LINE_IOCTL`, which replaced the legacy sysfs interface in
//! [`crate::sys::gpio`] (that one needs `CONFIG_GPIO_SYSFS`, disabled on most
//! recent kernels).
//!
//! Access normally requires `root` or membership in the `gpio` group.
//!
//! Blocking on an edge gives up the thread until the kernel has an event, and
//! the timestamp comes from `CLOCK_MONOTONIC`:
//!
//! ```no_run
//! use periphery::sys::gpiochip::{Chip, Edge, EventKind, LineSettings};
//! use std::time::Duration;
//!
//! # fn main() -> std::io::Result<()> {
//! let chip = Chip::new(0)?;
//! let line = chip.request_lines(
//!     &[17],
//!     "my-app",
//!     &LineSettings::new().input().edge(Edge::Both),
//! )?;
//!
//! while let Some(event) = line.wait_event(Some(Duration::from_secs(1)))? {
//!     if event.kind() == EventKind::Rising {
//!         println!("line {} went high", event.offset());
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Driving an output:
//!
//! ```no_run
//! use periphery::sys::gpiochip::{Chip, LineSettings, Value};
//!
//! # fn main() -> std::io::Result<()> {
//! let chip = Chip::new(0)?;
//! let line = chip.request_lines(
//!     &[17],
//!     "my-app",
//!     &LineSettings::new().output().initial_value(Value::Low),
//! )?;
//!
//! line.set_value(0, Value::High)?;
//! # Ok(())
//! # }
//! ```

use std::cell::Cell;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::marker::PhantomData;
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;
use std::time::Duration;

pub use super::gpio::{Edge, Value};

/// Length of the fixed-size name and consumer fields in the kernel ABI.
const NAME_SIZE: usize = 32;
/// Maximum number of lines that can be claimed by a single request.
const LINES_MAX: usize = 64;
/// Maximum number of attributes a request configuration can carry.
const ATTRS_MAX: usize = 10;

const CHIP_PATH_PREFIX: &str = "/dev/gpiochip";

// ioctl numbers, computed the same way the `_IOC` macros do.
const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;

const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const GPIO_MAGIC: u32 = 0xB4;

/// Builds a `_IOC` request code for a driver-private ioctl.
const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    ((dir & 0x3) << IOC_DIRSHIFT)
        | ((ty & 0xff) << IOC_TYPESHIFT)
        | ((nr & 0xff) << IOC_NRSHIFT)
        | ((size & 0x3fff) << IOC_SIZESHIFT)
}

// `enum gpio_v2_line_flag`
const LINE_FLAG_ACTIVE_LOW: u64 = 1 << 1;
const LINE_FLAG_INPUT: u64 = 1 << 2;
const LINE_FLAG_OUTPUT: u64 = 1 << 3;
const LINE_FLAG_EDGE_RISING: u64 = 1 << 4;
const LINE_FLAG_EDGE_FALLING: u64 = 1 << 5;
const LINE_FLAG_BIAS_PULL_UP: u64 = 1 << 8;
const LINE_FLAG_BIAS_PULL_DOWN: u64 = 1 << 9;
const LINE_FLAG_BIAS_DISABLED: u64 = 1 << 10;

// `enum gpio_v2_line_attr_id`
const ATTR_ID_OUTPUT_VALUES: u32 = 2;
const ATTR_ID_DEBOUNCE: u32 = 3;

// `enum gpio_v2_line_event_id`
const EVENT_RISING_EDGE: u32 = 1;
const EVENT_FALLING_EDGE: u32 = 2;

/// Number of events the kernel is asked to buffer per request.
const EVENT_BUFFER_SIZE: u32 = 16;

#[repr(C)]
struct ChipInfo {
    name: [u8; NAME_SIZE],
    label: [u8; NAME_SIZE],
    lines: u32,
}

impl ChipInfo {
    fn new() -> ChipInfo {
        ChipInfo {
            name: [0; NAME_SIZE],
            label: [0; NAME_SIZE],
            lines: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct LineAttribute {
    id: u32,
    padding: u32,
    /// Union of `flags`, `values` and `debounce_period_us`.
    value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct LineConfigAttribute {
    attr: LineAttribute,
    mask: u64,
}

const EMPTY_ATTRIBUTE: LineConfigAttribute = LineConfigAttribute {
    attr: LineAttribute {
        id: 0,
        padding: 0,
        value: 0,
    },
    mask: 0,
};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct LineConfig {
    flags: u64,
    num_attrs: u32,
    padding: [u32; 5],
    attrs: [LineConfigAttribute; ATTRS_MAX],
}

impl LineConfig {
    fn new(flags: u64) -> LineConfig {
        LineConfig {
            flags,
            num_attrs: 0,
            padding: [0; 5],
            attrs: [EMPTY_ATTRIBUTE; ATTRS_MAX],
        }
    }

    fn push(&mut self, id: u32, value: u64, mask: u64) -> io::Result<()> {
        let index = self.num_attrs as usize;
        let Some(slot) = self.attrs.get_mut(index) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("at most {ATTRS_MAX} line attributes are supported"),
            ));
        };

        *slot = LineConfigAttribute {
            attr: LineAttribute {
                id,
                padding: 0,
                value,
            },
            mask,
        };
        self.num_attrs += 1;

        Ok(())
    }
}

#[repr(C)]
struct LineRequestBuffer {
    offsets: [u32; LINES_MAX],
    consumer: [u8; NAME_SIZE],
    config: LineConfig,
    num_lines: u32,
    event_buffer_size: u32,
    padding: [u32; 5],
    fd: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct LineValuesBuffer {
    bits: u64,
    mask: u64,
}

#[repr(C)]
struct LineEventBuffer {
    timestamp_ns: u64,
    id: u32,
    offset: u32,
    seqno: u32,
    line_seqno: u32,
    padding: [u32; 6],
}

impl LineEventBuffer {
    fn new() -> LineEventBuffer {
        LineEventBuffer {
            timestamp_ns: 0,
            id: 0,
            offset: 0,
            seqno: 0,
            line_seqno: 0,
            padding: [0; 6],
        }
    }
}

// These sizes are part of the ioctl request code, so a mismatch would silently
// address the wrong ioctl instead of failing to compile.
const _: () = {
    assert!(size_of::<ChipInfo>() == 68);
    assert!(size_of::<LineConfig>() == 272);
    assert!(size_of::<LineRequestBuffer>() == 592);
    assert!(size_of::<LineValuesBuffer>() == 16);
    assert!(size_of::<LineEventBuffer>() == 48);
};

const GPIO_GET_CHIPINFO_IOCTL: u32 = ioc(IOC_READ, GPIO_MAGIC, 0x01, size_of::<ChipInfo>() as u32);
const GPIO_V2_GET_LINE_IOCTL: u32 = ioc(
    IOC_READ | IOC_WRITE,
    GPIO_MAGIC,
    0x07,
    size_of::<LineRequestBuffer>() as u32,
);
const GPIO_V2_LINE_GET_VALUES_IOCTL: u32 = ioc(
    IOC_READ | IOC_WRITE,
    GPIO_MAGIC,
    0x0E,
    size_of::<LineValuesBuffer>() as u32,
);
const GPIO_V2_LINE_SET_VALUES_IOCTL: u32 = ioc(
    IOC_READ | IOC_WRITE,
    GPIO_MAGIC,
    0x0F,
    size_of::<LineValuesBuffer>() as u32,
);

/// Direction under which lines are requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Lines are inputs.
    Input,
    /// Lines are outputs.
    Output,
}

/// Internal bias applied to the requested lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bias {
    /// Leave the bias as the pin defaults to.
    Default,
    /// Enable the internal pull-up.
    PullUp,
    /// Enable the internal pull-down.
    PullDown,
    /// Explicitly disable any bias.
    Disabled,
}

/// Description of how a set of lines should be requested.
///
/// The builder methods consume the value, so a configuration reads as a chain:
///
/// ```
/// use periphery::sys::gpiochip::{Bias, Edge, LineSettings, Value};
///
/// let settings = LineSettings::new()
///     .output()
///     .initial_value(Value::Low)
///     .bias(Bias::Disabled);
///
/// let _input = LineSettings::new().input().edge(Edge::Both).debounce_us(5_000);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct LineSettings {
    direction: Direction,
    initial_value: Option<Value>,
    edge: Edge,
    bias: Bias,
    active_low: bool,
    debounce_us: Option<u32>,
}

impl LineSettings {
    /// Creates settings for plain input lines.
    pub fn new() -> LineSettings {
        LineSettings {
            direction: Direction::Input,
            initial_value: None,
            edge: Edge::None,
            bias: Bias::Default,
            active_low: false,
            debounce_us: None,
        }
    }

    /// Requests the lines as inputs.
    pub fn input(mut self) -> Self {
        self.direction = Direction::Input;
        self
    }

    /// Requests the lines as outputs.
    pub fn output(mut self) -> Self {
        self.direction = Direction::Output;
        self
    }

    /// Drives the lines to `value` as soon as the request is granted.
    ///
    /// This is applied atomically by the kernel, so the line never shows an
    /// intermediate level. Only meaningful for outputs.
    pub fn initial_value(mut self, value: Value) -> Self {
        self.initial_value = Some(value);
        self
    }

    /// Reports edges on the lines as [`LineEvent`]s.
    ///
    /// Only valid for inputs; the kernel rejects the request otherwise.
    pub fn edge(mut self, edge: Edge) -> Self {
        self.edge = edge;
        self
    }

    /// Sets the internal bias of the lines.
    pub fn bias(mut self, bias: Bias) -> Self {
        self.bias = bias;
        self
    }

    /// Treats the lines as active-low.
    pub fn active_low(mut self, active_low: bool) -> Self {
        self.active_low = active_low;
        self
    }

    /// Ignores edges shorter than `period_us` microseconds.
    pub fn debounce_us(mut self, period_us: u32) -> Self {
        self.debounce_us = Some(period_us);
        self
    }

    fn flags(&self) -> u64 {
        let mut flags = match self.direction {
            Direction::Input => LINE_FLAG_INPUT,
            Direction::Output => LINE_FLAG_OUTPUT,
        };

        flags |= match self.edge {
            Edge::None => 0,
            Edge::Rising => LINE_FLAG_EDGE_RISING,
            Edge::Falling => LINE_FLAG_EDGE_FALLING,
            Edge::Both => LINE_FLAG_EDGE_RISING | LINE_FLAG_EDGE_FALLING,
        };

        flags |= match self.bias {
            Bias::Default => 0,
            Bias::PullUp => LINE_FLAG_BIAS_PULL_UP,
            Bias::PullDown => LINE_FLAG_BIAS_PULL_DOWN,
            Bias::Disabled => LINE_FLAG_BIAS_DISABLED,
        };

        if self.active_low {
            flags |= LINE_FLAG_ACTIVE_LOW;
        }

        flags
    }

    fn build_config(&self, num_lines: usize) -> io::Result<LineConfig> {
        if self.direction == Direction::Output && self.edge != Edge::None {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "edge detection is only available on input lines",
            ));
        }
        if self.direction == Direction::Input && self.initial_value.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an initial value is only available on output lines",
            ));
        }

        let mask = bit_mask(num_lines)?;
        let mut config = LineConfig::new(self.flags());

        if let Some(value) = self.initial_value {
            let bits = match value {
                Value::Low => 0,
                Value::High => u64::MAX,
            };
            config.push(ATTR_ID_OUTPUT_VALUES, bits & mask, mask)?;
        }

        if let Some(period_us) = self.debounce_us {
            config.push(ATTR_ID_DEBOUNCE, u64::from(period_us), mask)?;
        }

        Ok(config)
    }
}

impl Default for LineSettings {
    fn default() -> Self {
        LineSettings::new()
    }
}

/// Kind of edge that produced a [`LineEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// The line became active (inactive to active).
    Rising,
    /// The line became inactive (active to inactive).
    Falling,
}

/// An edge reported by the kernel for one of the requested lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineEvent {
    timestamp_ns: u64,
    kind: EventKind,
    offset: u32,
    seqno: u32,
    line_seqno: u32,
}

impl LineEvent {
    /// Monotonic timestamp of the edge, in nanoseconds.
    pub fn timestamp_ns(&self) -> u64 {
        self.timestamp_ns
    }

    /// Whether the line went active or inactive.
    pub fn kind(&self) -> EventKind {
        self.kind
    }

    /// Offset of the line that triggered the event.
    pub fn offset(&self) -> u32 {
        self.offset
    }

    /// Sequence number of the event across all lines of the request.
    pub fn seqno(&self) -> u32 {
        self.seqno
    }

    /// Sequence number of the event on its own line.
    pub fn line_seqno(&self) -> u32 {
        self.line_seqno
    }
}

/// A GPIO controller, i.e. `/dev/gpiochip<N>`.
///
/// The handle is `Send` but not `Sync`: claiming lines and the subsequent
/// transfers form one logical sequence that must not be interleaved.
pub struct Chip {
    file: File,
    name: String,
    label: String,
    lines: u32,
    _not_sync: PhantomData<Cell<()>>,
}

impl Chip {
    /// Opens `/dev/gpiochip<number>`.
    pub fn new(number: u32) -> io::Result<Chip> {
        Chip::open(format!("{CHIP_PATH_PREFIX}{number}"))
    }

    /// Opens the controller at `path` and queries its information.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Chip> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        let mut info = ChipInfo::new();
        syscall!(ioctl(
            file.as_raw_fd(),
            GPIO_GET_CHIPINFO_IOCTL as libc::Ioctl,
            &mut info
        ))?;

        Ok(Chip {
            file,
            name: fixed_string(&info.name),
            label: fixed_string(&info.label),
            lines: info.lines,
            _not_sync: PhantomData,
        })
    }

    /// Kernel name of the controller, e.g. `gpiochip0`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Functional label of the controller; empty when the kernel provides none.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Number of lines on this controller.
    pub fn lines(&self) -> u32 {
        self.lines
    }

    /// Claims the lines at `offsets` for exclusive use by `consumer`.
    ///
    /// The lines stay claimed until the returned [`Line`] is dropped, which
    /// closes the underlying file descriptor and releases them.
    pub fn request_lines(
        &self,
        offsets: &[u32],
        consumer: &str,
        settings: &LineSettings,
    ) -> io::Result<Line> {
        check_offsets(offsets, self.lines, &self.name)?;

        let config = settings.build_config(offsets.len())?;

        let mut request = LineRequestBuffer {
            offsets: [0; LINES_MAX],
            consumer: [0; NAME_SIZE],
            config,
            num_lines: offsets.len() as u32,
            event_buffer_size: if settings.edge == Edge::None {
                0
            } else {
                EVENT_BUFFER_SIZE
            },
            padding: [0; 5],
            fd: -1,
        };
        request.offsets[..offsets.len()].copy_from_slice(offsets);
        write_fixed_string(&mut request.consumer, consumer);

        syscall!(ioctl(
            self.file.as_raw_fd(),
            GPIO_V2_GET_LINE_IOCTL as libc::Ioctl,
            &mut request
        ))?;

        if request.fd < 0 {
            return Err(io::Error::other(
                "the kernel granted the request without returning a file descriptor",
            ));
        }

        // SAFETY: the kernel just handed us a newly created, owned descriptor.
        let file = unsafe { File::from_raw_fd(request.fd) };

        Ok(Line {
            file,
            offsets: offsets.to_vec(),
            has_events: settings.edge != Edge::None,
            _not_sync: PhantomData,
        })
    }
}

impl AsRawFd for Chip {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl fmt::Debug for Chip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chip")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("lines", &self.lines)
            .finish()
    }
}

/// Lines claimed from a [`Chip`], owned through their own file descriptor.
///
/// Dropping the value releases the lines back to the system.
pub struct Line {
    file: File,
    offsets: Vec<u32>,
    has_events: bool,
    _not_sync: PhantomData<Cell<()>>,
}

impl Line {
    /// Number of claimed lines.
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    /// Always `false`: a request has at least one line.
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Offsets of the claimed lines, in request order.
    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    /// Reads all claimed lines at once, as a bitmap indexed by request order.
    pub fn get_all(&self) -> io::Result<u64> {
        let mask = self.bit_mask();
        let mut values = LineValuesBuffer { bits: 0, mask };

        syscall!(ioctl(
            self.file.as_raw_fd(),
            GPIO_V2_LINE_GET_VALUES_IOCTL as libc::Ioctl,
            &mut values
        ))?;

        Ok(values.bits & mask)
    }

    /// Reads the line at `index` within the request.
    pub fn get_value(&self, index: usize) -> io::Result<Value> {
        let bit = 1u64 << self.check_index(index)?;
        let mut values = LineValuesBuffer { bits: 0, mask: bit };

        syscall!(ioctl(
            self.file.as_raw_fd(),
            GPIO_V2_LINE_GET_VALUES_IOCTL as libc::Ioctl,
            &mut values
        ))?;

        Ok(if values.bits & bit == 0 {
            Value::Low
        } else {
            Value::High
        })
    }

    /// Writes all claimed lines at once from a bitmap in request order.
    pub fn set_all(&self, bits: u64) -> io::Result<()> {
        let mask = self.bit_mask();
        let values = LineValuesBuffer {
            bits: bits & mask,
            mask,
        };

        syscall!(ioctl(
            self.file.as_raw_fd(),
            GPIO_V2_LINE_SET_VALUES_IOCTL as libc::Ioctl,
            &values
        ))?;

        Ok(())
    }

    /// Writes the line at `index` within the request.
    pub fn set_value(&self, index: usize, value: Value) -> io::Result<()> {
        let bit = 1u64 << self.check_index(index)?;
        let values = LineValuesBuffer {
            bits: if value == Value::High { bit } else { 0 },
            mask: bit,
        };

        syscall!(ioctl(
            self.file.as_raw_fd(),
            GPIO_V2_LINE_SET_VALUES_IOCTL as libc::Ioctl,
            &values
        ))?;

        Ok(())
    }

    /// Waits for the next edge event, or until `timeout` elapses.
    ///
    /// Returns `Ok(None)` on timeout; a `timeout` of `None` blocks until an
    /// edge arrives. Fails with `InvalidInput` when the lines were not
    /// requested with [`LineSettings::edge`].
    pub fn wait_event(&self, timeout: Option<Duration>) -> io::Result<Option<LineEvent>> {
        if !self.has_events {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the lines were requested without edge detection",
            ));
        }

        let mut fds = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };

        let timeout_ms = match timeout {
            Some(duration) => duration.as_millis().min(i32::MAX as u128) as i32,
            None => -1,
        };

        if unsafe { libc::poll(&mut fds, 1, timeout_ms) } == -1 {
            return Err(io::Error::last_os_error());
        }

        if fds.revents == 0 {
            return Ok(None);
        }

        let mut event = LineEventBuffer::new();

        // SAFETY: `LineEventBuffer` is `#[repr(C)]`, holds only integers and is
        // fully initialised above, so treating it as a byte buffer is valid.
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(
                &mut event as *mut LineEventBuffer as *mut u8,
                size_of::<LineEventBuffer>(),
            )
        };
        (&self.file).read_exact(bytes)?;

        parse_event(&event).map(Some)
    }

    fn check_index(&self, index: usize) -> io::Result<u32> {
        if index >= self.offsets.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "line index {index} is out of range ({} lines requested)",
                    self.offsets.len()
                ),
            ));
        }

        Ok(index as u32)
    }

    fn bit_mask(&self) -> u64 {
        bit_mask(self.offsets.len()).unwrap_or(0)
    }
}

impl AsRawFd for Line {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl fmt::Debug for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Line")
            .field("offsets", &self.offsets)
            .field("has_events", &self.has_events)
            .finish()
    }
}

/// Bitmap covering the first `num_lines` bits.
fn bit_mask(num_lines: usize) -> io::Result<u64> {
    match num_lines {
        0 => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "at least one line has to be requested",
        )),
        n if n > LINES_MAX => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("at most {LINES_MAX} lines can be requested at once"),
        )),
        LINES_MAX => Ok(u64::MAX),
        n => Ok((1u64 << n) - 1),
    }
}

fn check_offsets(offsets: &[u32], lines: u32, name: &str) -> io::Result<()> {
    bit_mask(offsets.len())?;

    for &offset in offsets {
        if offset >= lines {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("line offset {offset} is out of range for {name}, which has {lines} lines"),
            ));
        }
    }

    Ok(())
}

/// Decodes a raw event as delivered by the kernel.
fn parse_event(raw: &LineEventBuffer) -> io::Result<LineEvent> {
    let kind = match raw.id {
        EVENT_RISING_EDGE => EventKind::Rising,
        EVENT_FALLING_EDGE => EventKind::Falling,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown GPIO line event id: {other}"),
            ));
        }
    };

    Ok(LineEvent {
        timestamp_ns: raw.timestamp_ns,
        kind,
        offset: raw.offset,
        seqno: raw.seqno,
        line_seqno: raw.line_seqno,
    })
}

/// Decodes a fixed-size, NUL-terminated byte array.
fn fixed_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Writes `value` into a fixed-size, NUL-terminated byte array, truncating it if
/// the buffer is too small.
fn write_fixed_string(buffer: &mut [u8], value: &str) {
    let bytes = value.as_bytes();
    let len = bytes.len().min(buffer.len() - 1);

    buffer[..len].copy_from_slice(&bytes[..len]);
    buffer[len] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Request codes must match what `gcc` computes for linux/gpio.h, otherwise
    /// every ioctl would silently address the wrong command.
    #[test]
    fn ioctl_request_codes_match_the_kernel_header() {
        assert_eq!(GPIO_GET_CHIPINFO_IOCTL, 0x8044_b401);
        assert_eq!(GPIO_V2_GET_LINE_IOCTL, 0xc250_b407);
        assert_eq!(GPIO_V2_LINE_GET_VALUES_IOCTL, 0xc010_b40e);
        assert_eq!(GPIO_V2_LINE_SET_VALUES_IOCTL, 0xc010_b40f);
    }

    #[test]
    fn bit_mask_covers_exactly_the_requested_lines() {
        assert!(bit_mask(0).is_err());
        assert!(bit_mask(LINES_MAX + 1).is_err());
        assert_eq!(bit_mask(1).unwrap(), 0b1);
        assert_eq!(bit_mask(4).unwrap(), 0b1111);
        assert_eq!(bit_mask(63).unwrap(), (1u64 << 63) - 1);
        assert_eq!(bit_mask(LINES_MAX).unwrap(), u64::MAX);
    }

    #[test]
    fn fixed_strings_are_decoded_and_truncated_at_the_nul() {
        let mut buffer = [b'x'; NAME_SIZE];
        assert_eq!(fixed_string(&buffer), "x".repeat(NAME_SIZE));

        buffer[3] = 0;
        assert_eq!(fixed_string(&buffer), "xxx");
    }

    #[test]
    fn write_fixed_string_always_terminates_and_never_overflows() {
        let mut buffer = [0xffu8; NAME_SIZE];
        write_fixed_string(&mut buffer, "hello");
        assert_eq!(&buffer[..6], b"hello\0");
        assert_eq!(fixed_string(&buffer), "hello");

        let long = "x".repeat(NAME_SIZE + 10);
        let mut buffer = [0u8; NAME_SIZE];
        write_fixed_string(&mut buffer, &long);
        assert_eq!(buffer[NAME_SIZE - 1], 0);
        assert_eq!(fixed_string(&buffer).len(), NAME_SIZE - 1);
    }

    #[test]
    fn input_flags_follow_the_requested_configuration() {
        let flags = LineSettings::new().input().flags();
        assert_eq!(flags, LINE_FLAG_INPUT);

        let flags = LineSettings::new().input().edge(Edge::Rising).flags();
        assert_eq!(flags, LINE_FLAG_INPUT | LINE_FLAG_EDGE_RISING);

        let flags = LineSettings::new().input().edge(Edge::Both).flags();
        assert_eq!(
            flags,
            LINE_FLAG_INPUT | LINE_FLAG_EDGE_RISING | LINE_FLAG_EDGE_FALLING
        );

        let flags = LineSettings::new().input().bias(Bias::PullUp).flags();
        assert_eq!(flags, LINE_FLAG_INPUT | LINE_FLAG_BIAS_PULL_UP);

        let flags = LineSettings::new().input().bias(Bias::Disabled).flags();
        assert_eq!(flags, LINE_FLAG_INPUT | LINE_FLAG_BIAS_DISABLED);

        let flags = LineSettings::new().input().active_low(true).flags();
        assert_eq!(flags, LINE_FLAG_INPUT | LINE_FLAG_ACTIVE_LOW);
    }

    #[test]
    fn output_configuration_carries_the_initial_value_as_an_attribute() {
        let settings = LineSettings::new().output().initial_value(Value::High);
        let config = settings.build_config(2).unwrap();

        assert_eq!(config.flags, LINE_FLAG_OUTPUT);
        assert_eq!(config.num_attrs, 1);
        assert_eq!(config.attrs[0].attr.id, ATTR_ID_OUTPUT_VALUES);
        assert_eq!(config.attrs[0].mask, 0b11);
        // High for every requested line; bits outside the mask are ignored by
        // the kernel and must stay clear.
        assert_eq!(config.attrs[0].attr.value, 0b11);

        let settings = LineSettings::new().output().initial_value(Value::Low);
        let config = settings.build_config(1).unwrap();
        assert_eq!(config.attrs[0].attr.value, 0);
        assert_eq!(config.attrs[0].mask, 0b1);
    }

    #[test]
    fn debounce_is_only_emitted_when_requested() {
        let config = LineSettings::new().input().build_config(1).unwrap();
        assert_eq!(config.num_attrs, 0);

        let config = LineSettings::new()
            .input()
            .debounce_us(5_000)
            .build_config(1)
            .unwrap();
        assert_eq!(config.num_attrs, 1);
        assert_eq!(config.attrs[0].attr.id, ATTR_ID_DEBOUNCE);
        assert_eq!(config.attrs[0].attr.value, 5_000);
    }

    #[test]
    fn contradictory_settings_are_rejected_before_any_ioctl() {
        let err = LineSettings::new()
            .output()
            .edge(Edge::Both)
            .build_config(1);
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidInput);

        let err = LineSettings::new()
            .input()
            .initial_value(Value::High)
            .build_config(1);
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidInput);

        let err = LineSettings::new().input().build_config(0);
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidInput);

        let err = LineSettings::new().input().build_config(LINES_MAX + 1);
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn offsets_are_validated_against_the_line_count() {
        assert!(check_offsets(&[0, 3], 4, "gpiochip0").is_ok());
        assert!(check_offsets(&[4], 4, "gpiochip0").is_err());
        assert!(check_offsets(&[], 4, "gpiochip0").is_err());

        let err = check_offsets(&[4], 4, "gpiochip0").unwrap_err();
        assert!(err.to_string().contains("gpiochip0"));
    }

    #[test]
    fn edge_events_are_decoded_including_the_timestamp() {
        let raw = LineEventBuffer {
            timestamp_ns: 123_456,
            id: EVENT_RISING_EDGE,
            offset: 7,
            seqno: 3,
            line_seqno: 2,
            padding: [0; 6],
        };

        let event = parse_event(&raw).unwrap();
        assert_eq!(event.kind(), EventKind::Rising);
        assert_eq!(event.timestamp_ns(), 123_456);
        assert_eq!(event.offset(), 7);
        assert_eq!(event.seqno(), 3);
        assert_eq!(event.line_seqno(), 2);

        let raw = LineEventBuffer {
            id: EVENT_FALLING_EDGE,
            ..raw
        };
        assert_eq!(parse_event(&raw).unwrap().kind(), EventKind::Falling);
    }

    #[test]
    fn unknown_event_ids_are_reported_as_invalid_data() {
        let raw = LineEventBuffer {
            timestamp_ns: 0,
            id: 99,
            offset: 0,
            seqno: 0,
            line_seqno: 0,
            padding: [0; 6],
        };

        assert_eq!(
            parse_event(&raw).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
