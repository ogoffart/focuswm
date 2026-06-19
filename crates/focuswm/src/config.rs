//! App discovery + spawning of client programs (terminal, browser) into the
//! compositor's Wayland socket.

use std::process::Command;

/// Connection info for spawned clients: which Wayland socket to use and where it
/// lives.
#[derive(Clone, Debug, Default)]
pub struct SpawnEnv {
    pub wayland_display: String,
    pub runtime_dir: String,
}

/// Find a terminal emulator, preferring Wayland-native ones. Returns the program
/// plus any fixed arguments.
pub fn terminal_command() -> Vec<String> {
    if let Ok(t) = std::env::var("FOCUSWM_TERMINAL") {
        return shell_split(&t);
    }
    for cand in ["alacritty", "foot", "kitty", "wezterm"] {
        if which(cand) {
            return vec![cand.to_string()];
        }
    }
    vec!["xterm".to_string()]
}

/// Find a web browser.
pub fn browser_command() -> Vec<String> {
    if let Ok(b) = std::env::var("FOCUSWM_BROWSER") {
        return shell_split(&b);
    }
    for cand in ["firefox", "chromium", "google-chrome", "epiphany"] {
        if which(cand) {
            return vec![cand.to_string()];
        }
    }
    vec!["xdg-open".to_string(), "https://github.com".to_string()]
}

/// A short human label for the configured browser (for the UI tooltip).
pub fn browser_name() -> String {
    browser_command()
        .first()
        .cloned()
        .unwrap_or_else(|| "browser".into())
}

/// Spawn `cmd` as a Wayland client of the compositor, optionally in `cwd`.
/// Returns an error string on failure for the caller to surface as a notification.
pub fn spawn(cmd: &[String], env: &SpawnEnv, cwd: Option<&str>) -> Result<(), String> {
    let Some((program, args)) = cmd.split_first() else {
        return Err("empty command".into());
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .env("WAYLAND_DISPLAY", &env.wayland_display)
        .env("XDG_RUNTIME_DIR", &env.runtime_dir)
        // Prefer the Wayland backend in toolkits that autodetect.
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        .env_remove("DISPLAY");
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    match command.spawn() {
        Ok(_) => Ok(()),
        Err(err) => Err(format!("failed to launch {program}: {err}")),
    }
}

/// Whether a program is on `PATH`.
fn which(program: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

/// Minimal whitespace split for a configured command line (no quoting support).
fn shell_split(s: &str) -> Vec<String> {
    s.split_whitespace().map(|w| w.to_string()).collect()
}
