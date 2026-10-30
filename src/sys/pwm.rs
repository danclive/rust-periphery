//! Linux PWM access through the sysfs interface (`/sys/class/pwm`).
//!
//! [`Pwm`] wraps a single channel of a [`PwmChip`]; the channel has to be
//! exported before its files become available.
//!
//! Note that sysfs PWM is only available when the kernel is built with
//! `CONFIG_PWM_SYSFS`.

use std::cell::Cell;
use std::fmt;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind::{InvalidData, Other};
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::str::FromStr;

/// A single PWM channel.
///
/// The underlying sysfs files are opened per call, so the handle owns no
/// resources and dropping it does not unexport the channel; call
/// [`Pwm::unexport`] explicitly.
///
/// The handle is `Send`, so it can be moved to another thread, but not `Sync`:
/// configuring a channel takes several consecutive sysfs writes (period, duty
/// cycle, enable) that must not be interleaved with another thread's writes.
pub struct Pwm {
    chip: PwmChip,
    number: u32,
    _not_sync: PhantomData<Cell<()>>,
}

/// A PWM controller, i.e. `/sys/class/pwm/pwmchip<number>`.
#[derive(Debug)]
pub struct PwmChip {
    /// The controller index as used in the sysfs path.
    pub number: u32,
}

/// The polarity of a PWM signal.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Polarity {
    /// Duty cycle is the active (high) time.
    Normal,
    /// Duty cycle is the inactive (low) time.
    Inverse,
}

impl Pwm {
    /// Creates a handle for channel `number` of `chip`.
    ///
    /// This only verifies that the chip exists; the channel is not exported.
    pub fn new(chip: u32, number: u32) -> io::Result<Pwm> {
        let chip: PwmChip = PwmChip::new(chip)?;

        Ok(Pwm {
            chip,
            number,
            _not_sync: PhantomData,
        })
    }

    /// Exports the channel, making it usable.
    pub fn export(&self) -> io::Result<()> {
        self.chip.export(self.number)
    }

    /// Unexports the channel.
    pub fn unexport(&self) -> io::Result<()> {
        self.chip.unexport(self.number)
    }

    /// Enables or disables the PWM signal.
    pub fn enable(&self, enable: bool) -> io::Result<()> {
        let mut enable_file = pwm_file_wo(&self.chip, self.number, "enable")?;

        let contents = if enable { "1" } else { "0" };
        enable_file.write_all(contents.as_bytes())?;

        Ok(())
    }

    /// Returns whether the PWM signal is currently enabled.
    pub fn enabled(&self) -> io::Result<bool> {
        match pwm_file_parse::<u32>(&self.chip, self.number, "enable")? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(io::Error::new(
                InvalidData,
                format!("Unexpected enable file contents: {:?}", other),
            )),
        }
    }

    /// Returns the currently configured duty cycle in nanoseconds.
    pub fn duty_cycle_ns(&self) -> io::Result<u32> {
        pwm_file_parse::<u32>(&self.chip, self.number, "duty_cycle")
    }

    /// Sets the active time of the PWM signal in nanoseconds.
    ///
    /// The value is passed to the kernel unchecked and must be lower than the
    /// period.
    pub fn set_duty_cycle_ns(&self, duty_cycle_ns: u32) -> io::Result<()> {
        // we'll just let the kernel do the validation
        let mut duty_cycle_file = pwm_file_wo(&self.chip, self.number, "duty_cycle")?;
        duty_cycle_file.write_all(format!("{}", duty_cycle_ns).as_bytes())?;
        Ok(())
    }

    /// Returns the currently configured period in nanoseconds.
    pub fn period_ns(&self) -> io::Result<u32> {
        pwm_file_parse::<u32>(&self.chip, self.number, "period")
    }

    /// Sets the period of the PWM signal in nanoseconds.
    pub fn set_period_ns(&self, period_ns: u32) -> io::Result<()> {
        let mut period_file = pwm_file_wo(&self.chip, self.number, "period")?;
        period_file.write_all(format!("{}", period_ns).as_bytes())?;
        Ok(())
    }

    /// Returns the current polarity.
    pub fn polarity(&self) -> io::Result<Polarity> {
        match pwm_file_parse::<u32>(&self.chip, self.number, "polarity")? {
            0 => Ok(Polarity::Normal),
            1 => Ok(Polarity::Inverse),
            other => Err(io::Error::new(
                InvalidData,
                format!("Unexpected polarity file contents: {:?}", other),
            )),
        }
    }

    /// Sets the polarity.
    ///
    /// Most controllers only accept a change while the channel is disabled.
    pub fn set_polarity(&self, polarity: Polarity) -> io::Result<()> {
        let mut polarity_file = pwm_file_wo(&self.chip, self.number, "polarity")?;

        let contents = match polarity {
            Polarity::Normal => "normal",
            Polarity::Inverse => "inversed",
        };
        polarity_file.write_all(contents.as_bytes())?;

        Ok(())
    }
}

impl fmt::Debug for Pwm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pwm")
            .field("chip", &self.chip)
            .field("number", &self.number)
            .finish()
    }
}

impl PwmChip {
    /// Opens the controller at `/sys/class/pwm/pwmchip<number>`.
    pub fn new(number: u32) -> io::Result<PwmChip> {
        fs::metadata(format!("/sys/class/pwm/pwmchip{}", number))?;
        Ok(PwmChip { number })
    }

    /// Returns the number of channels exposed by this controller.
    pub fn count(&self) -> io::Result<u32> {
        let npwm_path = format!("/sys/class/pwm/pwmchip{}/npwm", self.number);

        let mut s = String::new();
        File::open(&npwm_path)?.read_to_string(&mut s)?;

        match s.parse::<u32>() {
            Ok(n) => Ok(n),
            Err(_) => Err(io::Error::new(
                Other,
                format!("Unexpected npwm contents: {:?}", s),
            )),
        }
    }

    /// Exports a channel, unless it is already exported.
    pub fn export(&self, number: u32) -> io::Result<()> {
        // only export if not already exported
        if fs::metadata(format!(
            "/sys/class/pwm/pwmchip{}/pwm{}",
            self.number, number
        ))
        .is_err()
        {
            let path = format!("/sys/class/pwm/pwmchip{}/export", self.number);
            File::create(&path)?.write_all(format!("{}", number).as_bytes())?;
        }
        Ok(())
    }

    /// Unexports a channel, unless it is already unexported.
    pub fn unexport(&self, number: u32) -> io::Result<()> {
        if fs::metadata(format!(
            "/sys/class/pwm/pwmchip{}/pwm{}",
            self.number, number
        ))
        .is_ok()
        {
            let path = format!("/sys/class/pwm/pwmchip{}/unexport", self.number);
            File::create(&path)?.write_all(format!("{}", number).as_bytes())?;
        }
        Ok(())
    }
}

/// Open the specified entry name as a writable file
fn pwm_file_wo(chip: &PwmChip, pin: u32, name: &str) -> io::Result<File> {
    let f = OpenOptions::new().write(true).open(format!(
        "/sys/class/pwm/pwmchip{}/pwm{}/{}",
        chip.number, pin, name
    ))?;
    Ok(f)
}

/// Open the specified entry name as a readable file
fn pwm_file_ro(chip: &PwmChip, pin: u32, name: &str) -> io::Result<File> {
    let f = File::open(format!(
        "/sys/class/pwm/pwmchip{}/pwm{}/{}",
        chip.number, pin, name
    ))?;
    Ok(f)
}

/// Get the value from the given entry
fn pwm_file_parse<T: FromStr>(chip: &PwmChip, pin: u32, name: &str) -> io::Result<T> {
    let mut s = String::with_capacity(10);
    let mut f = pwm_file_ro(chip, pin, name)?;
    f.read_to_string(&mut s)?;

    match s.trim().parse::<T>() {
        Ok(r) => Ok(r),
        Err(_) => Err(io::Error::new(
            Other,
            format!("Unexpected value file contents: {:?}", s),
        )),
    }
}
