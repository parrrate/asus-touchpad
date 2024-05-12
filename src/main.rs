use std::{
    fs::File,
    io::{BufRead, BufReader},
    ops::Mul,
    os::fd::AsRawFd,
    path::Path,
    time::Duration,
};

use async_io::{Async, Timer};
use async_signal::{Signal, Signals};
use evdev::{
    uinput::{VirtualDevice, VirtualDeviceBuilder},
    AbsoluteAxisType, AttributeSet, Device, EventType, InputEvent, InputEventKind, Key,
    Synchronization,
};
use futures_lite::{FutureExt, StreamExt};
use i2cdev::{
    core::I2CTransfer,
    linux::{I2CMessage, LinuxI2CDevice},
};
use nix::fcntl::{fcntl, FcntlArg, OFlag};

#[derive(PartialEq, PartialOrd, Default)]
struct Percent(i32);

impl Percent {
    fn div(what: i32, by: i32) -> Self {
        Self((100 * what).checked_div(by).unwrap_or_default())
    }
}

impl Mul<i32> for Percent {
    type Output = i32;

    fn mul(self, rhs: i32) -> Self::Output {
        rhs * self.0 / 100
    }
}

const TRY_TIMES: usize = 5;
const TRY_SLEEP: Duration = Duration::from_millis(100);
const COLS: usize = 5;
const ROWS: usize = 4;
const LEFT_OFFSET: Percent = Percent(7);
const RIGHT_OFFSET: Percent = Percent(7);
const TOP_OFFSET: Percent = Percent(10);
const BOTTOM_OFFSET: Percent = Percent(4);
const KEYS: [[Key; COLS]; ROWS] = [
    [
        Key::KEY_KP7,
        Key::KEY_KP8,
        Key::KEY_KP9,
        Key::KEY_KPSLASH,
        Key::KEY_BACKSPACE,
    ],
    [
        Key::KEY_KP4,
        Key::KEY_KP5,
        Key::KEY_KP6,
        Key::KEY_KPASTERISK,
        Key::KEY_BACKSPACE,
    ],
    [
        Key::KEY_KP1,
        Key::KEY_KP2,
        Key::KEY_KP3,
        Key::KEY_KPMINUS,
        Key::KEY_5,
    ],
    [
        Key::KEY_KP0,
        Key::KEY_KPDOT,
        Key::KEY_KPENTER,
        Key::KEY_KPPLUS,
        Key::KEY_KPEQUAL,
    ],
];

enum Touchpad {
    No,
    Yes,
    Some(String),
}

fn main() -> std::io::Result<()> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();
    async_io::block_on(run_outer())?;
    Ok(())
}

async fn run_outer() -> std::io::Result<()> {
    let mut signals = Signals::new([Signal::Term, Signal::Quit, Signal::Int])?;
    run_retry()
        .race(async {
            signals.try_next().await?;
            Ok(())
        })
        .await?;
    Ok(())
}

async fn run_retry() -> std::io::Result<()> {
    loop {
        if let Err(e) = run().await {
            log::error!("{e}")
        }
        Timer::after(TRY_SLEEP).await;
    }
}

async fn run() -> std::io::Result<()> {
    let mut tries = TRY_TIMES;
    let (touchpad, device_id) = loop {
        let mut touchpad = Touchpad::No;
        let mut device_id: Option<String> = None;
        let f = BufReader::new(File::open("/proc/bus/input/devices")?);
        for line in f.lines() {
            let line = line?;
            loop {
                match &touchpad {
                    Touchpad::No => {
                        if line.contains("Touchpad")
                            && (line.contains(r#"Name="ASUE"#) || line.contains(r#"Name="ELAN"#))
                        {
                            touchpad = Touchpad::Yes;
                        } else {
                            break;
                        }
                    }
                    Touchpad::Yes => {
                        if line.contains("S: ") {
                            device_id = Some(
                                regex::Regex::new(".*i2c-(\\d+)/.*$")
                                    .unwrap()
                                    .replace(&line, "$1")
                                    .to_string()
                                    .replace('\n', ""),
                            );
                        }
                        if line.contains("H: ") {
                            touchpad = Touchpad::Some(
                                line.split("event")
                                    .nth(1)
                                    .unwrap()
                                    .split(' ')
                                    .next()
                                    .unwrap()
                                    .to_string(),
                            );
                        } else {
                            break;
                        }
                    }
                    Touchpad::Some(_) => break,
                }
            }
        }
        match (touchpad, device_id) {
            (Touchpad::Some(touchpad), Some(device_id)) => break (touchpad, device_id),
            _ => log::error!("bwaaa"),
        }
        tries -= 1;
        if tries == 0 {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        Timer::after(TRY_SLEEP).await;
    };
    log::info!("touchpad {touchpad}");
    log::info!("device_id {device_id}");
    let touchpad = Device::open(Path::new("/dev/input").join(format!("event{touchpad}")))?;
    fcntl(touchpad.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
    let abs = touchpad.get_abs_state()?;
    let absx = abs[AbsoluteAxisType::ABS_X.0 as usize];
    let (minx, maxx) = (absx.minimum, absx.maximum);
    let absy = abs[AbsoluteAxisType::ABS_Y.0 as usize];
    let (miny, maxy) = (absy.minimum, absy.maximum);
    log::info!("x {minx}-{maxx}  y {miny}-{maxy}");
    let percentage_key = Key::KEY_5;
    let mut keys = AttributeSet::<Key>::new();
    keys.insert(Key::KEY_LEFTSHIFT);
    keys.insert(Key::KEY_NUMLOCK);
    keys.insert(Key::KEY_CALC);
    for key in KEYS.into_iter().flatten() {
        keys.insert(key);
    }
    if percentage_key != Key::KEY_5 {
        keys.insert(percentage_key);
    }
    let udev = VirtualDeviceBuilder::new()?
        .name("Asus Touchpad/Numpad")
        .with_keys(&keys)?
        .build()?;
    let device = unsafe {
        LinuxI2CDevice::force_new(Path::new("/dev").join(format!("i2c-{device_id}")), 0x15)
    }?;
    let touchpad = Async::new(touchpad)?;
    let mut context = Context {
        no_touch: NoTouch {
            device,
            udev,
            minx,
            maxx,
            miny,
            maxy,
            x: 0,
            y: 0,
            pressed: None,
            numlock: false,
        },
        touchpad,
    };
    context.run().await?;
    drop(context);
    log::info!("stopped");
    Ok(())
}

struct NoTouch {
    device: LinuxI2CDevice,
    udev: VirtualDevice,
    minx: i32,
    maxx: i32,
    miny: i32,
    maxy: i32,
    x: i32,
    y: i32,
    pressed: Option<Key>,
    numlock: bool,
}

impl Drop for NoTouch {
    fn drop(&mut self) {
        if let Err(e) = self.deactivate() {
            log::error!("{e}")
        }
    }
}

fn non_neg_sub(a: i32, b: i32) -> Option<i32> {
    let x = a.checked_sub(b)?;
    (x >= 0).then_some(x)
}

impl NoTouch {
    fn activate(&mut self) -> std::io::Result<()> {
        let mut msgs = [I2CMessage::write(&[
            0x05, 0x00, 0x3d, 0x03, 0x06, 0x00, 0x07, 0x00, 0x0d, 0x14, 0x03, 0x01, 0xad,
        ])];
        let t = self.device.transfer(&mut msgs)?;
        if t != 1 {
            log::error!("activate write failed");
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        self.udev
            .emit(&[InputEvent::new(EventType::KEY, Key::KEY_NUMLOCK.code(), 1)])?;
        Ok(())
    }

    fn deactivate(&mut self) -> std::io::Result<()> {
        self.udev
            .emit(&[InputEvent::new(EventType::KEY, Key::KEY_NUMLOCK.code(), 0)])?;
        let mut msgs = [I2CMessage::write(&[
            0x05, 0x00, 0x3d, 0x03, 0x06, 0x00, 0x07, 0x00, 0x0d, 0x14, 0x03, 0x00, 0xad,
        ])];
        let t = self.device.transfer(&mut msgs)?;
        if t != 1 {
            log::error!("deactivate write failed");
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        Ok(())
    }

    fn release(&mut self) -> std::io::Result<()> {
        if let Some(button) = self.pressed.take() {
            self.udev.emit(&[
                InputEvent::new(EventType::KEY, Key::KEY_LEFTSHIFT.code(), 0),
                InputEvent::new(EventType::KEY, button.code(), 0),
            ])?
        }
        Ok(())
    }

    fn try_calculator(&mut self) -> std::io::Result<()> {
        self.udev.emit(&[
            InputEvent::new(EventType::KEY, Key::KEY_CALC.code(), 1),
            InputEvent::new(EventType::SYNCHRONIZATION, Synchronization::SYN_REPORT.0, 0),
            InputEvent::new(EventType::KEY, Key::KEY_CALC.code(), 0),
        ])
    }

    fn calculator(&mut self) {
        if let Err(e) = self.try_calculator() {
            log::error!("{e}")
        }
    }

    fn width(&self) -> i32 {
        self.maxx - self.minx
    }

    fn height(&self) -> i32 {
        self.maxy - self.miny
    }

    fn left_percent(&self) -> Percent {
        Percent::div(self.x - self.minx, self.width())
    }

    fn right_percent(&self) -> Percent {
        Percent::div(self.maxx - self.x, self.width())
    }

    fn top_percent(&self) -> Percent {
        Percent::div(self.y - self.miny, self.height())
    }

    fn _bottom_percent(&self) -> Percent {
        Percent::div(self.maxy - self.y, self.height())
    }

    fn numlock_hit(&self) -> bool {
        self.right_percent() < Percent(5) && self.top_percent() < Percent(9)
    }

    fn calculator_hit(&self) -> bool {
        self.left_percent() < Percent(6) && self.top_percent() < Percent(7)
    }

    fn left_np(&self) -> i32 {
        self.minx + LEFT_OFFSET * self.width()
    }

    fn right_np(&self) -> i32 {
        self.maxx - RIGHT_OFFSET * self.width()
    }

    fn top_np(&self) -> i32 {
        self.miny + TOP_OFFSET * self.height()
    }

    fn bottom_np(&self) -> i32 {
        self.maxy - BOTTOM_OFFSET * self.height()
    }

    fn width_np(&self) -> i32 {
        self.right_np() - self.left_np()
    }

    fn height_np(&self) -> i32 {
        self.bottom_np() - self.top_np()
    }

    fn column_raw(&self) -> Option<usize> {
        (non_neg_sub(self.x, self.left_np())? * i32::try_from(COLS).ok()?)
            .checked_div(self.width_np() + 1)?
            .try_into()
            .ok()
    }

    fn row_raw(&self) -> Option<usize> {
        (non_neg_sub(self.y, self.top_np())? * i32::try_from(ROWS).ok()?)
            .checked_div(self.height_np() + 1)?
            .try_into()
            .ok()
    }

    fn column(&self, row: [Key; COLS]) -> Option<Key> {
        row.get(self.column_raw()?).copied()
    }

    fn row(&self) -> Option<[Key; COLS]> {
        KEYS.get(self.row_raw()?).copied()
    }

    fn key(&self) -> Option<Key> {
        self.column(self.row()?)
    }

    fn press(&mut self) -> std::io::Result<()> {
        if self.pressed.is_none() {
            if self.numlock_hit() {
                self.numlock = !self.numlock;
                if self.numlock {
                    self.activate()?;
                } else {
                    self.deactivate()?;
                }
            } else if self.calculator_hit() {
                self.calculator();
            } else if self.numlock {
                if let Some(key) = self.key() {
                    if key == Key::KEY_5 {
                        self.udev.emit(&[
                            InputEvent::new(EventType::KEY, Key::KEY_LEFTSHIFT.code(), 1),
                            InputEvent::new(EventType::KEY, Key::KEY_5.code(), 1),
                        ])?
                    } else {
                        self.udev
                            .emit(&[InputEvent::new(EventType::KEY, key.code(), 1)])?
                    }
                    self.pressed = Some(key);
                }
            }
        }
        Ok(())
    }

    fn with_touchpad(&mut self, touchpad: &mut Device) -> std::io::Result<()> {
        for e in touchpad.fetch_events()? {
            match e.kind() {
                InputEventKind::Key(Key::BTN_TOOL_FINGER) => match e.value() {
                    0 => self.release()?,
                    1 => self.press()?,
                    _ => {}
                },
                InputEventKind::AbsAxis(AbsoluteAxisType::ABS_MT_POSITION_X) => self.x = e.value(),
                InputEventKind::AbsAxis(AbsoluteAxisType::ABS_MT_POSITION_Y) => self.y = e.value(),
                _ => {}
            }
        }
        Ok(())
    }
}

struct Context {
    no_touch: NoTouch,
    touchpad: Async<Device>,
}

impl Drop for Context {
    fn drop(&mut self) {
        if let Err(e) = self.ungrab() {
            log::error!("{e}")
        }
    }
}

impl Context {
    fn grab(&mut self) -> std::io::Result<()> {
        self.touchpad.as_mut().grab()?;
        Ok(())
    }

    fn ungrab(&mut self) -> std::io::Result<()> {
        self.touchpad.as_mut().ungrab()?;
        Ok(())
    }

    async fn step(&mut self) -> std::io::Result<()> {
        self.touchpad
            .read_with_mut(|touchpad| self.no_touch.with_touchpad(touchpad))
            .await?;
        if self.no_touch.numlock {
            self.grab()?
        } else {
            self.ungrab()?
        }
        Ok(())
    }

    async fn run(&mut self) -> std::io::Result<()> {
        loop {
            self.step().await?
        }
    }
}
