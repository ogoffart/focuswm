//! Tasks — the focuswm unit of organization.
//!
//! Each task is one virtual desktop. A [`TaskList`] owns the ordered set of
//! tasks, the mapping of client windows to the task they belong to, which task
//! is currently active, and the per-task accumulated focus time.
//!
//! All time bookkeeping is driven by an externally supplied monotonic timestamp
//! (seconds) so the logic is deterministic and unit-testable without a real
//! clock: the binary crate passes `Instant`-derived seconds, tests pass fixed
//! values.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::model::WindowId;

/// Stable identifier for a task / virtual desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TaskId(pub u64);

/// A single task: one virtual desktop with its metadata and accumulated time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub name: String,
    /// Category, used to group time-tracking reports.
    pub category: String,
    /// Accent colour for the sidebar, as a "#rrggbb" hex string. Assigned from
    /// [`task_palette`] on creation; empty for tasks persisted before colours
    /// existed (the UI falls back to a palette colour in that case).
    #[serde(default)]
    pub color: String,
    /// Branch name requested in the wizard (drives the git worktree).
    #[serde(default)]
    pub branch: Option<String>,
    /// Origin repository url requested in the wizard.
    #[serde(default)]
    pub repo: Option<String>,
    /// Path of the git worktree created for this task, once it exists.
    #[serde(default)]
    pub worktree_path: Option<String>,
    /// Total focused time, in seconds, persisted across runs.
    #[serde(default)]
    pub accumulated_secs: u64,
    /// Whether this task has a pending notification (shown in the sidebar).
    #[serde(default, skip)]
    pub has_notification: bool,
    /// A linked GitHub issue or pull request whose activity drives the task's
    /// notification dot. `None` when the task isn't linked to anything.
    #[serde(default)]
    pub github: Option<GithubLink>,
}

/// A GitHub issue or pull request linked to a task. New activity on it (a newer
/// `updated_at` than `last_seen`) raises the task's notification dot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubLink {
    /// `owner/repo`.
    pub slug: String,
    /// Issue or PR number.
    pub number: u64,
    /// Title, for display.
    pub title: String,
    /// Web URL, opened when the user acts on the notification.
    pub url: String,
    /// `updated_at` (epoch seconds) last seen by a poll; `None` until first seen.
    #[serde(default)]
    pub last_seen: Option<i64>,
}

/// Parse a GitHub `owner/repo` slug from a remote URL or a bare `owner/repo`.
/// Handles `https://github.com/owner/repo(.git)`, `git@github.com:owner/repo.git`
/// and `owner/repo`. Returns `None` for anything that isn't GitHub-shaped.
pub fn parse_slug(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    // Strip the transport/host prefix for the URL forms.
    let rest = if let Some(r) = s.strip_prefix("https://github.com/") {
        r
    } else if let Some(r) = s.strip_prefix("http://github.com/") {
        r
    } else if let Some(r) = s.strip_prefix("git@github.com:") {
        r
    } else if let Some(r) = s.strip_prefix("ssh://git@github.com/") {
        r
    } else if !s.contains("://") && !s.contains('@') {
        // Bare `owner/repo`.
        s
    } else {
        return None;
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.splitn(3, '/');
    let owner = parts.next().filter(|p| !p.is_empty())?;
    let repo = parts.next().filter(|p| !p.is_empty())?;
    Some((owner.to_string(), repo.to_string()))
}

impl Task {
    pub fn new(id: TaskId, name: impl Into<String>, category: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            category: category.into(),
            color: String::new(),
            branch: None,
            repo: None,
            worktree_path: None,
            accumulated_secs: 0,
            has_notification: false,
            github: None,
        }
    }
}

/// The ordered set of tasks plus window assignment, active task and time
/// accumulation.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TaskList {
    /// Tasks in sidebar order.
    tasks: Vec<Task>,
    /// window -> the desktop it belongs to. `None` means desktop 0, the scratch
    /// desktop that isn't tied to any task.
    #[serde(default, skip)]
    window_task: HashMap<WindowId, Option<TaskId>>,
    /// The currently active (focused) desktop: `Some(task)` for a task, or
    /// `None` for desktop 0 (the scratch desktop). Desktop 0 is the default.
    #[serde(default, skip)]
    active: Option<TaskId>,
    /// Timestamp (seconds) at which the active task became active; `None` when
    /// desktop 0 is active or time tracking hasn't started. No time accrues on
    /// desktop 0.
    #[serde(default, skip)]
    active_since: Option<u64>,
    /// Windows the user has minimized (hidden from the content area but still
    /// listed in the sidebar so they can be restored).
    #[serde(default, skip)]
    minimized: HashSet<WindowId>,
    /// Windows the user has maximized (the client has been told it is maximized).
    #[serde(default, skip)]
    maximized: HashSet<WindowId>,
    /// Next task id to hand out.
    #[serde(default)]
    next_id: u64,
    /// Filesystem paths previously entered as a task's origin repo, most-recent
    /// first, for the wizard's dropdown. Persisted.
    #[serde(default)]
    repo_history: Vec<String>,
    /// Per-day, per-task time entries, for reporting. Persisted.
    #[serde(default)]
    time_log: TimeLog,
    /// User settings (terminal/browser commands, categories). Persisted.
    #[serde(default)]
    settings: Settings,
    /// Snapshot of the running apps (per desktop) for session restore, updated
    /// while running and on exit; respawned on the next start. Persisted.
    #[serde(default)]
    session: Vec<SessionApp>,
    /// The current local calendar day ("YYYY-MM-DD"), supplied by the host via
    /// [`set_date`]; used to attribute committed intervals in the time log.
    #[serde(default, skip)]
    current_date: String,
}

/// One running app in the session snapshot: its command line, working
/// directory, and the desktop its window was on (`None` = desktop 0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionApp {
    pub task: Option<TaskId>,
    /// The client process's argv, read from /proc at window creation.
    pub cmd: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl TaskList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn active(&self) -> Option<TaskId> {
        self.active
    }

    pub fn get(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn get_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    fn index_of(&self, id: TaskId) -> Option<usize> {
        self.tasks.iter().position(|t| t.id == id)
    }

    /// Create a new task, appended at the end of the sidebar order, and return
    /// its id. Does not change the active task.
    pub fn add_task(&mut self, name: impl Into<String>, category: impl Into<String>) -> TaskId {
        let id = TaskId(self.next_id);
        self.next_id += 1;
        let mut task = Task::new(id, name, category);
        let palette = task_palette();
        task.color = palette[(id.0 as usize) % palette.len()].clone();
        self.tasks.push(task);
        id
    }

    /// Update an existing task's user-editable properties (name, category,
    /// colour). No-op for an unknown id.
    pub fn set_task_props(
        &mut self,
        id: TaskId,
        name: impl Into<String>,
        category: impl Into<String>,
        color: impl Into<String>,
    ) {
        if let Some(task) = self.get_mut(id) {
            task.name = name.into();
            task.category = category.into();
            task.color = color.into();
        }
    }

    /// Remove a task and any windows assigned to it. If it was active, the
    /// active desktop falls back to desktop 0; the supplied `now` flushes the
    /// running interval first.
    pub fn remove_task(&mut self, id: TaskId, now: u64) {
        if self.active == Some(id) {
            self.flush(now);
            self.active = None;
            self.active_since = None;
        }
        self.tasks.retain(|t| t.id != id);
        self.window_task.retain(|_, t| *t != Some(id));
    }

    /// Move the task at `from` to position `to` (both clamped). No-op if out of
    /// range or equal.
    pub fn reorder(&mut self, from: usize, to: usize) {
        if from >= self.tasks.len() {
            return;
        }
        let to = to.min(self.tasks.len() - 1);
        if from == to {
            return;
        }
        let task = self.tasks.remove(from);
        self.tasks.insert(to, task);
    }

    /// Switch the active task, accumulating the elapsed focus time onto the
    /// previously active task. `now` is a monotonic timestamp in seconds.
    pub fn set_active(&mut self, id: TaskId, now: u64) {
        if self.index_of(id).is_none() {
            return;
        }
        if self.active == Some(id) {
            return;
        }
        self.flush(now);
        self.active = Some(id);
        self.active_since = Some(now);
        // Switching to a task clears its notification.
        if let Some(task) = self.get_mut(id) {
            task.has_notification = false;
        }
    }

    /// Switch to desktop 0 — the scratch desktop with no associated task.
    /// Flushes the running interval onto the previously active task first; no
    /// time accrues while desktop 0 is active.
    pub fn set_scratch_active(&mut self, now: u64) {
        if self.active.is_none() {
            return;
        }
        self.flush(now);
        self.active = None;
        self.active_since = None;
    }

    /// Pause time accrual (e.g. when the user goes idle): subsequent flushes add
    /// nothing until [`resume`](Self::resume) is called. The caller should
    /// [`flush`](Self::flush) up to the moment activity stopped first.
    pub fn pause(&mut self) {
        self.active_since = None;
    }

    /// Resume time accrual after a pause, counting from `now`.
    pub fn resume(&mut self, now: u64) {
        if self.active.is_some() && self.active_since.is_none() {
            self.active_since = Some(now);
        }
    }

    /// Whether time accrual is currently paused (a task is active but not being
    /// counted).
    pub fn is_paused(&self) -> bool {
        self.active.is_some() && self.active_since.is_none()
    }

    /// Flush the running focus interval into the active task without switching,
    /// e.g. on a periodic persist tick. `now` is seconds.
    pub fn flush(&mut self, now: u64) {
        if let (Some(active), Some(since)) = (self.active, self.active_since) {
            let delta = now.saturating_sub(since);
            if delta > 0 {
                self.commit_interval(active, delta);
                self.active_since = Some(now);
            }
        }
    }

    /// Add `secs` of focus time to `task_id`: both its lifetime total and the
    /// per-day time log (under the current date set via [`set_date`]).
    fn commit_interval(&mut self, task_id: TaskId, secs: u64) {
        if secs == 0 {
            return;
        }
        let (project, category) = self
            .get(task_id)
            .map(|t| (t.name.clone(), t.category.clone()))
            .unwrap_or_default();
        if let Some(task) = self.get_mut(task_id) {
            task.accumulated_secs += secs;
        }
        let date = self.current_date.clone();
        self.time_log
            .record(&date, task_id, &project, &category, secs);
    }

    /// Set the current local calendar day used to tag time-log entries
    /// ("YYYY-MM-DD"). The host updates this from its real clock.
    pub fn set_date(&mut self, today: &str) {
        self.current_date = today.to_string();
    }

    /// The persisted time log, for reporting.
    pub fn time_log(&self) -> &TimeLog {
        &self.time_log
    }

    /// The user settings.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Replace the user settings.
    pub fn set_settings(&mut self, settings: Settings) {
        self.settings = settings;
    }

    /// Replace the session snapshot (the running apps to restore on next start).
    pub fn set_session(&mut self, apps: Vec<SessionApp>) {
        self.session = apps;
    }

    /// Take the persisted session snapshot for restoring, leaving it empty (it
    /// is rebuilt from the live windows as they map).
    pub fn take_session(&mut self) -> Vec<SessionApp> {
        std::mem::take(&mut self.session)
    }

    /// Assign a newly mapped window to the active desktop (the active task, or
    /// desktop 0 when none is active). Returns the task it was assigned to, or
    /// `None` for desktop 0.
    pub fn assign_window(&mut self, window: WindowId) -> Option<TaskId> {
        self.window_task.insert(window, self.active);
        self.active
    }

    /// Forget a window that has been unmapped.
    pub fn remove_window(&mut self, window: WindowId) {
        self.window_task.remove(&window);
        self.minimized.remove(&window);
        self.maximized.remove(&window);
    }

    /// Move a window to another desktop: `Some(task)` for a task (which must
    /// exist), or `None` for desktop 0 (the scratch desktop). No-op when the
    /// target task is unknown.
    pub fn move_window_to(&mut self, window: WindowId, target: Option<TaskId>) {
        if let Some(id) = target {
            if self.index_of(id).is_none() {
                return;
            }
        }
        self.window_task.insert(window, target);
    }

    /// Whether a window is minimized (hidden from the content area).
    pub fn is_minimized(&self, window: WindowId) -> bool {
        self.minimized.contains(&window)
    }

    /// Set a window's minimized state.
    pub fn set_minimized(&mut self, window: WindowId, minimized: bool) {
        if minimized {
            self.minimized.insert(window);
        } else {
            self.minimized.remove(&window);
        }
    }

    /// Whether a window is maximized (the client has been told so).
    pub fn is_maximized(&self, window: WindowId) -> bool {
        self.maximized.contains(&window)
    }

    /// Set a window's maximized state.
    pub fn set_maximized(&mut self, window: WindowId, maximized: bool) {
        if maximized {
            self.maximized.insert(window);
        } else {
            self.maximized.remove(&window);
        }
    }

    /// The task a window belongs to, or `None` if it's on desktop 0 or unknown.
    pub fn task_of_window(&self, window: WindowId) -> Option<TaskId> {
        self.window_task.get(&window).copied().flatten()
    }

    /// Windows assigned to `desktop` (`None` = desktop 0). Ordering is by window
    /// id, ascending.
    fn windows_on(&self, desktop: Option<TaskId>) -> Vec<WindowId> {
        let mut v: Vec<WindowId> = self
            .window_task
            .iter()
            .filter(|(_, d)| **d == desktop)
            .map(|(w, _)| *w)
            .collect();
        v.sort();
        v
    }

    /// Windows belonging to a task. Ordering is by window id, ascending.
    pub fn windows_for(&self, id: TaskId) -> Vec<WindowId> {
        self.windows_on(Some(id))
    }

    /// Windows on desktop 0 (the scratch desktop).
    pub fn scratch_windows(&self) -> Vec<WindowId> {
        self.windows_on(None)
    }

    /// Windows on the active desktop (the ones that should be visible).
    pub fn active_windows(&self) -> Vec<WindowId> {
        self.windows_on(self.active)
    }

    /// Whether a window is on the active desktop (i.e. currently visible).
    pub fn is_visible(&self, window: WindowId) -> bool {
        self.window_task.get(&window).copied() == Some(self.active)
    }

    /// Mark a task as having a pending notification (no-op for the active task,
    /// which is already being looked at).
    pub fn notify(&mut self, id: TaskId) {
        if self.active == Some(id) {
            return;
        }
        if let Some(task) = self.get_mut(id) {
            task.has_notification = true;
        }
    }

    /// Rebuild the volatile state (ids counter) after loading from disk, so new
    /// task ids don't collide with persisted ones.
    pub fn reindex_after_load(&mut self) {
        self.next_id = self.tasks.iter().map(|t| t.id.0 + 1).max().unwrap_or(0);
    }

    /// Previously entered origin-repo paths, most-recent first.
    pub fn repo_history(&self) -> &[String] {
        &self.repo_history
    }

    /// Record an entered origin-repo path: de-duplicate and move it to the front,
    /// keeping the list bounded.
    pub fn record_repo(&mut self, repo: &str) {
        if repo.is_empty() {
            return;
        }
        self.repo_history.retain(|r| r != repo);
        self.repo_history.insert(0, repo.to_string());
        self.repo_history.truncate(20);
    }
}

/// One day's accumulated focus time for one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeEntry {
    /// Local calendar day, "YYYY-MM-DD".
    pub date: String,
    pub task_id: TaskId,
    /// Task name at the time, denormalized so reports survive task edits/deletes.
    pub project: String,
    pub category: String,
    pub secs: u64,
}

/// A log of per-day, per-task focus time, with one row per (date, task).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TimeLog {
    entries: Vec<TimeEntry>,
}

impl TimeLog {
    pub fn entries(&self) -> &[TimeEntry] {
        &self.entries
    }

    /// Add `secs` under `(date, task_id)`, merging with an existing row.
    pub fn record(&mut self, date: &str, task_id: TaskId, project: &str, category: &str, secs: u64) {
        if secs == 0 || date.is_empty() {
            return;
        }
        if let Some(e) = self
            .entries
            .iter_mut()
            .find(|e| e.date == date && e.task_id == task_id)
        {
            e.secs += secs;
            e.project = project.to_string();
            e.category = category.to_string();
        } else {
            self.entries.push(TimeEntry {
                date: date.to_string(),
                task_id,
                project: project.to_string(),
                category: category.to_string(),
                secs,
            });
        }
    }

    /// Aggregate seconds within the inclusive date range `[since, until]`
    /// (lexicographic comparison works for "YYYY-MM-DD"). Returns per-category
    /// and per-project totals (each sorted by time, descending) and the grand
    /// total.
    pub fn aggregate(&self, since: &str, until: &str) -> Aggregate {
        let mut by_category: HashMap<String, u64> = HashMap::new();
        let mut by_project: HashMap<String, u64> = HashMap::new();
        let mut total = 0u64;
        for e in &self.entries {
            if e.date.as_str() < since || e.date.as_str() > until {
                continue;
            }
            *by_category.entry(e.category.clone()).or_default() += e.secs;
            *by_project.entry(e.project.clone()).or_default() += e.secs;
            total += e.secs;
        }
        Aggregate {
            by_category: sorted_desc(by_category),
            by_project: sorted_desc(by_project),
            total,
        }
    }

    /// Per-day totals within `[since, until]`, sorted by date ascending.
    pub fn daily_totals(&self, since: &str, until: &str) -> Vec<(String, u64)> {
        let mut by_day: HashMap<String, u64> = HashMap::new();
        for e in &self.entries {
            if e.date.as_str() < since || e.date.as_str() > until {
                continue;
            }
            *by_day.entry(e.date.clone()).or_default() += e.secs;
        }
        let mut v: Vec<(String, u64)> = by_day.into_iter().collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

/// Aggregated report figures over a date range.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Aggregate {
    /// (category, seconds), sorted by seconds descending.
    pub by_category: Vec<(String, u64)>,
    /// (project, seconds), sorted by seconds descending.
    pub by_project: Vec<(String, u64)>,
    pub total: u64,
}

fn sorted_desc(map: HashMap<String, u64>) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// User settings, persisted with the task list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Terminal command (empty = auto-detect).
    #[serde(default)]
    pub terminal: String,
    /// Browser command (empty = auto-detect).
    #[serde(default)]
    pub browser: String,
    /// Categories offered in the wizard.
    #[serde(default = "default_categories")]
    pub categories: Vec<String>,
    /// Minutes of no input after which time tracking pauses (0 = never).
    #[serde(default = "default_idle_minutes")]
    pub idle_minutes: u64,
    /// Whether hovering a window gives it keyboard focus (focus-follows-mouse).
    #[serde(default = "default_focus_follows_mouse")]
    pub focus_follows_mouse: bool,
    /// GitHub personal-access token for the issue/PR integration (empty = fall
    /// back to the `GITHUB_TOKEN` environment variable). Stored in the plain
    /// JSON config file.
    #[serde(default)]
    pub github_token: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            terminal: String::new(),
            browser: String::new(),
            categories: default_categories(),
            idle_minutes: default_idle_minutes(),
            focus_follows_mouse: default_focus_follows_mouse(),
            github_token: String::new(),
        }
    }
}

/// Default for focus-follows-mouse: on.
pub fn default_focus_follows_mouse() -> bool {
    true
}

/// Default idle timeout, in minutes.
pub fn default_idle_minutes() -> u64 {
    5
}

/// The palette of accent colours offered for tasks (Catppuccin Mocha hues,
/// matching the dark theme). New tasks are assigned one round-robin by id, and
/// the task-settings dialog offers these as swatches.
pub fn task_palette() -> Vec<String> {
    [
        "#89b4fa", // blue
        "#a6e3a1", // green
        "#f9e2af", // yellow
        "#f38ba8", // red
        "#cba6f7", // mauve
        "#fab387", // peach
        "#94e2d5", // teal
        "#f5c2e7", // pink
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The built-in default category list.
pub fn default_categories() -> Vec<String> {
    ["work", "personal", "meeting", "learning", "other"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slug_handles_url_forms() {
        let want = Some(("ogoffart".to_string(), "focuswm".to_string()));
        assert_eq!(parse_slug("https://github.com/ogoffart/focuswm"), want);
        assert_eq!(parse_slug("https://github.com/ogoffart/focuswm.git"), want);
        assert_eq!(parse_slug("https://github.com/ogoffart/focuswm/"), want);
        assert_eq!(parse_slug("git@github.com:ogoffart/focuswm.git"), want);
        assert_eq!(parse_slug("ssh://git@github.com/ogoffart/focuswm.git"), want);
        assert_eq!(parse_slug("ogoffart/focuswm"), want);
        // Non-GitHub or malformed inputs yield nothing.
        assert_eq!(parse_slug("https://gitlab.com/ogoffart/focuswm"), None);
        assert_eq!(parse_slug("/home/olivier/code/focuswm"), None);
        assert_eq!(parse_slug("ogoffart"), None);
    }

    #[test]
    fn move_window_to_reassigns_desktop() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        let b = list.add_task("B", "work");
        list.set_active(a, 0);
        let w = WindowId(1);
        list.assign_window(w); // lands on the active task, A
        assert_eq!(list.task_of_window(w), Some(a));
        // Move to B, then to desktop 0.
        list.move_window_to(w, Some(b));
        assert_eq!(list.task_of_window(w), Some(b));
        list.move_window_to(w, None);
        assert_eq!(list.task_of_window(w), None);
        // Moving to an unknown task is a no-op.
        list.move_window_to(w, Some(TaskId(999)));
        assert_eq!(list.task_of_window(w), None);
    }

    #[test]
    fn add_assigns_increasing_ids() {
        let mut list = TaskList::new();
        let a = list.add_task("Fix bug", "work");
        let b = list.add_task("Write docs", "work");
        assert_eq!(a, TaskId(0));
        assert_eq!(b, TaskId(1));
        assert_eq!(list.tasks().len(), 2);
    }

    #[test]
    fn switching_accumulates_time_on_previous_task() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        let b = list.add_task("B", "work");
        list.set_active(a, 0);
        // 100s on A, then switch to B.
        list.set_active(b, 100);
        assert_eq!(list.get(a).unwrap().accumulated_secs, 100);
        assert_eq!(list.get(b).unwrap().accumulated_secs, 0);
        // 30s on B, then back to A.
        list.set_active(a, 130);
        assert_eq!(list.get(b).unwrap().accumulated_secs, 30);
        assert_eq!(list.active(), Some(a));
    }

    #[test]
    fn flush_accumulates_without_switching() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        list.set_active(a, 10);
        list.flush(40);
        assert_eq!(list.get(a).unwrap().accumulated_secs, 30);
        // A subsequent flush only counts time since the last flush.
        list.flush(50);
        assert_eq!(list.get(a).unwrap().accumulated_secs, 40);
    }

    #[test]
    fn windows_follow_active_task() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        let b = list.add_task("B", "work");
        list.set_active(a, 0);
        list.assign_window(WindowId(1));
        list.assign_window(WindowId(2));
        list.set_active(b, 5);
        list.assign_window(WindowId(3));

        assert_eq!(list.windows_for(a), vec![WindowId(1), WindowId(2)]);
        assert_eq!(list.windows_for(b), vec![WindowId(3)]);
        assert_eq!(list.active_windows(), vec![WindowId(3)]);
        assert!(list.is_visible(WindowId(3)));
        assert!(!list.is_visible(WindowId(1)));
    }

    #[test]
    fn new_tasks_get_a_palette_colour_and_props_can_be_edited() {
        let mut list = TaskList::new();
        let palette = task_palette();
        let a = list.add_task("A", "work");
        let b = list.add_task("B", "work");
        // Colours are assigned round-robin by id, so the first two differ.
        assert_eq!(list.get(a).unwrap().color, palette[0]);
        assert_eq!(list.get(b).unwrap().color, palette[1]);
        // Editing replaces name, category and colour together.
        list.set_task_props(a, "Renamed", "personal", "#ffffff");
        let t = list.get(a).unwrap();
        assert_eq!(t.name, "Renamed");
        assert_eq!(t.category, "personal");
        assert_eq!(t.color, "#ffffff");
        // Unknown id is a no-op.
        list.set_task_props(TaskId(999), "x", "y", "z");
    }

    #[test]
    fn minimized_and_maximized_state_tracks_and_clears_on_unmap() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        list.set_active(a, 0);
        list.assign_window(WindowId(1));
        assert!(!list.is_minimized(WindowId(1)));
        list.set_minimized(WindowId(1), true);
        list.set_maximized(WindowId(1), true);
        assert!(list.is_minimized(WindowId(1)));
        assert!(list.is_maximized(WindowId(1)));
        list.set_minimized(WindowId(1), false);
        assert!(!list.is_minimized(WindowId(1)));
        // Unmapping the window clears any leftover state.
        list.set_minimized(WindowId(1), true);
        list.remove_window(WindowId(1));
        assert!(!list.is_minimized(WindowId(1)));
        assert!(!list.is_maximized(WindowId(1)));
    }

    #[test]
    fn reorder_moves_tasks() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        let b = list.add_task("B", "work");
        let c = list.add_task("C", "work");
        list.reorder(0, 2); // move A to the end
        let order: Vec<TaskId> = list.tasks().iter().map(|t| t.id).collect();
        assert_eq!(order, vec![b, c, a]);
        list.reorder(99, 0); // out of range -> no-op
        let order2: Vec<TaskId> = list.tasks().iter().map(|t| t.id).collect();
        assert_eq!(order2, vec![b, c, a]);
    }

    #[test]
    fn removing_active_task_falls_back_to_desktop0_and_drops_windows() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        let _b = list.add_task("B", "work");
        list.set_active(a, 0);
        list.assign_window(WindowId(1));
        list.remove_task(a, 50);
        assert_eq!(list.get(a), None);
        assert_eq!(list.task_of_window(WindowId(1)), None);
        // Falls back to desktop 0 rather than auto-selecting another task.
        assert_eq!(list.active(), None);
    }

    #[test]
    fn desktop0_is_default_and_holds_its_own_windows() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        // A fresh list starts on desktop 0 (no active task).
        assert_eq!(list.active(), None);
        // Windows opened on desktop 0 stay there and are visible.
        list.assign_window(WindowId(1));
        assert_eq!(list.scratch_windows(), vec![WindowId(1)]);
        assert_eq!(list.active_windows(), vec![WindowId(1)]);
        assert!(list.is_visible(WindowId(1)));

        // Switching to a task hides desktop 0's windows; new ones go to the task.
        list.set_active(a, 10);
        list.assign_window(WindowId(2));
        assert!(!list.is_visible(WindowId(1)));
        assert!(list.is_visible(WindowId(2)));
        assert_eq!(list.windows_for(a), vec![WindowId(2)]);

        // Back to desktop 0: its window is visible again, the task's is hidden.
        list.set_scratch_active(20);
        assert_eq!(list.active(), None);
        assert_eq!(list.active_windows(), vec![WindowId(1)]);
        assert!(!list.is_visible(WindowId(2)));
    }

    #[test]
    fn desktop0_does_not_accrue_time() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        list.set_active(a, 0);
        list.set_scratch_active(100); // 100s counted onto A, then desktop 0
        assert_eq!(list.get(a).unwrap().accumulated_secs, 100);
        // Time on desktop 0 is not tracked.
        list.flush(400);
        assert_eq!(list.get(a).unwrap().accumulated_secs, 100);
        assert!(!list.is_paused());
    }

    #[test]
    fn notify_skips_active_task_and_clears_on_switch() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        let b = list.add_task("B", "work");
        list.set_active(a, 0);
        list.notify(a); // active -> ignored
        assert!(!list.get(a).unwrap().has_notification);
        list.notify(b);
        assert!(list.get(b).unwrap().has_notification);
        list.set_active(b, 10); // looking at B clears it
        assert!(!list.get(b).unwrap().has_notification);
    }

    #[test]
    fn time_log_records_per_day_per_task_on_flush() {
        let mut list = TaskList::new();
        let a = list.add_task("Fix bug", "work");
        let b = list.add_task("Docs", "writing");
        list.set_date("2024-03-01");
        list.set_active(a, 0);
        list.set_active(b, 100); // 100s -> A on 2024-03-01
        list.set_date("2024-03-02");
        list.flush(160); // 60s -> B on 2024-03-02

        let day1 = list.time_log().aggregate("2024-03-01", "2024-03-01");
        assert_eq!(day1.total, 100);
        assert_eq!(day1.by_category, vec![("work".to_string(), 100)]);
        assert_eq!(day1.by_project, vec![("Fix bug".to_string(), 100)]);

        let week = list.time_log().aggregate("2024-03-01", "2024-03-07");
        assert_eq!(week.total, 160);
        // Sorted by time descending: work(100) before writing(60).
        assert_eq!(
            week.by_category,
            vec![("work".to_string(), 100), ("writing".to_string(), 60)]
        );
    }

    #[test]
    fn daily_totals_are_sorted_by_date() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        list.set_date("2024-03-02");
        list.set_active(a, 0);
        list.flush(50);
        list.set_date("2024-03-01");
        list.flush(80); // 30s on the earlier date
        let daily = list.time_log().daily_totals("2024-03-01", "2024-03-31");
        assert_eq!(
            daily,
            vec![
                ("2024-03-01".to_string(), 30),
                ("2024-03-02".to_string(), 50)
            ]
        );
    }

    #[test]
    fn aggregate_excludes_out_of_range_days() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        list.set_date("2024-03-10");
        list.set_active(a, 0);
        list.flush(120); // 2 min on 2024-03-10
        list.set_date("2024-03-20");
        list.flush(180); // 1 min on 2024-03-20
        // Range covering only the first day.
        let agg = list.time_log().aggregate("2024-03-01", "2024-03-15");
        assert_eq!(agg.total, 120);
        // Empty range.
        let none = list.time_log().aggregate("2025-01-01", "2025-12-31");
        assert_eq!(none.total, 0);
        assert!(none.by_category.is_empty());
    }

    #[test]
    fn default_idle_minutes_is_five() {
        assert_eq!(super::default_idle_minutes(), 5);
        assert_eq!(Settings::default().idle_minutes, 5);
    }

    #[test]
    fn pause_stops_accrual_and_resume_restarts_it() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        list.set_active(a, 0);
        list.flush(100); // 100s active
        // Go idle: flush up to last activity (100), then pause.
        list.pause();
        assert!(list.is_paused());
        list.flush(500); // idle window — counts nothing
        assert_eq!(list.get(a).unwrap().accumulated_secs, 100);
        // Resume and accrue again.
        list.resume(500);
        list.flush(560);
        assert_eq!(list.get(a).unwrap().accumulated_secs, 160);
    }

    #[test]
    fn settings_round_trip_and_defaults() {
        let list = TaskList::new();
        assert!(list.settings().categories.contains(&"work".to_string()));
        // Focus-follows-mouse defaults on (and old configs without the field
        // deserialize to the same default via serde).
        assert!(list.settings().focus_follows_mouse);
        assert!(Settings::default().focus_follows_mouse);
        let mut list = list;
        // A GitHub token isn't configured by default.
        assert!(Settings::default().github_token.is_empty());
        list.set_settings(Settings {
            terminal: "foot".into(),
            browser: "firefox".into(),
            categories: vec!["x".into()],
            idle_minutes: 10,
            focus_follows_mouse: false,
            github_token: "ghp_test".into(),
        });
        assert_eq!(list.settings().terminal, "foot");
        assert_eq!(list.settings().categories, vec!["x".to_string()]);
        assert!(!list.settings().focus_follows_mouse);
        assert_eq!(list.settings().github_token, "ghp_test");
    }

    #[test]
    fn session_snapshot_persists_and_takes_once() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        list.set_session(vec![
            SessionApp { task: Some(a), cmd: vec!["foot".into()], cwd: Some("/tmp".into()) },
            SessionApp { task: None, cmd: vec!["firefox".into()], cwd: None },
        ]);
        // Survives serialization (the persist path) including the task ids.
        let json = serde_json::to_string(&list).unwrap();
        let mut loaded: TaskList = serde_json::from_str(&json).unwrap();
        let apps = loaded.take_session();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].task, Some(a));
        assert_eq!(apps[0].cmd, vec!["foot".to_string()]);
        assert_eq!(apps[1].task, None);
        // Taking leaves it empty (it's rebuilt from live windows afterwards).
        assert!(loaded.take_session().is_empty());
        // Old persisted files without the field deserialize to no session.
        let bare: TaskList = serde_json::from_str(r#"{"tasks":[]}"#).unwrap();
        assert!(bare.session.is_empty());
    }

    #[test]
    fn repo_history_dedups_and_orders_most_recent_first() {
        let mut list = TaskList::new();
        list.record_repo("/home/me/a");
        list.record_repo("/home/me/b");
        list.record_repo("/home/me/a"); // re-entered -> moves to front
        assert_eq!(list.repo_history(), &["/home/me/a", "/home/me/b"]);
        list.record_repo(""); // ignored
        assert_eq!(list.repo_history().len(), 2);
    }

    #[test]
    fn reindex_after_load_avoids_id_collisions() {
        let mut list = TaskList::new();
        list.add_task("A", "work");
        list.add_task("B", "work");
        // Simulate a load that reset next_id but kept tasks.
        list.next_id = 0;
        list.reindex_after_load();
        let c = list.add_task("C", "work");
        assert_eq!(c, TaskId(2));
    }

    #[test]
    fn github_link_persists_through_json() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        list.get_mut(a).unwrap().github = Some(GithubLink {
            slug: "ogoffart/focuswm".into(),
            number: 42,
            title: "Fix it".into(),
            url: "https://github.com/ogoffart/focuswm/pull/42".into(),
            last_seen: Some(1_700_000_000),
        });
        let json = serde_json::to_string(&list).unwrap();
        let back: TaskList = serde_json::from_str(&json).unwrap();
        let link = back.get(a).unwrap().github.as_ref().unwrap();
        assert_eq!(link.number, 42);
        assert_eq!(link.slug, "ogoffart/focuswm");
        assert_eq!(link.last_seen, Some(1_700_000_000));
    }

    #[test]
    fn tasklist_json_round_trip_preserves_tasks_settings_and_log() {
        let mut list = TaskList::new();
        list.set_date("2026-01-02");
        let a = list.add_task("A", "work");
        list.add_task("B", "personal");
        list.set_active(a, 0);
        list.flush(600); // 10 min on A
        list.record_repo("~/code/x");
        let mut s = list.settings().clone();
        s.terminal = "foot".into();
        list.set_settings(s);

        let json = serde_json::to_string(&list).unwrap();
        let mut back: TaskList = serde_json::from_str(&json).unwrap();
        back.reindex_after_load();

        assert_eq!(back.tasks().len(), 2);
        assert_eq!(back.settings().terminal, "foot");
        assert_eq!(back.repo_history(), &["~/code/x".to_string()]);
        // The 10 minutes on A survived in the time log.
        let agg = back.time_log().aggregate("2026-01-01", "2026-01-31");
        assert_eq!(agg.total, 600);
    }

    #[test]
    fn record_repo_dedups_moves_to_front_and_caps_at_twenty() {
        let mut list = TaskList::new();
        for i in 0..25 {
            list.record_repo(&format!("repo-{i}"));
        }
        assert_eq!(list.repo_history().len(), 20, "history is capped at 20");
        assert_eq!(list.repo_history()[0], "repo-24", "most recent is first");
        // Re-recording an existing entry moves it to the front without growing.
        list.record_repo("repo-10");
        assert_eq!(list.repo_history()[0], "repo-10");
        assert_eq!(list.repo_history().len(), 20);
        // Empty paths are ignored.
        list.record_repo("");
        assert_eq!(list.repo_history().len(), 20);
    }

    #[test]
    fn removing_a_non_active_task_keeps_the_active_one() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        let b = list.add_task("B", "work");
        list.set_active(a, 0);
        let w = WindowId(1);
        list.assign_window(w);
        list.remove_task(b, 10);
        assert_eq!(list.active(), Some(a), "active task is untouched");
        assert_eq!(list.tasks().len(), 1);
        assert_eq!(list.task_of_window(w), Some(a), "A's window survives");
    }

    #[test]
    fn switch_to_desktop0_flushes_time_onto_previous_task() {
        let mut list = TaskList::new();
        list.set_date("2026-03-03");
        let a = list.add_task("A", "work");
        list.set_active(a, 100);
        list.set_scratch_active(400); // 300s on A, then desktop 0
        assert_eq!(list.active(), None);
        let agg = list.time_log().aggregate("2026-03-01", "2026-03-31");
        assert_eq!(agg.total, 300);
        // No time accrues while desktop 0 is active.
        list.flush(1000);
        let agg = list.time_log().aggregate("2026-03-01", "2026-03-31");
        assert_eq!(agg.total, 300);
    }

    #[test]
    fn aggregate_groups_by_category_and_project_sorted_desc() {
        let mut log = TimeLog::default();
        log.record("2026-04-01", TaskId(1), "Alpha", "work", 100);
        log.record("2026-04-01", TaskId(2), "Beta", "work", 300);
        log.record("2026-04-02", TaskId(3), "Gamma", "learning", 200);
        let agg = log.aggregate("2026-04-01", "2026-04-30");
        assert_eq!(agg.total, 600);
        // Categories: work=400, learning=200 (sorted by seconds desc).
        assert_eq!(agg.by_category, vec![("work".into(), 400), ("learning".into(), 200)]);
        // Projects sorted desc: Beta=300, Gamma=200, Alpha=100.
        assert_eq!(
            agg.by_project,
            vec![("Beta".into(), 300), ("Gamma".into(), 200), ("Alpha".into(), 100)]
        );
    }

    #[test]
    fn time_log_keeps_denormalized_project_after_task_rename() {
        let mut list = TaskList::new();
        list.set_date("2026-05-05");
        let a = list.add_task("OldName", "work");
        list.set_active(a, 0);
        list.flush(120);
        // Rename the task; the already-logged interval keeps the old project name.
        list.set_task_props(a, "NewName", "work", "");
        let agg = list.time_log().aggregate("2026-05-01", "2026-05-31");
        assert_eq!(agg.by_project, vec![("OldName".into(), 120)]);
    }

    #[test]
    fn set_task_props_updates_name_category_and_colour() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        list.set_task_props(a, "Renamed", "personal", "#ff0000");
        let t = list.get(a).unwrap();
        assert_eq!(t.name, "Renamed");
        assert_eq!(t.category, "personal");
        assert_eq!(t.color, "#ff0000");
    }
}
