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

use std::collections::HashMap;

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
}

impl Task {
    pub fn new(id: TaskId, name: impl Into<String>, category: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            category: category.into(),
            branch: None,
            repo: None,
            worktree_path: None,
            accumulated_secs: 0,
            has_notification: false,
        }
    }
}

/// The ordered set of tasks plus window assignment, active task and time
/// accumulation.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TaskList {
    /// Tasks in sidebar order.
    tasks: Vec<Task>,
    /// window -> the task it belongs to.
    #[serde(default, skip)]
    window_task: HashMap<WindowId, TaskId>,
    /// The currently active (focused) task.
    #[serde(default, skip)]
    active: Option<TaskId>,
    /// Timestamp (seconds) at which the active task became active; `None` when
    /// no task is active or time tracking hasn't started.
    #[serde(default, skip)]
    active_since: Option<u64>,
    /// Next task id to hand out.
    #[serde(default)]
    next_id: u64,
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
        self.tasks.push(Task::new(id, name, category));
        id
    }

    /// Remove a task and any windows assigned to it. If it was active, the
    /// active task becomes the first remaining task (or `None`); the supplied
    /// `now` flushes the running interval first.
    pub fn remove_task(&mut self, id: TaskId, now: u64) {
        if self.active == Some(id) {
            self.flush(now);
            self.active = None;
            self.active_since = None;
        }
        self.tasks.retain(|t| t.id != id);
        self.window_task.retain(|_, t| *t != id);
        if self.active.is_none() {
            if let Some(first) = self.tasks.first().map(|t| t.id) {
                self.set_active(first, now);
            }
        }
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

    /// Flush the running focus interval into the active task without switching,
    /// e.g. on a periodic persist tick. `now` is seconds.
    pub fn flush(&mut self, now: u64) {
        if let (Some(active), Some(since)) = (self.active, self.active_since) {
            let delta = now.saturating_sub(since);
            if delta > 0 {
                if let Some(task) = self.get_mut(active) {
                    task.accumulated_secs += delta;
                }
                self.active_since = Some(now);
            }
        }
    }

    /// Assign a newly mapped window to the active task. Returns the task it was
    /// assigned to, if any.
    pub fn assign_window(&mut self, window: WindowId) -> Option<TaskId> {
        let active = self.active?;
        self.window_task.insert(window, active);
        Some(active)
    }

    /// Forget a window that has been unmapped.
    pub fn remove_window(&mut self, window: WindowId) {
        self.window_task.remove(&window);
    }

    /// The task a window belongs to, if known.
    pub fn task_of_window(&self, window: WindowId) -> Option<TaskId> {
        self.window_task.get(&window).copied()
    }

    /// Windows belonging to a task, in insertion order is not guaranteed; the
    /// caller should not rely on ordering.
    pub fn windows_for(&self, id: TaskId) -> Vec<WindowId> {
        let mut v: Vec<WindowId> = self
            .window_task
            .iter()
            .filter(|(_, t)| **t == id)
            .map(|(w, _)| *w)
            .collect();
        v.sort();
        v
    }

    /// Windows belonging to the active task (the ones that should be visible).
    pub fn active_windows(&self) -> Vec<WindowId> {
        match self.active {
            Some(id) => self.windows_for(id),
            None => Vec::new(),
        }
    }

    /// Whether a window is on the active task (i.e. currently visible).
    pub fn is_visible(&self, window: WindowId) -> bool {
        self.active.is_some() && self.task_of_window(window) == self.active
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn removing_active_task_picks_a_new_active_and_drops_windows() {
        let mut list = TaskList::new();
        let a = list.add_task("A", "work");
        let b = list.add_task("B", "work");
        list.set_active(a, 0);
        list.assign_window(WindowId(1));
        list.remove_task(a, 50);
        assert_eq!(list.get(a), None);
        assert_eq!(list.task_of_window(WindowId(1)), None);
        assert_eq!(list.active(), Some(b));
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
}
