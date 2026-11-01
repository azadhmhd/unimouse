//! Linux backend: evdev in (grabbing the real mice), uinput out (a virtual mouse and keyboard).
//! It sits below the display server, so it works the same on X11 and Wayland.

use crate::{
    engine::{launch, parse_combo, practicing, Action, Combo, Engine, Mouse, Up, INPUT_ACTIVE},
    scroll::SCROLLER,
};
use evdev::{uinput::VirtualDevice, AttributeSet, Device, EventType, InputEvent, KeyCode, RelativeAxisCode as Rel};
use std::{
    process::{exit, Command},
    sync::{atomic::Ordering, Mutex},
    thread,
    time::Instant,
};

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
/// Grabbed mice are silent to the system, so everything we don't swallow is re-emitted here.
static MOUSE: Mutex<Option<VirtualDevice>> = Mutex::new(None);
static KEYBOARD: Mutex<Option<VirtualDevice>> = Mutex::new(None);
/// Hi-res wheel units not yet sent as a legacy whole-notch REL_WHEEL event: [horizontal, vertical].
static WHEEL_CARRY: Mutex<[i32; 2]> = Mutex::new([0; 2]);
/// The mice we grabbed, for the settings page.
static MICE: Mutex<Vec<Mouse>> = Mutex::new(Vec::new());
const NAME: &str = "unimouse virtual";
/// One wheel notch in hi-res units (kernel convention).
const NOTCH: i32 = 120;

const BTN_LEFT: u16 = 0x110;
const BTN_TASK: u16 = 0x117;

pub fn run(engine: Engine) {
    *ENGINE.lock().unwrap() = Some(engine);
    let mice: Vec<Device> = evdev::enumerate().map(|(_, d)| d).filter(is_mouse).collect();
    if mice.is_empty() {
        eprintln!(
            "No mice found (or no permission to read them).\n\
                   Add yourself to the input group: sudo usermod -aG input $USER, then log out and back in."
        );
        exit(1);
    }
    let make = |d: std::io::Result<VirtualDevice>| {
        d.unwrap_or_else(|e| {
            eprintln!(
                "Could not create a virtual device on /dev/uinput: {e}\nSee the README for the one-line udev rule."
            );
            exit(1)
        })
    };
    *MOUSE.lock().unwrap() = Some(make(virtual_mouse()));
    *KEYBOARD.lock().unwrap() = Some(make(virtual_keyboard()));
    SCROLLER.spawn(post_scroll);
    *MICE.lock().unwrap() = mice
        .iter()
        .map(|d| Mouse {
            name: d.name().unwrap_or("Mouse").to_string(),
            vendor: d.input_id().vendor(),
            product: d.input_id().product(),
            transport: format!("{:?}", d.input_id().bus_type()),
        })
        .collect();
    // ponytail: mice are picked up at start only. Watch /dev/input with inotify if hot-plugging matters.
    let readers: Vec<_> = mice.into_iter().map(|d| thread::spawn(move || read_loop(d))).collect();
    INPUT_ACTIVE.store(true, Ordering::Relaxed);
    for r in readers {
        let _ = r.join();
    }
}

/// Relative pointer with a left button. Touchpads report absolute axes, so they're left alone.
fn is_mouse(d: &Device) -> bool {
    let rel = d.supported_relative_axes().is_some_and(|a| a.contains(Rel::REL_X) && a.contains(Rel::REL_Y));
    let left = d.supported_keys().is_some_and(|k| k.contains(KeyCode(BTN_LEFT)));
    rel && left && !d.name().unwrap_or("").starts_with(NAME)
}

fn virtual_mouse() -> std::io::Result<VirtualDevice> {
    let keys: AttributeSet<KeyCode> = (BTN_LEFT..=BTN_TASK).map(KeyCode).collect();
    let axes: AttributeSet<Rel> =
        [Rel::REL_X, Rel::REL_Y, Rel::REL_WHEEL, Rel::REL_HWHEEL, Rel::REL_WHEEL_HI_RES, Rel::REL_HWHEEL_HI_RES]
            .into_iter()
            .collect();
    VirtualDevice::builder()?.name(&format!("{NAME} mouse")).with_keys(&keys)?.with_relative_axes(&axes)?.build()
}

fn virtual_keyboard() -> std::io::Result<VirtualDevice> {
    let keys: AttributeSet<KeyCode> = (1..=248).map(KeyCode).collect();
    VirtualDevice::builder()?.name(&format!("{NAME} keyboard")).with_keys(&keys)?.build()
}

fn read_loop(mut dev: Device) {
    let name = dev.name().unwrap_or("mouse").to_string();
    if let Err(e) = dev.grab() {
        eprintln!("could not grab {name}: {e}");
        return;
    }
    println!("using {name}");
    let mut pos = (0.0, 0.0); // evdev is relative-only; a virtual position is enough for gesture distances
    let mut batch = Vec::new();
    loop {
        let events: Vec<InputEvent> = match dev.fetch_events() {
            Ok(events) => events.collect(),
            Err(e) => {
                eprintln!("{name}: {e}");
                return;
            }
        };
        for ev in events {
            if ev.event_type() != EventType::SYNCHRONIZATION {
                batch.push(ev);
                continue;
            }
            filter(&mut batch, &mut pos);
            if !batch.is_empty() {
                emit(&MOUSE, &batch)
            }
            batch.clear();
        }
    }
}

/// Feeds one SYN-terminated batch to the engine and removes what it swallows.
fn filter(batch: &mut Vec<InputEvent>, pos: &mut (f64, f64)) {
    let mut guard = ENGINE.lock().unwrap();
    let engine = guard.as_mut().unwrap();
    let rel = |code: Rel| {
        batch.iter().find(|e| e.event_type() == EventType::RELATIVE && e.code() == code.0).map(|e| e.value())
    };

    let (dx, dy) = (rel(Rel::REL_X).unwrap_or(0), rel(Rel::REL_Y).unwrap_or(0));
    if (dx, dy) != (0, 0) {
        *pos = (pos.0 + dx as f64, pos.1 + dy as f64);
        if let Some(a) = engine.on_move(*pos) {
            perform(&a)
        }
    }

    let (wheel, hwheel) = (rel(Rel::REL_WHEEL).unwrap_or(0), rel(Rel::REL_HWHEEL).unwrap_or(0));
    // Classic notches only: on free-spin/hi-res wheels the hi-res value isn't a whole notch, so pass those through.
    let whole_notches = rel(Rel::REL_WHEEL_HI_RES).is_none_or(|h| h == wheel * NOTCH)
        && rel(Rel::REL_HWHEEL_HI_RES).is_none_or(|h| h == hwheel * NOTCH);
    if (wheel, hwheel) != (0, 0) && whole_notches && scroll(engine, wheel, hwheel) {
        let wheel_codes = [Rel::REL_WHEEL.0, Rel::REL_HWHEEL.0, Rel::REL_WHEEL_HI_RES.0, Rel::REL_HWHEEL_HI_RES.0];
        batch.retain(|e| !(e.event_type() == EventType::RELATIVE && wheel_codes.contains(&e.code())));
    }

    let now = Instant::now();
    batch.retain(|e| {
        let Some(button) = (e.event_type() == EventType::KEY).then(|| button_number(e.code())).flatten() else {
            return true;
        };
        match e.value() {
            1 => !engine.on_down(button, *pos, active_app, now),
            0 => match engine.on_up(button, *pos, now) {
                Up::Pass => true,
                Up::Swallow(a) => {
                    if let Some(a) = a {
                        perform(&a)
                    }
                    false
                }
            },
            _ => true, // autorepeat
        }
    });
}

/// Returns true if the notch was taken.
fn scroll(engine: &mut Engine, wheel: i32, hwheel: i32) -> bool {
    // REL_WHEEL > 0 = wheel pushed away from you.
    if wheel != 0 {
        if let Some(a) = engine.on_held_scroll(wheel > 0) {
            perform(&a);
            return true;
        }
    }
    // ponytail: keyboards aren't read, so unlike macOS/Windows a held Ctrl doesn't bypass smoothing.
    let Some(s) = engine.scroll(active_app) else { return false };
    SCROLLER.notch(hwheel as f64, wheel as f64, NOTCH as f64 * s.factor(), s.smooth);
    true
}

/// Hi-res events for smooth-scrolling apps (GTK, Qt, browsers), plus legacy whole notches for the rest.
fn post_scroll(dx: i32, dy: i32) {
    let mut carry = WHEEL_CARRY.lock().unwrap();
    let mut events = Vec::new();
    for (i, (v, lo, hi)) in [(dx, Rel::REL_HWHEEL, Rel::REL_HWHEEL_HI_RES), (dy, Rel::REL_WHEEL, Rel::REL_WHEEL_HI_RES)]
        .into_iter()
        .enumerate()
    {
        if v == 0 {
            continue;
        }
        events.push(InputEvent::new(EventType::RELATIVE.0, hi.0, v));
        carry[i] += v;
        let whole = carry[i] / NOTCH;
        if whole != 0 {
            events.push(InputEvent::new(EventType::RELATIVE.0, lo.0, whole));
            carry[i] -= whole * NOTCH;
        }
    }
    emit(&MOUSE, &events);
}

fn button_number(code: u16) -> Option<u8> {
    match code {
        0x112 => Some(3),         // BTN_MIDDLE
        0x113 | 0x116 => Some(4), // BTN_SIDE, BTN_BACK
        0x114 | 0x115 => Some(5), // BTN_EXTRA, BTN_FORWARD
        0x117 => Some(6),         // BTN_TASK
        _ => None,
    }
}

fn button_code(n: u8) -> u16 {
    match n {
        3 => 0x112,
        4 => 0x113,
        5 => 0x114,
        _ => BTN_TASK,
    }
}

fn emit(dev: &Mutex<Option<VirtualDevice>>, events: &[InputEvent]) {
    if let Err(e) = dev.lock().unwrap().as_mut().unwrap().emit(events) {
        eprintln!("uinput: {e}")
    }
}

/// WM_CLASS of the focused window, e.g. "firefox", "Code", "Google-chrome".
fn active_app() -> String {
    // ponytail: X11 only (via xprop). Wayland has no standard way to ask, so app sections apply on X11 only.
    let xprop = |args: &[&str]| {
        Command::new("xprop").args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let Some(id) =
        xprop(&["-root", "_NET_ACTIVE_WINDOW"]).and_then(|s| s.split_whitespace().last().map(str::to_string))
    else {
        return String::new();
    };
    xprop(&["-id", &id, "WM_CLASS"])
        .and_then(|s| s.rsplit('"').nth(1).map(str::to_string)) // WM_CLASS(STRING) = "Navigator", "firefox"
        .unwrap_or_default()
}

pub fn mice() -> Vec<Mouse> {
    MICE.lock().unwrap().clone()
}

fn perform(action: &Action) {
    if practicing() {
        return;
    }
    // ponytail: desktop shortcuts are GNOME's defaults. Other desktops: rebind with keys: in the config.
    let combo = match action {
        Action::Keys(c) => return send_combo(c),
        Action::Click(n) => {
            let code = button_code(*n);
            emit(&MOUSE, &[InputEvent::new(EventType::KEY.0, code, 1)]);
            return emit(&MOUSE, &[InputEvent::new(EventType::KEY.0, code, 0)]);
        }
        Action::Nothing | Action::Native => return,
        Action::Open(_) | Action::Run(_) => return launch(action),
        Action::Back => "alt+left",
        Action::Forward => "alt+right",
        Action::MissionControl | Action::AppWindows => "super",
        Action::PrevDesktop => "super+pageup",
        Action::NextDesktop => "super+pagedown",
        Action::ShowDesktop => "super+d",
        Action::VolumeUp => "volume_up",
        Action::VolumeDown => "volume_down",
        Action::Mute => "mute",
        Action::PlayPause => "play_pause",
        Action::NextTrack => "next_track",
        Action::PrevTrack => "prev_track",
    };
    send_combo(&parse_combo(combo).unwrap());
}

fn send_combo(c: &Combo) {
    let mut keys = Vec::new();
    if c.mods.ctrl {
        keys.push(29)
    } // KEY_LEFTCTRL
    if c.mods.alt {
        keys.push(56)
    } // KEY_LEFTALT
    if c.mods.shift {
        keys.push(42)
    } // KEY_LEFTSHIFT
    if c.mods.meta {
        keys.push(125)
    } // KEY_LEFTMETA
    keys.push(key_code(&c.key).unwrap());
    let key = |code: u16, value| InputEvent::new(EventType::KEY.0, code, value);
    for &k in &keys {
        emit(&KEYBOARD, &[key(k, 1)])
    }
    for &k in keys.iter().rev() {
        emit(&KEYBOARD, &[key(k, 0)])
    }
}

/// Linux input key codes (input-event-codes.h).
pub fn key_code(name: &str) -> Option<u16> {
    Some(match name {
        "escape" => 1,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        "-" => 12,
        "=" => 13,
        "backspace" => 14,
        "tab" => 15,
        "q" => 16,
        "w" => 17,
        "e" => 18,
        "r" => 19,
        "t" => 20,
        "y" => 21,
        "u" => 22,
        "i" => 23,
        "o" => 24,
        "p" => 25,
        "[" => 26,
        "]" => 27,
        "return" => 28,
        "a" => 30,
        "s" => 31,
        "d" => 32,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        ";" => 39,
        "'" => 40,
        "`" => 41,
        "\\" => 43,
        "z" => 44,
        "x" => 45,
        "c" => 46,
        "v" => 47,
        "b" => 48,
        "n" => 49,
        "m" => 50,
        "," => 51,
        "." => 52,
        "/" => 53,
        "space" => 57,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        "home" => 102,
        "up" => 103,
        "pageup" => 104,
        "left" => 105,
        "right" => 106,
        "end" => 107,
        "down" => 108,
        "pagedown" => 109,
        "insert" => 110,
        "delete" => 111,
        "mute" => 113,
        "volume_down" => 114,
        "volume_up" => 115,
        "super" => 125,
        "browser_back" => 158,
        "browser_forward" => 159,
        "next_track" => 163,
        "play_pause" => 164,
        "prev_track" => 165,
        _ => return None,
    })
}
