//! `--install` / `--uninstall`: start unimouse at login, using each OS's own mechanism
//! (launchd agent, HKCU Run key, XDG autostart entry).

use std::process::Command;
use std::{env, path::Path};
#[cfg(not(windows))]
use std::{fs, path::PathBuf};

#[cfg(not(windows))]
fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").expect("HOME not set"))
}

fn exe() -> Result<String, String> {
    env::current_exe().map(|p| p.display().to_string()).map_err(|e| format!("can't find own executable: {e}"))
}

#[cfg(target_os = "macos")]
const LABEL: &str = "com.unimouse.agent";
#[cfg(target_os = "macos")]
fn plist() -> PathBuf {
    home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

#[cfg(target_os = "macos")]
pub fn install(config: &Path) -> Result<String, String> {
    let xml = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let exe = exe()?;
    let log = home().join("Library/Logs/unimouse.log");
    let plist_text = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
        xml(&exe),
        xml(&config.display().to_string()),
        xml(&log.display().to_string()),
        xml(&log.display().to_string())
    );
    let path = plist();
    fs::create_dir_all(path.parent().unwrap()).and_then(|_| fs::write(&path, plist_text)).map_err(|e| e.to_string())?;
    let _ = Command::new("launchctl").args(["unload", &path.display().to_string()]).output();
    run("launchctl", &["load", "-w", &path.display().to_string()])?;
    Ok(format!(
        "Installed: unimouse now starts at login and is running in the background.\n\
                Give it Accessibility access: System Settings > Privacy & Security > Accessibility > + > {exe}\n\
                Log: {}",
        log.display()
    ))
}

#[cfg(target_os = "macos")]
pub fn is_installed() -> bool {
    plist().exists()
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<String, String> {
    let path = plist();
    let _ = Command::new("launchctl").args(["unload", "-w", &path.display().to_string()]).output();
    let _ = fs::remove_file(&path);
    Ok("Uninstalled: stopped, and won't start at login.".into())
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
pub fn install(config: &Path) -> Result<String, String> {
    let exe = exe()?;
    let line = format!("\"{exe}\" --background \"{}\"", config.display());
    run("reg", &["add", RUN_KEY, "/v", "unimouse", "/t", "REG_SZ", "/d", &line, "/f"])?;
    Command::new(&exe).arg("--background").arg(config).spawn().map_err(|e| e.to_string())?;
    Ok("Installed: unimouse now starts at login and is running in the background.".into())
}

#[cfg(windows)]
pub fn is_installed() -> bool {
    run("reg", &["query", RUN_KEY, "/v", "unimouse"]).is_ok()
}

#[cfg(windows)]
pub fn uninstall() -> Result<String, String> {
    run("reg", &["delete", RUN_KEY, "/v", "unimouse", "/f"])?;
    // ponytail: doesn't stop the running copy. End "unimouse.exe" in Task Manager, or log out.
    Ok("Uninstalled: won't start at login. The running copy stays until you end it in Task Manager.".into())
}

#[cfg(target_os = "linux")]
fn desktop_file() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("autostart/unimouse.desktop")
}

#[cfg(target_os = "linux")]
pub fn install(config: &Path) -> Result<String, String> {
    // Desktop Entry spec quoting: wrap in quotes, escape " ` $ \
    let quote = |s: &str| {
        format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\"").replace('`', "\\`").replace('$', "\\$"))
    };
    let exe = exe()?;
    let entry = format!("[Desktop Entry]\nType=Application\nName=unimouse\nExec={} {}\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
                        quote(&exe), quote(&config.display().to_string()));
    let path = desktop_file();
    fs::create_dir_all(path.parent().unwrap()).and_then(|_| fs::write(&path, entry)).map_err(|e| e.to_string())?;
    Command::new(&exe).arg(config).spawn().map_err(|e| e.to_string())?;
    Ok(format!("Installed: unimouse starts at login ({}) and is running now.", path.display()))
}

#[cfg(target_os = "linux")]
pub fn is_installed() -> bool {
    desktop_file().exists()
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> Result<String, String> {
    let _ = fs::remove_file(desktop_file());
    // Stop the running copies, but not this process (it's called unimouse too).
    let pids = Command::new("pgrep")
        .args(["-x", "unimouse"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    for pid in pids.unwrap_or_default().split_whitespace().filter(|p| *p != std::process::id().to_string()) {
        let _ = Command::new("kill").arg(pid).output();
    }
    Ok("Uninstalled: stopped, and won't start at login.".into())
}

#[allow(dead_code)] // unused on Linux
fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(program).args(args).output().map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}
