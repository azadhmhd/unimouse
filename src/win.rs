//! Windows backend: low-level mouse hook in, SendInput out.

use crate::{
    engine::{launch, parse_combo, practicing, Action, Combo, Engine, Mouse, Up, INPUT_ACTIVE},
    scroll::SCROLLER,
};
use std::{
    mem::size_of,
    path::Path,
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex,
    },
    time::Instant,
};
use windows::{
    core::PWSTR,
    Win32::{
        Foundation::{CloseHandle, LPARAM, LRESULT, WPARAM},
        System::Threading::{
            GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

/// Tags the input we send so our own hook lets it through.
const MAGIC: usize = 0x756E_696D;
static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
/// Actions run from the message loop, not inside the hook: Windows silently
/// unhooks low-level hooks that take too long (LowLevelHooksTimeout).
static QUEUE: Mutex<Vec<Action>> = Mutex::new(Vec::new());
static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);

/// ponytail: Windows names every mouse "HID-compliant mouse"; read the HID product string via SetupAPI if the UI needs real names.
pub fn mice() -> Vec<Mouse> {
    Vec::new()
}

/// `--background` (the login copy): drop the console window.
pub fn detach_console() {
    let _ = unsafe { windows::Win32::System::Console::FreeConsole() };
}

pub fn run(engine: Engine) {
    *ENGINE.lock().unwrap() = Some(engine);
    SCROLLER.spawn(post_scroll);
    unsafe {
        MAIN_THREAD.store(GetCurrentThreadId(), Ordering::Relaxed);
        let _hook = SetWindowsHookExW(WH_MOUSE_LL, Some(hook), None, 0).expect("SetWindowsHookExW failed");
        INPUT_ACTIVE.store(true, Ordering::Relaxed);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_APP {
                let jobs = std::mem::take(&mut *QUEUE.lock().unwrap());
                jobs.iter().for_each(perform);
            }
        }
    }
}

unsafe extern "system" fn hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && handle(w.0 as u32, unsafe { &*(l.0 as *const MSLLHOOKSTRUCT) }) {
        return LRESULT(1); // swallow
    }
    unsafe { CallNextHookEx(None, code, w, l) }
}

/// Returns true to swallow the event.
fn handle(msg: u32, info: &MSLLHOOKSTRUCT) -> bool {
    if info.dwExtraInfo == MAGIC {
        return false;
    }
    let pos = (info.pt.x as f64, info.pt.y as f64);
    let button = match msg {
        WM_MBUTTONDOWN | WM_MBUTTONUP => 3,
        _ => 3 + (info.mouseData >> 16) as u8, // XBUTTON1 -> 4, XBUTTON2 -> 5
    };
    let mut guard = ENGINE.lock().unwrap();
    let engine = guard.as_mut().unwrap();
    match msg {
        WM_MBUTTONDOWN | WM_XBUTTONDOWN => engine.on_down(button, pos, foreground_app, Instant::now()),
        WM_MBUTTONUP | WM_XBUTTONUP => match engine.on_up(button, pos, Instant::now()) {
            Up::Pass => false,
            Up::Swallow(action) => {
                if let Some(a) = action {
                    queue(a)
                }
                true
            }
        },
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => scroll(engine, msg, info),
        WM_MOUSEMOVE => {
            if let Some(a) = engine.on_move(pos) {
                queue(a)
            }
            false
        }
        _ => false,
    }
}

/// Classic wheel notches only (multiples of 120). Touchpads and free-spin/high-res wheels
/// send smaller deltas and already scroll smoothly, so they pass through.
fn scroll(engine: &mut Engine, msg: u32, info: &MSLLHOOKSTRUCT) -> bool {
    let delta = (info.mouseData >> 16) as u16 as i16 as i32;
    // Positive = wheel pushed away from you; Windows has no natural-scrolling flip for mice.
    // ponytail: whole notches only, so a high-res wheel doesn't fire 15 volume steps per click.
    if msg == WM_MOUSEWHEEL && delta != 0 && delta % 120 == 0 {
        if let Some(a) = engine.on_held_scroll(delta > 0) {
            queue(a);
            return true;
        }
    }
    let held = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) } < 0;
    // modifier+wheel is zoom/horizontal in most apps: leave it native
    if delta == 0 || delta % 120 != 0 || [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN].into_iter().any(held) {
        return false;
    }
    let Some(s) = engine.scroll(foreground_app) else { return false };
    let n = (delta / 120) as f64;
    let (dx, dy) = if msg == WM_MOUSEHWHEEL { (n, 0.0) } else { (0.0, n) };
    // 120 = one native notch, so speed 1.0 covers the same distance Windows would, just smoothly.
    // Apps that ignore sub-notch deltas (rare, legacy Win32) still scroll, in whole notches.
    SCROLLER.notch(dx, dy, 120.0 * s.factor(), s.smooth);
    true
}

fn post_scroll(dx: i32, dy: i32) {
    let wheel = |flags, amount: i32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: amount as _, dwFlags: flags, time: 0, dwExtraInfo: MAGIC },
        },
    };
    let mut inputs = Vec::new();
    if dy != 0 {
        inputs.push(wheel(MOUSEEVENTF_WHEEL, dy))
    }
    if dx != 0 {
        inputs.push(wheel(MOUSEEVENTF_HWHEEL, dx))
    }
    unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
}

fn queue(action: Action) {
    QUEUE.lock().unwrap().push(action);
    let _ = unsafe { PostThreadMessageW(MAIN_THREAD.load(Ordering::Relaxed), WM_APP, WPARAM(0), LPARAM(0)) };
}

/// Exe name without extension, e.g. "chrome", "Code".
fn foreground_app() -> String {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

fn perform(action: &Action) {
    if practicing() {
        return;
    }
    let combo = match action {
        Action::Keys(c) => return send_combo(c),
        Action::Click(n) => return send_click(*n),
        Action::Nothing | Action::Native => return,
        Action::Open(_) | Action::Run(_) => return launch(action),
        Action::VolumeUp => "volume_up",
        Action::VolumeDown => "volume_down",
        Action::Mute => "mute",
        Action::PlayPause => "play_pause",
        Action::NextTrack => "next_track",
        Action::PrevTrack => "prev_track",
        Action::Back => "browser_back",
        Action::Forward => "browser_forward",
        Action::MissionControl => "win+tab",
        Action::AppWindows => "win+tab", // ponytail: Windows has no per-app Exposé; Task View is the closest
        Action::PrevDesktop => "ctrl+win+left",
        Action::NextDesktop => "ctrl+win+right",
        Action::ShowDesktop => "win+d",
    };
    send_combo(&parse_combo(combo).unwrap());
}

fn send_combo(c: &Combo) {
    let mut keys = Vec::new();
    if c.mods.ctrl {
        keys.push(VK_CONTROL.0)
    }
    if c.mods.alt {
        keys.push(VK_MENU.0)
    }
    if c.mods.shift {
        keys.push(VK_SHIFT.0)
    }
    if c.mods.meta {
        keys.push(VK_LWIN.0)
    }
    keys.push(key_code(&c.key).unwrap());
    let inputs: Vec<INPUT> =
        keys.iter().map(|&k| key_input(k, false)).chain(keys.iter().rev().map(|&k| key_input(k, true))).collect();
    unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
}

fn key_input(vk: u16, up: bool) -> INPUT {
    // Arrows, nav keys, Win and browser keys are "extended": without the flag they act as numpad keys.
    const EXTENDED: [u16; 19] = [
        0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2D, 0x2E, 0x5B, 0xA6, 0xA7, 0xAD, 0xAE, 0xAF, 0xB0, 0xB1,
        0xB3,
    ];
    let mut flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
    if EXTENDED.contains(&vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: MAGIC },
        },
    }
}

fn send_click(button: u8) {
    let (down, up, data) = match button {
        3 => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        n => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, (n - 3) as _), // 4 -> XBUTTON1, 5 -> XBUTTON2
    };
    let mouse = |flags| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: MAGIC },
        },
    };
    unsafe { SendInput(&[mouse(down), mouse(up)], size_of::<INPUT>() as i32) };
}

/// Windows virtual key codes.
pub fn key_code(name: &str) -> Option<u16> {
    let b = name.as_bytes();
    if let [c @ (b'a'..=b'z' | b'0'..=b'9')] = b {
        return Some(c.to_ascii_uppercase() as u16);
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()).filter(|n| (1..=12).contains(n)) {
        return Some(0x6F + n); // F1 = 0x70
    }
    Some(match name {
        "backspace" => 0x08,
        "tab" => 0x09,
        "return" => 0x0D,
        "escape" => 0x1B,
        "space" => 0x20,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "end" => 0x23,
        "home" => 0x24,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "browser_back" => 0xA6,
        "browser_forward" => 0xA7,
        "mute" => 0xAD,
        "volume_down" => 0xAE,
        "volume_up" => 0xAF,
        "next_track" => 0xB0,
        "prev_track" => 0xB1,
        "play_pause" => 0xB3,
        ";" => 0xBA,
        "=" => 0xBB,
        "," => 0xBC,
        "-" => 0xBD,
        "." => 0xBE,
        "/" => 0xBF,
        "`" => 0xC0,
        "[" => 0xDB,
        "\\" => 0xDC,
        "]" => 0xDD,
        "'" => 0xDE,
        _ => return None,
    })
}
