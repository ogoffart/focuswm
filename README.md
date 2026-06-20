# focuswm

A task-focused **Wayland compositor** written in Rust, using
[Slint](https://slint.dev) for the interface and
[Smithay](https://smithay.github.io/) for the Wayland protocol.

Instead of generic numbered workspaces, **every virtual desktop is one "task"**.
A sidebar lets you create tasks, reorder them, and see which ones have
notifications. focuswm tracks how much time you spend on each task, auto-opens a
terminal in every new task, and can launch a browser — to help you stay focused
and remember what's on your plate.

## Status

This is the **foundation milestone**. Working today:

- A nested Wayland compositor (Slint owns rendering/input/event-loop, Smithay is
  the protocol engine on its own thread).
- Dynamic, named **task-desktops**: create (via a wizard), switch, delete and
  reorder them from the sidebar.
- A creation **wizard** collecting name, category, branch and origin repo.
- **Auto-spawn a terminal** in each new task, plus an "Open browser" button.
- Per-task **time tracking** (focused time, accumulated and persisted to JSON).
- Sidebar **notification indicators**.

Planned next: git worktree creation from the branch/repo, GitHub issue/PR linking
with comment notifications, drag-and-drop reordering, on-screen notification
toasts, dmabuf import and bare-metal (linuxkms) output.

## Architecture

```
┌──────────── main thread (Slint) ─────────────┐
│ Sidebar + wizard + task view (ui/*.slint)    │
│ rendering-notifier imports client buffers     │
│ -> GL textures -> Slint Image tiles           │
│ time-tracking + JSON persistence              │
└───────────▲───────────────────────┬──────────┘
     Event channel (mpsc)     Command channel (calloop)
┌───────────┴───────────────────────▼──────────┐
│ wayland thread (Smithay + calloop)            │
│ wl_compositor / xdg-shell / shm / seat        │
└───────────────────────────────────────────────┘
```

| Crate | Role |
|-------|------|
| `focuswm` | The binary: wires the Slint UI and the Wayland thread together; owns time tracking, persistence, GL upload. |
| `focuswm-wayland` | Smithay protocol engine (no rendering). Unit-testable logic. |
| `focuswm-render` | Pure `wl_shm` → RGBA pixel conversion + compositing. |
| `focuswm-shell` | Plain-Rust data models (windows, tasks, time accumulation). |

The Slint UI lives in `crates/focuswm/ui/`.

## Building

You need a recent Rust toolchain and a few system libraries (Skia is *not* used;
the FemtoVG/GL renderer keeps the build light):

```sh
sudo apt install build-essential clang libxkbcommon-dev libfontconfig-dev \
    libudev-dev libgbm-dev libdrm-dev
```

Slint is pinned to the **master** branch (Slint 1.17) as a git dependency.

```sh
cargo build
cargo test          # unit tests, no display/GPU required
cargo run           # opens a nested compositor window
```

When running nested, focuswm prints the `WAYLAND_DISPLAY` it created; apps
launched from a task connect to it automatically.

## Keyboard shortcuts

Global shortcuts are intercepted by the compositor before keys reach the focused
client:

| Shortcut | Action |
|----------|--------|
| `Alt`+`Tab` / `Alt`+`Shift`+`Tab` | Cycle windows in the active task |
| `Super`+`1`…`9` | Switch to task N |
| `Super`+`Return` | Open a terminal |
| `Super`+`N` | New task (open the wizard) |
| `Super`+`W` | Close the focused window |
| `Super`+`L` | Lock the screen |


### Headless UI preview

```sh
cargo run -p focuswm --example shell_screenshot   # writes shot_desktop.png
```

## License

Licensed under the **GNU General Public License v3.0 or later**. See
[`LICENSE`](LICENSE).
