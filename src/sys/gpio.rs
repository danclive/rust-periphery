//! Linux GPIO access through the legacy sysfs interface (`/sys/class/gpio`).
//!
//! [`Pin`] wraps a single GPIO line. Access normally requires `root` or
//! membership in the `gpio` group, and the pin has to be [`Pin::export`]ed
//! before it can be configured.
//!
//! Note that this interface is deprecated by the kernel in favour of the
//! `gpiochip` character device, and is only available when the kernel is
//! built with `CONFIG_GPIO_SYSFS`.

use std::fs;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind::{InvalidData, Other};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Duration;

/// Direction of a GPIO line, as written to the `direction` file.
#[derive(Debug, Copy, PartialEq, Eq, Clone, PartialOrd, Ord)]
pub enum Direction {
    /// Input.
    In,
    /// Output, initial level unchanged.
    Out,
    /// Output, driven low.
    Low,
    /// Output, driven high.
    High,
}

/// Logic level of a GPIO line.
#[derive(Debug, Copy, PartialEq, Eq, Clone, PartialOrd, Ord)]
#[repr(u8)]
pub enum Value {
    /// Low level (`0`).
    Low = 0,
    /// High level (`1`).
    High = 1,
}

/// Edge that triggers an interrupt on the `value` file.
#[derive(Debug, Copy, PartialEq, Eq, Clone, PartialOrd, Ord)]
pub enum Edge {
    /// No interrupt.
    None,
    /// Rising edge.
    Rising,
    /// Falling edge.
    Falling,
    /// Both rising and falling edges.
    Both,
}

/// A single GPIO line, addressed by its global number.
#[derive(Debug, Copy, PartialEq, Eq, Clone, PartialOrd, Ord)]
pub struct Pin {
    /// The global GPIO number, used as `/sys/class/gpio/gpio<num>`.
    pub num: usize,
}

impl Pin {
    /// Creates a handle for gpio `num`.
    pub fn new(num: usize) -> Pin {
        Pin { num }
    }

    /// Creates a handle from a path such as `/sys/class/gpio/gpio17`.
    pub fn from_path<T: AsRef<Path>>(path: T) -> io::Result<Pin> {
        let pb = fs::canonicalize(path.as_ref())?;

        if !fs::metadata(&pb)?.is_dir() {
            return Err(io::Error::new(
                Other,
                "Provided path not a directory or symlink to a directory".to_owned(),
            ));
        }
        let num = Pin::extract_pin_from_path(&pb)?;
        Ok(Pin::new(num))
    }

    fn extract_pin_from_path<P: AsRef<Path>>(path: P) -> io::Result<usize> {
        path.as_ref()
            .file_name()
            .and_then(|filename| filename.to_str())
            .and_then(|filename_str| {
                filename_str
                    .trim_start_matches("gpio")
                    .parse::<usize>()
                    .ok()
            })
            .ok_or(io::Error::new(InvalidData, format!("{:?}", path.as_ref())))
    }

    fn write_sys_file(&self, file_name: &str, value: &str) -> io::Result<()> {
        let path = format!("/sys/class/gpio/gpio{}/{}", self.num, file_name);

        let mut file = OpenOptions::new().write(true).open(&path)?;
        file.write_all(value.as_bytes())?;

        Ok(())
    }

    fn read_sys_file(&self, file_name: &str) -> io::Result<String> {
        let path = format!("/sys/class/gpio/gpio{}/{}", self.num, file_name);

        let mut file = File::open(&path)?;
        let mut s = String::new();
        file.read_to_string(&mut s)?;

        Ok(s)
    }

    /// Returns whether the pin is currently exported.
    ///
    /// Any inspection failure (including a permission error) is reported as
    /// "not exported".
    pub fn is_exported(&self) -> bool {
        fs::metadata(format!("/sys/class/gpio/gpio{}", self.num)).is_ok()
    }

    /// Exports the pin so that its files become available.
    pub fn export(&self) -> io::Result<()> {
        if !self.is_exported() {
            let mut file = OpenOptions::new()
                .write(true)
                .open("/sys/class/gpio/export")?;
            file.write_all(format!("{}", self.num).as_bytes())?;
        }

        Ok(())
    }

    /// Unexports the pin.
    pub fn unexport(&self) -> io::Result<()> {
        if self.is_exported() {
            let mut file = OpenOptions::new()
                .write(true)
                .open("/sys/class/gpio/unexport")?;
            file.write_all(format!("{}", self.num).as_bytes())?;
        }

        Ok(())
    }

    /// Returns the configured direction.
    pub fn direction(&self) -> io::Result<Direction> {
        match self.read_sys_file("direction") {
            Ok(s) => match s.trim() {
                "in" => Ok(Direction::In),
                "out" => Ok(Direction::Out),
                "high" => Ok(Direction::High),
                "low" => Ok(Direction::Low),
                other => Err(io::Error::new(
                    Other,
                    format!("direction file contents {}", other),
                )),
            },
            Err(e) => Err(e),
        }
    }

    /// Sets the pin direction.
    pub fn set_direction(&self, dir: Direction) -> io::Result<()> {
        self.write_sys_file(
            "direction",
            match dir {
                Direction::In => "in",
                Direction::Out => "out",
                Direction::High => "high",
                Direction::Low => "low",
            },
        )?;

        Ok(())
    }

    /// Returns the logic level of the pin.
    pub fn value(&self) -> io::Result<Value> {
        match self.read_sys_file("value") {
            Ok(s) => match s.trim() {
                "1" => Ok(Value::High),
                "0" => Ok(Value::Low),
                other => Err(io::Error::new(
                    Other,
                    format!("value file contents {}", other),
                )),
            },
            Err(e) => Err(e),
        }
    }

    /// Drives the pin to `value`.
    pub fn set_value(&self, value: Value) -> io::Result<()> {
        self.write_sys_file(
            "value",
            match value {
                Value::Low => "0",
                Value::High => "1",
            },
        )?;

        Ok(())
    }

    /// Returns the configured interrupt edge.
    pub fn edge(&self) -> io::Result<Edge> {
        match self.read_sys_file("edge") {
            Ok(s) => match s.trim() {
                "none" => Ok(Edge::None),
                "rising" => Ok(Edge::Rising),
                "falling" => Ok(Edge::Falling),
                "both" => Ok(Edge::Both),
                other => Err(io::Error::new(
                    Other,
                    format!("Unexpected file contents {}", other),
                )),
            },
            Err(e) => Err(e),
        }
    }

    /// Sets the interrupt edge.
    ///
    /// Use [`Pin::poll`] to wait for the configured edge.
    pub fn set_edge(&self, edge: Edge) -> io::Result<()> {
        self.write_sys_file(
            "edge",
            match edge {
                Edge::None => "none",
                Edge::Rising => "rising",
                Edge::Falling => "falling",
                Edge::Both => "both",
            },
        )?;

        Ok(())
    }

    /// Waits until the pin reports an edge interrupt, or `timeout` elapses.
    ///
    /// Returns `Ok(true)` when an edge arrived and `Ok(false)` on timeout; a
    /// `timeout` of `None` blocks indefinitely.
    ///
    /// The pin has to be exported and configured for interrupts with
    /// [`Pin::set_edge`] beforehand. sysfs reports the current level as an
    /// initial event when the value file is opened, so the very first call
    /// after configuring the edge may return immediately.
    pub fn poll(&self, timeout: Option<Duration>) -> io::Result<bool> {
        let mut file = File::open(format!("/sys/class/gpio/gpio{}/value", self.num))?;

        let mut fds = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLPRI | libc::POLLERR,
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
            return Ok(false);
        }

        // Rewinding and reading the value file is what acknowledges the
        // interrupt, so that the next call waits for a new edge.
        file.seek(SeekFrom::Start(0))?;
        let mut value = String::with_capacity(4);
        file.read_to_string(&mut value)?;

        Ok(true)
    }

    /// Returns whether the pin is treated as active-low.
    pub fn active_low(&self) -> io::Result<bool> {
        match self.read_sys_file("active_low") {
            Ok(s) => match s.trim() {
                "1" => Ok(true),
                "0" => Ok(false),
                other => Err(io::Error::new(
                    Other,
                    format!("active_low file contents {}", other),
                )),
            },
            Err(e) => Err(e),
        }
    }

    /// Sets whether the pin is treated as active-low.
    pub fn set_active_low(&self, active_low: bool) -> io::Result<()> {
        self.write_sys_file(
            "active_low",
            match active_low {
                true => "1",
                false => "0",
            },
        )?;

        Ok(())
    }
}
