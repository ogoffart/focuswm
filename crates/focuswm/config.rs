//! App discovery + spawning of client programs (terminal, browser) into the
//! compositor's Wayland socket.

use std::process::Command;

/// Connection info for spawned clients: which Wayland socket to use and where it
/// lives.
#[derive(Clone, Debug, Default)]
pub struct SpawnEnv {
    pub wayland_display: String,
    pub runtime_dir: String,
    /// X11 display number from XWayland (e.g. `:1`), once it is ready.
    pub x_display: Option<u32>,
    /// Address of focuswm's private D-Bus session, if one was started. Clients
    /// run on it so apps that "remote" over the session bus (gnome-terminal,
    /// konsole, Firefox, …) spawn a fresh instance inside focuswm rather than
    /// handing the window to a server already running in the parent session.
    pub dbus_address: Option<String>,
}

/// Start a private D-Bus session daemon for focuswm's clients and return its
/// address. The daemon inherits `wayland_display`/`runtime_dir` so services it
/// activates (e.g. `gnome-terminal-server`) connect to our compositor. The
/// daemon process is returned so the caller can keep it alive for the session;
/// dropping it tears the bus down. Returns `None` if `dbus-daemon` is missing.
pub fn start_private_dbus(
    wayland_display: &str,
    runtime_dir: &str,
) -> Option<(std::process::Child, String)> {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let mut child = Command::new("dbus-daemon")
        .args(["--session", "--print-address", "--nofork", "--nopidfile"])
        .env("WAYLAND_DISPLAY", wayland_display)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| log::warn!("private dbus: could not start dbus-daemon ({e}); clients share the session bus"))
        .ok()?;

    let stdout = child.stdout.take()?;
    let mut reader = BufReader::new(stdout);
    let mut address = String::new();
    if reader.read_line(&mut address).is_err() || address.trim().is_empty() {
        log::warn!("private dbus: dbus-daemon printed no address; clients share the session bus");
        let _ = child.kill();
        let _ = child.wait(); // reap; kill() alone leaves a zombie
        return None;
    }
    let address = address.trim().to_string();
    // Keep draining stdout so the daemon never blocks/SIGPIPEs on a full pipe.
    std::thread::spawn(move || {
        let mut sink = String::new();
        while reader.read_line(&mut sink).map(|n| n > 0).unwrap_or(false) {
            sink.clear();
        }
    });
    log::info!("private dbus: clients use {address}");
    Some((child, address))
}

/// Find a terminal emulator, preferring Wayland-native ones. Returns the program
/// plus any fixed arguments.
pub fn terminal_command() -> Vec<String> {
    if let Ok(t) = std::env::var("FOCUSWM_TERMINAL") {
        return split_command(&t);
    }
    // Wayland-native first, then common X11 terminals (run via XWayland), then
    // the Debian `x-terminal-emulator` alternative. Only pick one that's
    // actually on PATH so we don't try to launch a terminal that isn't there.
    const CANDIDATES: &[&str] = &[
        "alacritty",
        "foot",
        "kitty",
        "wezterm",
        "gnome-terminal",
        "konsole",
        "xfce4-terminal",
        "tilix",
        "st",
        "urxvt",
        "rxvt",
        "x-terminal-emulator",
        "xterm",
    ];
    for cand in CANDIDATES {
        if which(cand) {
            return vec![cand.to_string()];
        }
    }
    // Nothing is installed; fall back to xterm so the failure names a real
    // terminal (the caller surfaces the launch error as a toast).
    vec!["xterm".to_string()]
}

/// Find a web browser.
pub fn browser_command() -> Vec<String> {
    if let Ok(b) = std::env::var("FOCUSWM_BROWSER") {
        return split_command(&b);
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
    // Browsers "remote" a new invocation into an already-running instance; when
    // focuswm runs nested that instance lives in the parent session, so the new
    // window opens there instead of here. Append isolation flags that force a
    // fresh instance bound to a focuswm-private profile.
    let mut args = args.to_vec();
    let isolation = browser_isolation(program, &args);
    args.extend(isolation);

    let mut command = Command::new(program);
    command
        .args(&args)
        .env("WAYLAND_DISPLAY", &env.wayland_display)
        .env("XDG_RUNTIME_DIR", &env.runtime_dir)
        // Prefer the Wayland backend in toolkits that autodetect.
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        // Make Firefox/Thunderbird use our Wayland socket rather than XWayland.
        .env("MOZ_ENABLE_WAYLAND", "1");
    // Route clients onto focuswm's private session bus so they don't remote
    // into servers (gnome-terminal-server, an existing browser, …) running in
    // the parent session.
    if let Some(addr) = &env.dbus_address {
        command.env("DBUS_SESSION_BUS_ADDRESS", addr);
    }
    // Point X11-only apps at our XWayland server. When it isn't up, force an
    // empty DISPLAY so X11 clients fail to connect rather than fall through to
    // the X server of the session focuswm runs in (which would make them open
    // in the parent session). Also clear XAUTHORITY so a stale cookie can't
    // authorize them against that parent X server.
    match env.x_display {
        Some(n) => {
            command.env("DISPLAY", format!(":{n}"));
        }
        None => {
            command.env("DISPLAY", "");
            command.env_remove("XAUTHORITY");
        }
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    log::info!(
        "spawning {program} {args:?} with WAYLAND_DISPLAY={} XDG_RUNTIME_DIR={} DISPLAY={} private-dbus={}",
        env.wayland_display,
        env.runtime_dir,
        match env.x_display {
            Some(n) => format!(":{n}"),
            None => "<unset: XWayland unavailable>".to_string(),
        },
        env.dbus_address.is_some(),
    );
    match command.spawn() {
        Ok(mut child) => {
            // focuswm is the direct parent of every client it launches and
            // runs for the whole session: without a reaper each exited client
            // would linger as a zombie in the process table.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(())
        }
        Err(err) => Err(format!("failed to launch {program}: {err}")),
    }
}

/// Extra arguments that pin a browser to a focuswm-private profile so it starts
/// a fresh instance here instead of forwarding the request to an existing
/// instance in the parent session. Returns empty for non-browsers, or when the
/// caller already supplies its own profile/instance flags.
fn browser_isolation(program: &str, args: &[String]) -> Vec<String> {
    let name = std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program);

    // True if the user already passes any of these flag prefixes themselves.
    let supplied = |flags: &[&str]| {
        args.iter()
            .any(|a| flags.iter().any(|f| a == f || a.starts_with(&format!("{f}="))))
    };

    // A per-browser private profile directory under the data dir, created on
    // demand. None if the data dir is unavailable or can't be created.
    let profile_dir = |sub: &str| -> Option<String> {
        let mut dir = dirs::data_dir()?;
        dir.push("focuswm");
        dir.push("browser-profiles");
        dir.push(sub);
        std::fs::create_dir_all(&dir).ok()?;
        dir.into_os_string().into_string().ok()
    };

    match name {
        "firefox" | "firefox-esr" | "librewolf" | "thunderbird" => {
            if supplied(&["--profile", "-P", "--no-remote", "--new-instance"]) {
                return Vec::new();
            }
            match firefox_profile_dir(name) {
                Some(p) => vec!["--new-instance".into(), "--profile".into(), p],
                None => vec!["--new-instance".into()],
            }
        }
        "chromium" | "chromium-browser" | "google-chrome" | "google-chrome-stable"
        | "brave" | "brave-browser" | "microsoft-edge" => {
            if supplied(&["--user-data-dir"]) {
                return Vec::new();
            }
            profile_dir(name)
                .map(|p| vec![format!("--user-data-dir={p}")])
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// A private Firefox profile directory for focuswm. Snap-confined Firefox can
/// only read/write inside its sandbox, so a profile under `~/.local/share` is
/// invisible to it; when the snap is present, place the profile inside
/// `~/snap/firefox/common` instead. Returns `None` if no home/data dir is
/// available or the directory can't be created.
fn firefox_profile_dir(sub: &str) -> Option<String> {
    let snap_common = dirs::home_dir().map(|h| h.join("snap/firefox/common"));
    let dir = match snap_common {
        Some(base) if base.is_dir() => base.join(".mozilla/firefox/focuswm"),
        _ => dirs::data_dir()?.join("focuswm/browser-profiles").join(sub),
    };
    std::fs::create_dir_all(&dir).ok()?;
    dir.into_os_string().into_string().ok()
}

/// Whether a program is on `PATH`.
fn which(program: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

/// Minimal whitespace split for a configured command line (no quoting support).
pub fn split_command(s: &str) -> Vec<String> {
    s.split_whitespace().map(|w| w.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::browser_isolation;

    #[test]
    fn non_browsers_get_no_extra_args() {
        assert!(browser_isolation("alacritty", &[]).is_empty());
        assert!(browser_isolation("/usr/bin/htop", &[]).is_empty());
    }

    #[test]
    fn firefox_is_forced_into_a_fresh_private_instance() {
        // Resolved by basename, so an absolute path still matches.
        let args = browser_isolation("/usr/bin/firefox", &[]);
        assert!(args.contains(&"--new-instance".to_string()));
    }

    #[test]
    fn chromium_gets_a_dedicated_user_data_dir() {
        let args = browser_isolation("chromium", &[]);
        assert!(args.iter().any(|a| a.starts_with("--user-data-dir")));
    }

    #[test]
    fn user_supplied_profile_flags_are_respected() {
        let firefox = browser_isolation("firefox", &["-P".into(), "work".into()]);
        assert!(firefox.is_empty());
        let chromium =
            browser_isolation("chromium", &["--user-data-dir=/tmp/x".into()]);
        assert!(chromium.is_empty());
    }
}
