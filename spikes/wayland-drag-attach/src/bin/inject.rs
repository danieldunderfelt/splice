use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode, UinputAbsSetup};

const BTN_LEFT: u16 = 0x110;
const BTN_TASK: u16 = 0x117;
const KEY_ESC: u16 = 1;
const KEY_LEFTMETA: u16 = 125;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_UP: u16 = 103;

fn stamp(x: i32, y: i32) {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    println!("{ms} {x} {y}");
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let screen_w: i32 = args.next().context("screen width")?.parse()?;
    let screen_h: i32 = args.next().context("screen height")?.parse()?;
    let mut buttons = AttributeSet::<KeyCode>::new();
    for code in BTN_LEFT..=BTN_TASK {
        buttons.insert(KeyCode::new(code));
    }
    let axis = AbsInfo::new(0, 0, 65535, 0, 0, 0);
    let mut pointer = VirtualDevice::builder()?
        .name("Splice Spike Pointer")
        .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5350, 0x0099, 1))
        .with_keys(&buttons)?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, axis))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, axis))?
        .build()?;
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in [KEY_ESC, KEY_LEFTMETA, KEY_LEFT, KEY_RIGHT, KEY_UP] {
        keys.insert(KeyCode::new(code));
    }
    let mut keyboard = VirtualDevice::builder()?
        .name("Splice Spike Keyboard")
        .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5350, 0x009a, 1))
        .with_keys(&keys)?
        .build()?;
    std::thread::sleep(Duration::from_millis(1500));
    let scale = |v: i32, extent: i32| ((f64::from(v) + 0.5) * 65536.0 / f64::from(extent)).floor().clamp(0.0, 65535.0) as i32;
    let mut cur = (0, 0);
    while let Some(cmd) = args.next() {
        match cmd.as_str() {
            "move" => {
                let x: i32 = args.next().context("x")?.parse()?;
                let y: i32 = args.next().context("y")?.parse()?;
                cur = (x, y);
                stamp(x, y);
                pointer.emit(&[
                    InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, scale(x, screen_w)),
                    InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, scale(y, screen_h)),
                ])?;
            }
            "glide" => {
                let x: i32 = args.next().context("x")?.parse()?;
                let y: i32 = args.next().context("y")?.parse()?;
                let steps: i32 = args.next().context("steps")?.parse()?;
                let (sx, sy) = cur;
                for i in 1..=steps {
                    let t = f64::from(i) / f64::from(steps);
                    let p = (sx + ((x - sx) as f64 * t) as i32, sy + ((y - sy) as f64 * t) as i32);
                    pointer.emit(&[
                        InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, scale(p.0, screen_w)),
                        InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, scale(p.1, screen_h)),
                    ])?;
                    std::thread::sleep(Duration::from_millis(15));
                }
                cur = (x, y);
            }
            "sweep" => {
                let step: i32 = args.next().context("step")?.parse()?;
                let mut y = step / 2;
                while y < screen_h {
                    let mut x = step / 2;
                    while x < screen_w {
                        stamp(x, y);
                        pointer.emit(&[
                            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, scale(x, screen_w)),
                            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, scale(y, screen_h)),
                        ])?;
                        std::thread::sleep(Duration::from_millis(25));
                        x += step;
                    }
                    y += step;
                }
            }
            "press" => pointer.emit(&[InputEvent::new(EventType::KEY.0, BTN_LEFT, 1)])?,
            "release" => pointer.emit(&[InputEvent::new(EventType::KEY.0, BTN_LEFT, 0)])?,
            "click" => {
                pointer.emit(&[InputEvent::new(EventType::KEY.0, BTN_LEFT, 1)])?;
                std::thread::sleep(Duration::from_millis(80));
                pointer.emit(&[InputEvent::new(EventType::KEY.0, BTN_LEFT, 0)])?;
            }
            "key" => {
                let name = args.next().context("key name")?;
                let codes: Vec<u16> = match name.as_str() {
                    "esc" => vec![KEY_ESC],
                    "super+left" => vec![KEY_LEFTMETA, KEY_LEFT],
                    "super+right" => vec![KEY_LEFTMETA, KEY_RIGHT],
                    "super+up" => vec![KEY_LEFTMETA, KEY_UP],
                    other => return Err(anyhow!("unknown key {other}")),
                };
                for code in &codes {
                    keyboard.emit(&[InputEvent::new(EventType::KEY.0, *code, 1)])?;
                    std::thread::sleep(Duration::from_millis(30));
                }
                for code in codes.iter().rev() {
                    keyboard.emit(&[InputEvent::new(EventType::KEY.0, *code, 0)])?;
                    std::thread::sleep(Duration::from_millis(30));
                }
            }
            "sleep" => {
                let ms: u64 = args.next().context("ms")?.parse()?;
                std::thread::sleep(Duration::from_millis(ms));
            }
            other => return Err(anyhow!("unknown command {other}")),
        }
    }
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}
