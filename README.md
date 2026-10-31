# rust-periphery

A Rust library for peripheral I/O (GPIO, PWM, SPI, I2C) in Linux.

The crate talks to the kernel interfaces directly through `std::fs` and
`libc::ioctl`, so it has no C dependencies and needs no cross-compilation
setup.

## Supported interfaces

| Bus  | Kernel interface                      | Kernel option         |
|------|---------------------------------------|-----------------------|
| GPIO | `/dev/gpiochip<N>` (recommended)      | `CONFIG_GPIO_CDEV`    |
| GPIO | `/sys/class/gpio` (deprecated)        | `CONFIG_GPIO_SYSFS`   |
| PWM  | `/sys/class/pwm` (sysfs)              | `CONFIG_PWM_SYSFS`    |
| SPI  | `/dev/spidev<bus>.<cs>` (`spidev`)    | `CONFIG_SPI_SPIDEV`   |
| I2C  | `/dev/i2c-<bus>` (`i2c-dev`)          | `CONFIG_I2C_CHARDEV`  |

MMIO is not implemented.

## GPIO: which interface to use

Recent kernels ship with `CONFIG_GPIO_SYSFS` disabled by default, so
`/sys/class/gpio` is often missing entirely. Use the `gpiochip` character
device instead — it is available on every current kernel:

| | `periphery::sys::gpiochip` | `periphery::sys::gpio` |
|---|---|---|
| Kernel interface | `/dev/gpiochip<N>` (v2 ABI) | `/sys/class/gpio` |
| Kernel option | `CONFIG_GPIO_CDEV` (default on) | `CONFIG_GPIO_SYSFS` (default off) |
| Claiming lines | atomic, exclusive request | `export` / `unexport` |
| Several lines at once | yes, one atomic request | one pin at a time |
| Edge interrupts | blocking `wait_event`, kernel timestamps | `poll` on `value` |
| Initial output level | applied atomically at request time | separate write afterwards |

Only reach for `sys::gpio` when you have to support an old kernel or a system
where someone enabled GPIO sysfs.

## Installation

```toml
[dependencies]
periphery = "0.2"
```

## Permissions

Unless you run as `root`, your user must be able to access the device nodes
and sysfs attributes:

```sh
sudo usermod -aG gpio,i2c,spi,pwm "$USER"   # log out and back in afterwards
```

## Usage

### GPIO (`gpiochip` character device — recommended)

```rust,no_run
use periphery::sys::gpiochip::{Chip, LineSettings, Value};

fn main() -> std::io::Result<()> {
    let chip = Chip::new(0)?;
    println!("{} ({}) with {} lines", chip.name(), chip.label(), chip.lines());

    let line = chip.request_lines(
        &[17],
        "my-app",
        &LineSettings::new().output().initial_value(Value::Low),
    )?;

    line.set_value(0, Value::High)?;
    println!("level = {:?}", line.get_value(0)?);

    Ok(()) // dropping `line` releases the claim
}
```

Several lines can be claimed in one atomic request and driven together:

```rust,no_run
use periphery::sys::gpiochip::{Chip, LineSettings};

fn main() -> std::io::Result<()> {
    let chip = Chip::new(0)?;
    let line = chip.request_lines(&[17, 27, 22], "my-app", &LineSettings::new().output())?;

    line.set_all(0b101)?;        // line 0 and 2 high, line 1 low
    println!("{:#05b}", line.get_all()?);
    Ok(())
}
```

Waiting for edges:

```rust,no_run
use periphery::sys::gpiochip::{Chip, Edge, EventKind, LineSettings};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let chip = Chip::new(0)?;
    let line = chip.request_lines(
        &[17],
        "my-app",
        &LineSettings::new().input().edge(Edge::Both).debounce_us(5_000),
    )?;

    while let Some(event) = line.wait_event(Some(Duration::from_secs(1)))? {
        match event.kind() {
            EventKind::Rising => println!("rising at {} ns", event.timestamp_ns()),
            EventKind::Falling => println!("falling at {} ns", event.timestamp_ns()),
        }
    }

    Ok(())
}
```

`wait_event` returns `Ok(None)` on timeout and blocks until an edge when passed
`None`. Unlike the sysfs backend no acknowledgement read is needed: the kernel
queues events and they are consumed from the descriptor.

### GPIO (legacy sysfs)

Only needed on old kernels, or where `CONFIG_GPIO_SYSFS` has been enabled
explicitly.

```rust,no_run
use periphery::sys::gpio::{Direction, Pin, Value};

fn main() -> std::io::Result<()> {
    let pin = Pin::new(17);

    pin.export()?;
    pin.set_direction(Direction::Out)?;
    pin.set_value(Value::High)?;
    println!("level = {:?}", pin.value()?);

    pin.unexport()?;
    Ok(())
}
```

`Pin::export()` / `Pin::unexport()` are explicit: nothing is unexported when
the `Pin` is dropped. The same applies to `Pwm`.

Waiting for an edge interrupt on the sysfs backend:

```rust,no_run
use periphery::sys::gpio::{Edge, Pin};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let pin = Pin::new(27);
    pin.export()?;
    pin.set_edge(Edge::Rising)?;

    if pin.poll(Some(Duration::from_secs(1)))? {
        println!("rising edge detected");
    }

    pin.unexport()?;
    Ok(())
}
```

`poll` acknowledges the interrupt before returning, so consecutive calls wait
for fresh edges.

### PWM

```rust,no_run
use periphery::sys::pwm::Pwm;

fn main() -> std::io::Result<()> {
    let pwm = Pwm::new(0, 0)?; // chip 0, channel 0

    pwm.export()?;
    pwm.set_period_ns(1_000_000)?;  // 1 kHz
    pwm.set_duty_cycle_ns(250_000)?; // 25 %
    pwm.enable(true)?;

    pwm.enable(false)?;
    pwm.unexport()?;
    Ok(())
}
```

### I2C

```rust,no_run
use periphery::sys::i2c::I2C;

fn main() -> std::io::Result<()> {
    let mut i2c = I2C::new(1)?;
    i2c.set_slave_address(0x68)?;

    let mut who_am_i = [0u8; 1];
    i2c.write_read(&[0x75], &mut who_am_i)?;
    println!("WHO_AM_I = {:#04x}", who_am_i[0]);

    Ok(())
}
```

### SPI

```rust,no_run
use periphery::sys::spi::{Mode, SpidevTransfer, SPI};

fn main() -> std::io::Result<()> {
    let spi = SPI::new(0, 0, 1_000_000, Mode::Mode0)?;

    // Full duplex transfer: send 0x9F and read one byte back.
    let mut read = [0u8; 2];
    let mut transfer = SpidevTransfer::read_write(&[0x9F, 0x00], &mut read);
    spi.transfer(&mut transfer)?;

    println!("{:02x?}", read);
    Ok(())
}
```

## Thread safety

`I2C`, `SPI`, `Pwm`, `Chip` and `Line` are `Send`: they can be moved into a thread. They are
not `Sync`, so a single handle cannot be shared between threads — give each
thread its own handle, or wrap one in `Mutex`.

## Notes

* Use `sys::gpiochip` for GPIO. The sysfs backend in `sys::gpio` needs
  `CONFIG_GPIO_SYSFS`, which recent kernels disable by default.
* Edge detection over the `gpiochip` interface depends on interrupt support in
  the controller driver; some platform pins (for example several Intel PCH
  lines that belong to ACPI) reject it with `EINVAL`.
* `I2C::set_slave_address` uses `I2C_SLAVE`, so it fails with `EBUSY` if a
  kernel driver already owns the address.
* `I2C::clock_speed` only works on platforms that expose the device-tree
  `clock-frequency` attribute.

## MSRV

Rust 1.85 (edition 2024).

## License

MIT, see [LICENSE](LICENSE).
