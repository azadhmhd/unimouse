//! Platform-independent part: config, per-app profiles, button gestures.
//! Platforms feed raw button/move events in and perform the actions that come out.

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    path::PathBuf,
    process::Command,
    sync::{atomic::AtomicBool, Mutex},
    thread,
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Gesture {
    Click,
    Left,
    Right,
    Up,
    Down,
    ScrollUp,
    ScrollDown,
    Hold,
}

const ALL_GESTURES: [Gesture; 8] = [
    Gesture::Click,
    Gesture::Left,
    Gesture::Right,
    Gesture::Up,
    Gesture::Down,
    Gesture::ScrollUp,
    Gesture::ScrollDown,
    Gesture::Hold,
];

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Mods {
    pub meta: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Combo {
    pub mods: Mods,
    pub key: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Back,
    Forward,
    MissionControl,
    AppWindows,
    PrevDesktop,
    NextDesktop,
    ShowDesktop,
    VolumeUp,
    VolumeDown,
    Mute,
    PlayPause,
    NextTrack,
    PrevTrack,
    Keys(Combo),
    /// Open a URL, file, folder or app with the OS default handler.
    Open(String),
    /// Run a shell command (sh on macOS/Linux, cmd on Windows). Hook up scripts, AI tools, anything.
    Run(String),
    /// Re-send the original click: the button has gestures but no click action.
    Click(u8),
    /// Swallow the button.
    Nothing,
    /// Leave the button to the OS/app.
    Native,
}

type Profile = HashMap<(u8, Gesture), Action>;

/// Pseudo-button for the per-app `scroll = "native"` opt-out.
const SCROLL: u8 = 0;

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Scroll {
    pub smooth: bool,
    /// Distance multiplier: the knob for "too slow / too fast" on your mouse.
    pub speed: f64,
    /// Flip wheel direction for mice only (trackpads untouched).
    pub reverse: bool,
}

impl Default for Scroll {
    fn default() -> Self {
        Scroll { smooth: true, speed: 1.0, reverse: false }
    }
}

impl Scroll {
    /// Multiplier for a notch's distance: speed, with the sign flipped when reversed.
    pub fn factor(&self) -> f64 {
        if self.reverse {
            -self.speed
        } else {
            self.speed
        }
    }
}

#[derive(Debug)]
pub struct Config {
    pub drag_threshold: f64,
    /// Press longer than this (without dragging) = `buttonN_hold`.
    pub hold: Duration,
    pub scroll: Scroll,
    default: Profile,
    /// (aliases, profile). Aliases are lowercase app names.
    apps: Vec<(Vec<String>, Profile)>,
}

/// The config file as written: what the settings UI reads and saves. BTreeMaps keep the file's order stable.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Raw {
    #[serde(default = "default_threshold")]
    pub drag_threshold: f64,
    #[serde(default = "default_hold_ms")]
    pub hold_ms: u64,
    #[serde(default)]
    pub scroll: Scroll,
    #[serde(default)]
    pub default: BTreeMap<String, String>,
    #[serde(default)]
    pub app: BTreeMap<String, BTreeMap<String, String>>,
}

impl Raw {
    pub fn parse(text: &str) -> Result<Raw, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// TOML text, validated by the same parser the engine uses.
    pub fn to_toml(&self) -> Result<String, String> {
        let body = toml::to_string(self).map_err(|e| e.to_string())?;
        let text = format!("# Saved by unimouse settings (unimouse --settings). Hand edits work too, but comments aren't kept.\n\n{body}");
        Config::parse(&text)?;
        Ok(text)
    }
}

fn default_threshold() -> f64 {
    40.0
}
fn default_hold_ms() -> u64 {
    400
}

pub const EXAMPLE_CONFIG: &str = include_str!("../config.example.toml");

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        let raw = Raw::parse(text)?;
        let profile = |table: BTreeMap<String, String>| -> Result<Profile, String> {
            table
                .into_iter()
                .map(|(k, v)| {
                    let (trigger, action) = (parse_trigger(&k)?, parse_action(&v).map_err(|e| format!("{k}: {e}"))?);
                    if trigger.0 == SCROLL && action != Action::Native {
                        return Err(format!("{k}: only \"native\" is supported (turns smooth scrolling off here)"));
                    }
                    Ok((trigger, action))
                })
                .collect()
        };
        let mut apps = Vec::new();
        for (name, table) in raw.app {
            let aliases = name.split('|').map(|s| s.trim().to_lowercase()).collect();
            apps.push((aliases, profile(table).map_err(|e| format!("[app.{name}] {e}"))?));
        }
        Ok(Config {
            drag_threshold: raw.drag_threshold,
            hold: Duration::from_millis(raw.hold_ms),
            scroll: raw.scroll,
            default: profile(raw.default).map_err(|e| format!("[default] {e}"))?,
            apps,
        })
    }

    /// Index of the app profile matching the frontmost app's name, if any.
    /// An alias ending in `*` matches by prefix, for names with versions ("adobe photoshop*").
    pub fn app_profile(&self, app_name: &str) -> Option<usize> {
        let name = app_name.to_lowercase();
        let matches =
            |alias: &String| alias.strip_suffix('*').map_or(*alias == name, |prefix| name.starts_with(prefix));
        self.apps.iter().position(|(aliases, _)| aliases.iter().any(matches))
    }

    pub fn lookup(&self, app: Option<usize>, button: u8, g: Gesture) -> Option<&Action> {
        app.and_then(|i| self.apps[i].1.get(&(button, g))).or_else(|| self.default.get(&(button, g)))
    }
}

/// Config trigger suffixes: "button4" + "_drag_left" etc.
const TRIGGER_SUFFIXES: [(&str, Gesture); 8] = [
    ("", Gesture::Click),
    ("_hold", Gesture::Hold),
    ("_drag_left", Gesture::Left),
    ("_drag_right", Gesture::Right),
    ("_drag_up", Gesture::Up),
    ("_drag_down", Gesture::Down),
    ("_scroll_up", Gesture::ScrollUp),
    ("_scroll_down", Gesture::ScrollDown),
];

fn trigger_name(button: u8, g: Gesture) -> String {
    let suffix = TRIGGER_SUFFIXES.iter().find(|(_, x)| *x == g).map_or("", |(s, _)| s);
    format!("button{button}{suffix}")
}

fn parse_trigger(k: &str) -> Result<(u8, Gesture), String> {
    let bad = || {
        format!("unknown trigger '{k}' (expected e.g. button4, button4_drag_left, button4_scroll_up, button4_hold or scroll)")
    };
    if k == "scroll" {
        return Ok((SCROLL, Gesture::Click));
    }
    let rest = k.strip_prefix("button").ok_or_else(bad)?;
    let (num, suffix) = rest.split_at(rest.bytes().take_while(u8::is_ascii_digit).count());
    let gesture = TRIGGER_SUFFIXES.iter().find(|(s, _)| *s == suffix).ok_or_else(bad)?.1;
    // ponytail: buttons 1/2 (left/right) are never remapped, too easy to lock yourself out.
    match num.parse::<u8>() {
        Ok(n @ 3..=16) => Ok((n, gesture)),
        _ => Err(bad()),
    }
}

/// Actions written as a plain name in the config.
const NAMED_ACTIONS: [(&str, Action); 15] = [
    ("back", Action::Back),
    ("forward", Action::Forward),
    ("mission_control", Action::MissionControl),
    ("app_windows", Action::AppWindows),
    ("prev_desktop", Action::PrevDesktop),
    ("next_desktop", Action::NextDesktop),
    ("show_desktop", Action::ShowDesktop),
    ("volume_up", Action::VolumeUp),
    ("volume_down", Action::VolumeDown),
    ("mute", Action::Mute),
    ("play_pause", Action::PlayPause),
    ("next_track", Action::NextTrack),
    ("prev_track", Action::PrevTrack),
    ("none", Action::Nothing),
    ("native", Action::Native),
];

impl Action {
    /// The action as the config spells it (what the settings UI shows in its activity feed).
    pub fn to_config(&self) -> String {
        match self {
            Action::Keys(c) => {
                let mac = cfg!(target_os = "macos");
                let mut parts: Vec<&str> = Vec::new();
                if c.mods.meta {
                    parts.push(if mac { "mod" } else { "win" })
                }
                if c.mods.ctrl {
                    parts.push(if mac { "ctrl" } else { "mod" })
                }
                if c.mods.alt {
                    parts.push("alt")
                }
                if c.mods.shift {
                    parts.push("shift")
                }
                parts.push(&c.key);
                format!("keys:{}", parts.join("+"))
            }
            Action::Open(t) => format!("open:{t}"),
            Action::Run(c) => format!("run:{c}"),
            Action::Click(n) => format!("click{n}"),
            named => NAMED_ACTIONS.iter().find(|(_, a)| a == named).map_or_else(String::new, |(n, _)| n.to_string()),
        }
    }
}

pub fn parse_action(s: &str) -> Result<Action, String> {
    if let Some((_, a)) = NAMED_ACTIONS.iter().find(|(n, _)| *n == s) {
        return Ok(a.clone());
    }
    Ok(match s.split_once(':') {
        Some(("keys", c)) => Action::Keys(parse_combo(c)?),
        Some(("open", t)) if !t.trim().is_empty() => Action::Open(t.trim().to_string()),
        Some(("run", c)) if !c.trim().is_empty() => Action::Run(c.trim().to_string()),
        _ => return Err(format!("unknown action '{s}'")),
    })
}

/// `open:` and `run:`. Fire and forget; a thread reaps the child so it doesn't linger as a zombie.
pub fn launch(action: &Action) {
    let mut cmd = match action {
        #[cfg(target_os = "macos")]
        Action::Open(target) => {
            let mut c = Command::new("open");
            c.arg(target);
            c
        }
        #[cfg(windows)]
        Action::Open(target) => {
            let mut c = Command::new("explorer");
            c.arg(target);
            c
        }
        #[cfg(target_os = "linux")]
        Action::Open(target) => {
            let mut c = Command::new("xdg-open");
            c.arg(target);
            c
        }
        #[cfg(windows)]
        Action::Run(line) => {
            use std::os::windows::process::CommandExt;
            let mut c = Command::new("cmd");
            c.arg("/C").raw_arg(line); // raw: cmd has its own quoting rules
            c
        }
        #[cfg(not(windows))]
        Action::Run(line) => {
            let mut c = Command::new("sh");
            c.args(["-c", line]);
            c
        }
        _ => return,
    };
    match cmd.spawn() {
        Ok(mut child) => {
            thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("could not launch {action:?}: {e}"),
    }
}

/// "mod+shift+t" -> Combo. `mod` is Cmd on macOS and Ctrl elsewhere, so one config works everywhere.
pub fn parse_combo(s: &str) -> Result<Combo, String> {
    let parts: Vec<String> = s.split('+').map(|p| p.trim().to_lowercase()).collect();
    let (key, mods_str) = parts.split_last().ok_or("empty key combo")?;
    let mut mods = Mods::default();
    for m in mods_str {
        match m.as_str() {
            "mod" if cfg!(target_os = "macos") => mods.meta = true,
            "mod" => mods.ctrl = true,
            "cmd" | "command" | "win" | "super" | "meta" => mods.meta = true,
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "opt" | "option" => mods.alt = true,
            "shift" => mods.shift = true,
            _ => return Err(format!("unknown modifier '{m}' in '{s}'")),
        }
    }
    let key = match key.as_str() {
        "enter" => "return",
        "esc" => "escape",
        k => k,
    }
    .to_string();
    if crate::platform::key_code(&key).is_none() {
        return Err(format!("unknown key '{key}' in '{s}'"));
    }
    Ok(Combo { mods, key })
}

/// What the settings UI sees: a feed of presses and fired actions, and the modes it has asked for.
pub struct Monitor {
    pub events: VecDeque<Event>,
    next_id: u64,
    /// The UI polled recently: worth looking up the app name on each press.
    pub watch_until: Option<Instant>,
    /// "Find a new button" is open: presses are recorded but trigger nothing.
    pub learn_until: Option<Instant>,
    /// Practice mode: gestures are recognised and reported, but no action is performed.
    pub practice_until: Option<Instant>,
}

/// `kind` "press": a button went down. `kind` "action": `trigger` fired `action` (config spelling).
#[derive(Serialize, Clone)]
pub struct Event {
    pub id: u64,
    pub kind: &'static str,
    pub button: u8,
    pub app: String,
    pub trigger: String,
    pub action: String,
}

pub static MONITOR: Mutex<Monitor> = Mutex::new(Monitor {
    events: VecDeque::new(),
    next_id: 1,
    watch_until: None,
    learn_until: None,
    practice_until: None,
});

/// A connected mouse, as the settings UI shows it ("MX Master 3S").
#[derive(Serialize, Clone)]
pub struct Mouse {
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    pub transport: String,
}

/// Set by the platform once it's really capturing input (on macOS, after Accessibility is granted).
pub static INPUT_ACTIVE: AtomicBool = AtomicBool::new(false);

impl Monitor {
    fn push(&mut self, kind: &'static str, button: u8, app: String, trigger: String, action: String) {
        self.events.push_back(Event { id: self.next_id, kind, button, app, trigger, action });
        self.next_id += 1;
        if self.events.len() > 100 {
            self.events.pop_front();
        }
    }

    pub(crate) fn record(&mut self, button: u8, app: String) {
        self.push("press", button, app, String::new(), String::new())
    }
}

/// Platforms check this before performing an action: in practice mode actions are only reported.
pub fn practicing() -> bool {
    MONITOR.lock().unwrap().practice_until.is_some_and(|t| Instant::now() < t)
}

/// Turns raw button events into actions. One button is tracked at a time.
pub struct Engine {
    pub cfg: Config,
    path: PathBuf,
    mtime: Option<SystemTime>,
    held: Option<Held>,
}

struct Held {
    button: u8,
    app: Option<usize>,
    start: (f64, f64),
    since: Instant,
    fired: bool,
}

#[derive(Debug, PartialEq)]
pub enum Up {
    Pass,
    Swallow(Option<Action>),
}

impl Engine {
    pub fn new(cfg: Config, path: PathBuf) -> Engine {
        let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
        Engine { cfg, path, mtime, held: None }
    }

    /// Hot reload: cheap stat on every button press, re-parse only when the file changed.
    fn reload_if_changed(&mut self) {
        let mtime = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if mtime == self.mtime {
            return;
        }
        self.mtime = mtime;
        match fs::read_to_string(&self.path).map_err(|e| e.to_string()).and_then(|t| Config::parse(&t)) {
            Ok(cfg) => {
                self.cfg = cfg;
                eprintln!("config reloaded");
            }
            Err(e) => eprintln!("config error, keeping previous config: {e}"),
        }
    }

    /// Scroll settings for a mouse-wheel notch, or None to leave the notch alone.
    /// `app_name` is only called when some app opts out, so the common case skips the lookup.
    pub fn scroll(&mut self, app_name: impl FnOnce() -> String) -> Option<Scroll> {
        self.reload_if_changed();
        let s = self.cfg.scroll;
        if !s.smooth && !s.reverse {
            return None;
        }
        let opted_out = |p: &Profile| p.contains_key(&(SCROLL, Gesture::Click));
        if opted_out(&self.cfg.default) || self.cfg.apps.iter().any(|(_, p)| opted_out(p)) {
            let app = self.cfg.app_profile(&app_name());
            if self.cfg.lookup(app, SCROLL, Gesture::Click) == Some(&Action::Native) {
                return None;
            }
        }
        Some(s)
    }

    /// Returns true if the press should be swallowed.
    /// `app_name` is only called when the config has app sections.
    pub fn on_down(&mut self, button: u8, pos: (f64, f64), app_name: impl FnOnce() -> String, now: Instant) -> bool {
        let (watching, learning) = {
            let m = MONITOR.lock().unwrap();
            (m.watch_until.is_some_and(|t| now < t), m.learn_until.is_some_and(|t| now < t))
        };
        let name = if watching || !self.cfg.apps.is_empty() { app_name() } else { String::new() };
        MONITOR.lock().unwrap().record(button, if watching { name.clone() } else { String::new() });
        if self.held.is_some() {
            return false;
        }
        if learning {
            // swallow the press and its release, fire nothing
            self.held = Some(Held { button, app: None, start: pos, since: now, fired: true });
            return true;
        }
        self.reload_if_changed();
        let app = if self.cfg.apps.is_empty() { None } else { self.cfg.app_profile(&name) };
        let bound = |g| self.cfg.lookup(app, button, g);
        if bound(Gesture::Click) == Some(&Action::Native) {
            return false;
        }
        let any = ALL_GESTURES.into_iter().any(|g| bound(g).is_some());
        if any {
            self.held = Some(Held { button, app, start: pos, since: now, fired: false });
        }
        any
    }

    /// Wheel notch while a button is held. A `buttonN_scroll_up/down` binding takes the notch
    /// (e.g. hold + scroll = volume); otherwise None and the notch scrolls normally.
    /// `up` is the physical direction (wheel pushed away from you), whatever "natural scrolling" says.
    pub fn on_held_scroll(&mut self, up: bool) -> Option<Action> {
        let h = self.held.as_mut()?;
        let g = if up { Gesture::ScrollUp } else { Gesture::ScrollDown };
        let action = self.cfg.lookup(h.app, h.button, g)?.clone();
        h.fired = true; // no click on release
        log(h.button, g, &action);
        Some(action)
    }

    /// Pointer moved. Fires a drag gesture as soon as it crosses the threshold (once per press).
    pub fn on_move(&mut self, pos: (f64, f64)) -> Option<Action> {
        let h = self.held.as_mut().filter(|h| !h.fired)?;
        let g = direction(pos.0 - h.start.0, pos.1 - h.start.1, self.cfg.drag_threshold)?;
        h.fired = true; // an unbound direction still counts as a drag, so no click on release
        let (app, button) = (h.app, h.button);
        self.cfg.lookup(app, button, g).cloned().inspect(|a| log(button, g, a))
    }

    pub fn on_up(&mut self, button: u8, pos: (f64, f64), now: Instant) -> Up {
        let Some(h) = self.held.take_if(|h| h.button == button) else { return Up::Pass };
        if h.fired {
            return Up::Swallow(None);
        }
        // No move events seen (some mice/tests jump straight to release): check the release point too.
        let mut g = direction(pos.0 - h.start.0, pos.1 - h.start.1, self.cfg.drag_threshold).unwrap_or(Gesture::Click);
        // ponytail: long-press is decided on release. Fire it mid-press with a timer if waiting for release feels slow.
        if g == Gesture::Click
            && now - h.since >= self.cfg.hold
            && self.cfg.lookup(h.app, button, Gesture::Hold).is_some()
        {
            g = Gesture::Hold;
        }
        let action = match self.cfg.lookup(h.app, button, g) {
            Some(a) => Some(a.clone()),
            None if g == Gesture::Click => Some(Action::Click(button)),
            None => None,
        };
        if let Some(a) = &action {
            log(button, g, a);
        }
        Up::Swallow(action)
    }
}

fn log(button: u8, g: Gesture, a: &Action) {
    if matches!(a, Action::Click(_)) {
        return;
    }
    println!("button{button} {g:?} -> {a:?}");
    MONITOR.lock().unwrap().push("action", button, String::new(), trigger_name(button, g), a.to_config());
}

/// Dominant axis wins. Screen y grows downward on both macOS (CG) and Windows.
fn direction(dx: f64, dy: f64, threshold: f64) -> Option<Gesture> {
    if dx.hypot(dy) < threshold {
        return None;
    }
    Some(if dx.abs() >= dy.abs() {
        if dx < 0.0 {
            Gesture::Left
        } else {
            Gesture::Right
        }
    } else if dy < 0.0 {
        Gesture::Up
    } else {
        Gesture::Down
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(toml: &str) -> Engine {
        Engine::new(Config::parse(toml).unwrap(), PathBuf::from("/nonexistent"))
    }

    /// Tests that press buttons share the global MONITOR (learn mode), so they take turns.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn example_config_parses() {
        let cfg = Config::parse(EXAMPLE_CONFIG).unwrap();
        assert_eq!(cfg.lookup(None, 4, Gesture::Click), Some(&Action::Back));
        let chrome = cfg.app_profile("Google Chrome");
        assert_eq!(chrome, cfg.app_profile("chrome")); // macOS name and Windows exe hit one profile
        assert!(matches!(cfg.lookup(chrome, 5, Gesture::Left), Some(Action::Keys(_))));
        assert_eq!(cfg.lookup(chrome, 4, Gesture::Click), Some(&Action::Back)); // falls back to default
        assert_eq!(cfg.app_profile("Xcode"), None); // exact match, "code" must not catch Xcode
        let ps = Config::parse("[app.\"adobe photoshop*\"]\nbutton4 = \"back\"").unwrap();
        assert_eq!(ps.app_profile("Adobe Photoshop 2026"), Some(0));
        assert_eq!(ps.app_profile("Photoshop"), None);
    }

    #[test]
    fn click_drag_and_passthrough() {
        let _serial = serial();
        let mut e = engine("[default]\nbutton4 = \"back\"\nbutton4_drag_left = \"prev_desktop\"\nbutton3_drag_up = \"mission_control\"");
        // click
        assert!(e.on_down(4, (100.0, 100.0), || "Finder".into(), Instant::now()));
        assert_eq!(e.on_up(4, (102.0, 101.0), Instant::now()), Up::Swallow(Some(Action::Back)));
        // drag fires once, mid-drag
        assert!(e.on_down(4, (100.0, 100.0), || "Finder".into(), Instant::now()));
        assert_eq!(e.on_move((80.0, 100.0)), None);
        assert_eq!(e.on_move((50.0, 105.0)), Some(Action::PrevDesktop));
        assert_eq!(e.on_move((10.0, 105.0)), None);
        assert_eq!(e.on_up(4, (10.0, 105.0), Instant::now()), Up::Swallow(None));
        // gesture-only button keeps its normal click
        assert!(e.on_down(3, (0.0, 0.0), || "Finder".into(), Instant::now()));
        assert_eq!(e.on_up(3, (0.0, 0.0), Instant::now()), Up::Swallow(Some(Action::Click(3))));
        // unbound direction on a bound button: nothing
        assert!(e.on_down(4, (0.0, 0.0), || "Finder".into(), Instant::now()));
        assert_eq!(e.on_up(4, (0.0, 90.0), Instant::now()), Up::Swallow(None));
        // unbound button passes through untouched
        assert!(!e.on_down(5, (0.0, 0.0), || "Finder".into(), Instant::now()));
        assert_eq!(e.on_up(5, (0.0, 0.0), Instant::now()), Up::Pass);
    }

    #[test]
    fn native_in_app_passes_whole_button() {
        let _serial = serial();
        let mut e = engine("[default]\nbutton4 = \"back\"\nbutton4_drag_up = \"mission_control\"\n[app.\"minecraft|javaw\"]\nbutton4 = \"native\"");
        assert!(!e.on_down(4, (0.0, 0.0), || "javaw".into(), Instant::now()));
        assert_eq!(e.on_up(4, (0.0, 0.0), Instant::now()), Up::Pass);
        assert!(e.on_down(4, (0.0, 0.0), || "Safari".into(), Instant::now()));
    }

    #[test]
    fn scroll_settings_and_app_opt_out() {
        let mut e = engine("[scroll]\nspeed = 1.5\n[app.\"minecraft|javaw\"]\nscroll = \"native\"");
        assert_eq!(e.scroll(|| "Safari".into()), Some(Scroll { smooth: true, speed: 1.5, reverse: false }));
        assert_eq!(e.scroll(|| "javaw".into()), None);
        let mut off = engine("[scroll]\nsmooth = false");
        assert_eq!(off.scroll(|| unreachable!("no app lookup needed")), None);
        assert!(Config::parse("[default]\nscroll = \"back\"").is_err());
        assert!(Config::parse("[scroll]\nsmoth = true").is_err());
    }

    #[test]
    fn long_press_and_held_scroll() {
        let _serial = serial();
        let mut e = engine("hold_ms = 300\n[default]\nbutton5 = \"forward\"\nbutton5_hold = \"play_pause\"\nbutton5_scroll_up = \"volume_up\"\nbutton5_scroll_down = \"volume_down\"");
        let t = Instant::now();
        // quick press = click, long press = hold
        assert!(e.on_down(5, (0.0, 0.0), String::new, t));
        assert_eq!(e.on_up(5, (0.0, 0.0), t + Duration::from_millis(100)), Up::Swallow(Some(Action::Forward)));
        assert!(e.on_down(5, (0.0, 0.0), String::new, t));
        assert_eq!(e.on_up(5, (0.0, 0.0), t + Duration::from_millis(500)), Up::Swallow(Some(Action::PlayPause)));
        // hold + scroll: each notch fires, release does nothing
        assert!(e.on_down(5, (0.0, 0.0), String::new, t));
        assert_eq!(e.on_held_scroll(true), Some(Action::VolumeUp));
        assert_eq!(e.on_held_scroll(false), Some(Action::VolumeDown));
        assert_eq!(e.on_up(5, (0.0, 0.0), t), Up::Swallow(None));
        // nothing held: scroll is normal
        assert_eq!(e.on_held_scroll(true), None);
    }

    #[test]
    fn learn_mode_records_and_swallows() {
        let _serial = serial();
        let mut e = engine("[default]\nbutton4 = \"back\"");
        let t = Instant::now();
        MONITOR.lock().unwrap().learn_until = Some(t + Duration::from_secs(5));
        assert!(e.on_down(7, (0.0, 0.0), String::new, t)); // even an unbound button is captured
        assert_eq!(e.on_up(7, (0.0, 0.0), t), Up::Swallow(None));
        assert!(e.on_down(4, (0.0, 0.0), String::new, t));
        assert_eq!(e.on_up(4, (0.0, 0.0), t), Up::Swallow(None)); // bound button: no action while learning
        assert!(MONITOR.lock().unwrap().events.iter().any(|p| p.kind == "press" && p.button == 7));
        MONITOR.lock().unwrap().learn_until = None;
    }

    #[test]
    fn fired_actions_are_reported_in_config_spelling() {
        let _serial = serial();
        let mut e = engine("[default]\nbutton4_drag_left = \"keys:mod+shift+t\"\nbutton5_hold = \"play_pause\"");
        let t = Instant::now();
        assert!(e.on_down(4, (100.0, 0.0), String::new, t));
        assert!(e.on_move((10.0, 0.0)).is_some());
        let m = MONITOR.lock().unwrap();
        let fired: Vec<_> =
            m.events.iter().filter(|e| e.kind == "action").map(|e| (e.trigger.clone(), e.action.clone())).collect();
        assert!(fired.contains(&("button4_drag_left".into(), "keys:mod+shift+t".into())), "{fired:?}");
        for (name, a) in &NAMED_ACTIONS {
            assert_eq!(&parse_action(name).unwrap(), a);
            assert_eq!(a.to_config(), *name);
        }
    }

    #[test]
    fn raw_round_trips_through_toml() {
        let raw = Raw::parse(EXAMPLE_CONFIG).unwrap();
        let text = raw.to_toml().unwrap();
        let again = Raw::parse(&text).unwrap();
        assert_eq!(again.default, raw.default);
        assert_eq!(again.app, raw.app);
        assert_eq!(again.scroll, raw.scroll);
        let mut bad = again;
        bad.default.insert("button4".into(), "teleport".into());
        assert!(bad.to_toml().is_err()); // the UI can't save a config the engine would reject
    }

    #[test]
    fn open_and_run() {
        assert_eq!(parse_action("open: https://example.com "), Ok(Action::Open("https://example.com".into())));
        assert!(parse_action("run:").is_err());
        #[cfg(unix)]
        {
            let marker = std::env::temp_dir().join(format!("unimouse-test-{}", std::process::id()));
            let _ = fs::remove_file(&marker);
            launch(&parse_action(&format!("run: touch '{}'", marker.display())).unwrap());
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(marker.exists(), "run: action did not execute");
            let _ = fs::remove_file(&marker);
        }
    }

    #[test]
    fn combos_and_errors() {
        let c = parse_combo("Mod+Shift+T").unwrap();
        assert_eq!(c.key, "t");
        assert!(
            c.mods.shift && (c.mods.meta == cfg!(target_os = "macos")) && (c.mods.ctrl != cfg!(target_os = "macos"))
        );
        assert_eq!(parse_combo("ctrl+enter").unwrap().key, "return");
        assert!(parse_combo("hyper+t").is_err());
        assert!(parse_combo("mod+nosuchkey").is_err());
        assert!(Config::parse("[default]\nbutton1 = \"back\"").is_err());
        assert!(Config::parse("[default]\nbutton4 = \"teleport\"").is_err());
        assert!(Config::parse("[default]\nbutton4_drag_sideways = \"back\"").is_err());
        assert!(Config::parse("[default]\nbutton4_hold_forever = \"back\"").is_err());
    }
}
