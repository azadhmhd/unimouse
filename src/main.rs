mod engine;
mod install;
mod scroll;
mod ui;

#[cfg(target_os = "macos")]
#[path = "mac.rs"]
mod platform;

#[cfg(windows)]
#[path = "win.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
compile_error!("unimouse supports macOS, Windows and Linux");

use engine::{launch, Action, Config, Engine, EXAMPLE_CONFIG};
use std::{env, fs, path::PathBuf, process::exit};

const USAGE: &str = "usage: unimouse [config.toml] [--settings | --install | --uninstall]\n  \
  --settings   open the settings page (starts unimouse if it isn't running)\n  \
  --install    start at login (and now, in the background)\n  \
  --uninstall  stop, and don't start at login";

/// Default config location: the per-user app-data folder. Pass a path instead to use a
/// synced folder (iCloud, Dropbox, OneDrive) and share one config across machines.
fn default_config_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = PathBuf::from(env::var_os("HOME").expect("HOME not set")).join("Library/Application Support");
    #[cfg(windows)]
    let base = PathBuf::from(env::var_os("APPDATA").expect("APPDATA not set"));
    #[cfg(target_os = "linux")]
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").expect("HOME not set")).join(".config"));
    base.join("unimouse").join("config.toml")
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    if args.iter().any(|a| {
        a.starts_with('-') && !["--settings", "--install", "--uninstall", "--background"].contains(&a.as_str())
    }) {
        println!("{USAGE}");
        exit(if flag("--help") || flag("-h") { 0 } else { 2 });
    }
    if flag("--uninstall") {
        report(install::uninstall())
    }
    let path = args.iter().find(|a| !a.starts_with("--")).map(PathBuf::from).unwrap_or_else(default_config_path);
    if !path.exists() {
        let written = path.parent().map_or(Ok(()), fs::create_dir_all).and_then(|_| fs::write(&path, EXAMPLE_CONFIG));
        if let Err(e) = written {
            eprintln!("could not write {}: {e}", path.display());
            exit(1);
        }
        println!("wrote default config to {}", path.display());
    }
    let cfg =
        fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| Config::parse(&t)).unwrap_or_else(|e| {
            eprintln!("{}: {e}", path.display());
            exit(1)
        });
    if flag("--install") {
        report(install::install(&fs::canonicalize(&path).unwrap_or(path)))
    }

    // One copy at a time: a login copy plus a terminal copy would fire every action twice.
    let lock_path = path.with_extension("lock");
    let lock = fs::File::create(&lock_path).and_then(|f| f.try_lock().map(|_| f).map_err(std::io::Error::other));
    let Ok(_lock) = lock else {
        if flag("--settings") {
            if let Ok(url) = fs::read_to_string(ui::url_file(&path)) {
                launch(&Action::Open(url));
                exit(0);
            }
        }
        eprintln!(
            "unimouse is already running with this config (maybe the copy started at login).\n\
                   Stop it first, or run: unimouse --uninstall"
        );
        exit(1);
    };
    #[cfg(windows)]
    if flag("--background") {
        platform::detach_console()
    }

    match ui::start(path.clone(), ui::PORT) {
        Ok(url) => {
            if let Err(e) = ui::write_url_file(&path, &url) {
                eprintln!("could not save settings URL: {e}")
            }
            if flag("--settings") {
                launch(&Action::Open(url))
            }
            println!("Settings page: run `unimouse --settings`");
        }
        Err(e) => eprintln!("settings page unavailable (is port {} in use?): {e}", ui::PORT),
    }
    println!("unimouse running. Config: {} (edits apply live). Ctrl+C to quit.", path.display());
    platform::run(Engine::new(cfg, path));
}

fn report(result: Result<String, String>) -> ! {
    match result {
        Ok(msg) => {
            println!("{msg}");
            exit(0)
        }
        Err(e) => {
            eprintln!("{e}");
            exit(1)
        }
    }
}
