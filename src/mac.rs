//! macOS backend: CGEventTap in, CGEventPost out.

use crate::{
    engine::{launch, parse_combo, practicing, Action, Combo, Engine, Mouse, Up, INPUT_ACTIVE},
    scroll::SCROLLER,
};
use core_foundation::{
    base::TCFType,
    boolean::CFBoolean,
    dictionary::{CFDictionary, CFDictionaryRef},
    mach_port::CFMachPortRef,
    runloop::{kCFRunLoopCommonModes, CFRunLoop},
    string::CFString,
};
use core_graphics::{
    event::{
        CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy,
        CGEventType, CGMouseButton, CallbackResult, EventField, ScrollEventUnit,
    },
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use foreign_types::ForeignType;
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType, NSWorkspace};
use objc2_foundation::NSPoint;
use std::{
    ffi::c_void,
    ptr::null_mut,
    sync::{
        atomic::{AtomicPtr, Ordering},
        Mutex,
    },
    time::Instant,
};

/// Tags the events we post so our own tap lets them through.
const MAGIC: i64 = 0x756E_696D;
static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
static TAP: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
/// Distance of one wheel notch at speed 1.0.
const PIXELS_PER_NOTCH: f64 = 50.0;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    fn AXIsProcessTrusted() -> bool;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

pub fn run(engine: Engine) {
    let prompt =
        CFDictionary::from_CFType_pairs(&[(CFString::new("AXTrustedCheckOptionPrompt"), CFBoolean::true_value())]);
    if !unsafe { AXIsProcessTrustedWithOptions(prompt.as_concrete_TypeRef()) } {
        eprintln!(
            "unimouse needs Accessibility access.\n\
                   System Settings > Privacy & Security > Accessibility: turn on the app running unimouse\n\
                   (your terminal while developing). Waiting..."
        );
        while !unsafe { AXIsProcessTrusted() } {
            std::thread::sleep(std::time::Duration::from_secs(2))
        }
        eprintln!("Accessibility access granted.");
    }
    *ENGINE.lock().unwrap() = Some(engine);
    SCROLLER.spawn(post_scroll);
    let events = vec![
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::OtherMouseDragged,
        CGEventType::MouseMoved,
        CGEventType::ScrollWheel,
    ];
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        events,
        callback,
    )
    .expect("could not create event tap");
    TAP.store(tap.mach_port().as_concrete_TypeRef() as *mut c_void, Ordering::Relaxed);
    let source = tap.mach_port().create_runloop_source(0).expect("run loop source");
    unsafe { CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes) };
    tap.enable();
    INPUT_ACTIVE.store(true, Ordering::Relaxed);
    CFRunLoop::run_current();
}

fn callback(_: CGEventTapProxy, ty: CGEventType, ev: &CGEvent) -> CallbackResult {
    // macOS disables taps that are slow or when secure input kicks in; turn ours straight back on.
    if matches!(ty, CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput) {
        unsafe { CGEventTapEnable(TAP.load(Ordering::Relaxed) as CFMachPortRef, true) };
        return CallbackResult::Keep;
    }
    if ev.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == MAGIC {
        return CallbackResult::Keep;
    }
    let p = ev.location();
    let pos = (p.x, p.y);
    let button = ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) as u8 + 1; // 0-based -> 1-based
    let mut guard = ENGINE.lock().unwrap();
    let engine = guard.as_mut().unwrap();
    match ty {
        CGEventType::OtherMouseDown if engine.on_down(button, pos, frontmost_app, Instant::now()) => {
            CallbackResult::Drop
        }
        CGEventType::OtherMouseUp => match engine.on_up(button, pos, Instant::now()) {
            Up::Pass => CallbackResult::Keep,
            Up::Swallow(action) => {
                action.inspect(|a| perform(a, p));
                CallbackResult::Drop
            }
        },
        CGEventType::ScrollWheel => scroll(engine, ev, p),
        CGEventType::OtherMouseDragged | CGEventType::MouseMoved => {
            if let Some(a) = engine.on_move(pos) {
                perform(&a, p)
            }
            CallbackResult::Keep
        }
        _ => CallbackResult::Keep,
    }
}

/// Mouse-wheel notches only. Trackpads and the Magic Mouse send continuous (pixel) events, which pass through.
fn scroll(engine: &mut Engine, ev: &CGEvent, at: CGPoint) -> CallbackResult {
    if ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0 {
        return CallbackResult::Keep;
    }
    let dy = ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
    if dy != 0 {
        // The delta's sign follows "natural scrolling"; undo that to get the physical wheel direction.
        let cg = unsafe { &*(ev.as_ptr() as *const objc2_core_graphics::CGEvent) };
        let inverted = NSEvent::eventWithCGEvent(cg).is_some_and(|e| e.isDirectionInvertedFromDevice());
        if let Some(a) = engine.on_held_scroll((dy > 0) != inverted) {
            perform(&a, at);
            return CallbackResult::Drop;
        }
    }
    let modifiers = CGEventFlags::CGEventFlagCommand
        | CGEventFlags::CGEventFlagControl
        | CGEventFlags::CGEventFlagAlternate
        | CGEventFlags::CGEventFlagShift;
    if ev.get_flags().intersects(modifiers) {
        return CallbackResult::Keep; // modifier+scroll is zoom/horizontal in most apps: leave it native
    }
    // ponytail: one notch per event, our flick acceleration replaces macOS's own wheel acceleration.
    let notch = |field| ev.get_integer_value_field(field).signum() as f64;
    let (dx, dy) =
        (notch(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2), notch(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1));
    if (dx, dy) == (0.0, 0.0) {
        return CallbackResult::Keep;
    }
    let Some(s) = engine.scroll(frontmost_app) else { return CallbackResult::Keep };
    SCROLLER.notch(dx, dy, PIXELS_PER_NOTCH * s.factor(), s.smooth);
    CallbackResult::Drop
}

fn post_scroll(dx: i32, dy: i32) {
    let e = CGEvent::new_scroll_event(source(), ScrollEventUnit::PIXEL, 2, dy, dx, 0).unwrap();
    e.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, MAGIC);
    e.post(CGEventTapLocation::HID);
}

/// Connected mice as `hidutil list` reports them. Its table is fixed-width, so columns are cut at the
/// header's positions (product names and transports contain spaces).
pub fn mice() -> Vec<Mouse> {
    let out = std::process::Command::new("hidutil")
        .args(["list", "--matching", r#"{"PrimaryUsagePage":1,"PrimaryUsage":2}"#])
        .output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let Some(section) = text.split("Devices:").nth(1) else { return Vec::new() };
    let mut lines = section.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else { return Vec::new() };
    let col = |name: &str| header.find(name);
    let (Some(transport), Some(class), Some(product), Some(user_class), Some(built_in)) =
        (col("Transport"), col("Class"), col("Product "), col("UserClass"), col("Built-In"))
    else {
        return Vec::new();
    };
    let mut mice: Vec<Mouse> = Vec::new();
    for line in lines {
        let cell = |a: usize, b: usize| line.get(a..b.min(line.len())).unwrap_or("").trim().to_string();
        let hex = |i: usize| {
            line.split_whitespace()
                .nth(i)
                .and_then(|v| u16::from_str_radix(v.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0)
        };
        let name = cell(product, user_class);
        if cell(built_in, line.len()) == "1" || name.is_empty() || mice.iter().any(|m| m.name == name) {
            continue;
        }
        mice.push(Mouse { name, vendor: hex(0), product: hex(1), transport: cell(transport, class) });
    }
    mice
}

fn frontmost_app() -> String {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .and_then(|app| app.localizedName())
        .map(|name| name.to_string())
        .unwrap_or_default()
}

fn perform(action: &Action, at: CGPoint) {
    if practicing() {
        return;
    }
    // ponytail: built-ins are the default macOS shortcuts. If a user rebinds Mission Control, this misses it;
    // reading the real binding via CGSGetSymbolicHotKeyValue (what Mac Mouse Fix does) is the upgrade.
    let combo = match action {
        Action::Keys(c) => return post_combo(c),
        Action::Click(n) => return post_click(*n, at),
        Action::Nothing | Action::Native => return,
        Action::Open(_) | Action::Run(_) => return launch(action),
        Action::VolumeUp => return post_media_key(0), // NX_KEYTYPE_SOUND_UP
        Action::VolumeDown => return post_media_key(1), // NX_KEYTYPE_SOUND_DOWN
        Action::Mute => return post_media_key(7),     // NX_KEYTYPE_MUTE
        Action::PlayPause => return post_media_key(16), // NX_KEYTYPE_PLAY
        Action::NextTrack => return post_media_key(17), // NX_KEYTYPE_NEXT
        Action::PrevTrack => return post_media_key(18), // NX_KEYTYPE_PREVIOUS
        Action::Back => "cmd+[",
        Action::Forward => "cmd+]",
        Action::MissionControl => "ctrl+up",
        Action::AppWindows => "ctrl+down",
        Action::PrevDesktop => "ctrl+left",
        Action::NextDesktop => "ctrl+right",
        Action::ShowDesktop => "f11",
    };
    post_combo(&parse_combo(combo).unwrap());
}

/// Media keys aren't key codes on macOS: they're "system defined" events (subtype 8, key in data1's high bits).
fn post_media_key(key: isize) {
    for down in [true, false] {
        let state: isize = if down { 0xa } else { 0xb };
        let ev = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::SystemDefined,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags((state << 8) as usize),
            0.0,
            0,
            None,
            8,
            (key << 16) | (state << 8),
            -1,
        );
        if let Some(cg) = ev.and_then(|e| e.CGEvent()) {
            objc2_core_graphics::CGEvent::post(objc2_core_graphics::CGEventTapLocation::HIDEventTap, Some(&cg));
        }
    }
}

fn source() -> CGEventSource {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState).expect("event source")
}

fn post_combo(c: &Combo) {
    let code = key_code(&c.key).unwrap();
    let mut flags = CGEventFlags::empty();
    if c.mods.meta {
        flags |= CGEventFlags::CGEventFlagCommand
    }
    if c.mods.ctrl {
        flags |= CGEventFlags::CGEventFlagControl
    }
    if c.mods.alt {
        flags |= CGEventFlags::CGEventFlagAlternate
    }
    if c.mods.shift {
        flags |= CGEventFlags::CGEventFlagShift
    }
    // Real arrow keys carry fn+numpad flags and F-keys carry fn; the system's
    // Mission Control / Spaces shortcuts only match when ours do too.
    if (123..=126).contains(&code) {
        flags |= CGEventFlags::CGEventFlagNumericPad | CGEventFlags::CGEventFlagSecondaryFn;
    } else if c.key.len() > 1 && c.key.starts_with('f') && c.key[1..].parse::<u8>().is_ok() {
        flags |= CGEventFlags::CGEventFlagSecondaryFn;
    }
    for down in [true, false] {
        let e = CGEvent::new_keyboard_event(source(), code, down).unwrap();
        e.set_flags(flags);
        e.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, MAGIC);
        e.post(CGEventTapLocation::Session);
    }
}

fn post_click(button: u8, at: CGPoint) {
    for ty in [CGEventType::OtherMouseDown, CGEventType::OtherMouseUp] {
        let e = CGEvent::new_mouse_event(source(), ty, at, CGMouseButton::Center).unwrap();
        e.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, button as i64 - 1);
        e.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, MAGIC);
        e.post(CGEventTapLocation::HID);
    }
}

/// macOS virtual key codes (kVK_*, ANSI layout positions).
pub fn key_code(name: &str) -> Option<u16> {
    Some(match name {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" => 24,
        "9" => 25,
        "7" => 26,
        "-" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "return" => 36,
        "l" => 37,
        "j" => 38,
        "'" => 39,
        "k" => 40,
        ";" => 41,
        "\\" => 42,
        "," => 43,
        "/" => 44,
        "n" => 45,
        "m" => 46,
        "." => 47,
        "tab" => 48,
        "space" => 49,
        "`" => 50,
        "backspace" => 51,
        "escape" => 53,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        "home" => 115,
        "pageup" => 116,
        "delete" => 117,
        "end" => 119,
        "pagedown" => 121,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        _ => return None,
    })
}
