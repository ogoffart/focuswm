//! Plain-Rust data models for focuswm.
//!
//! This crate has no Slint, GL or Wayland dependencies on purpose: the data
//! types and logic here (windows, tasks, time accumulation) are pure and
//! unit-testable without a display. The binary crate binds these to the
//! Slint-generated models, and the Wayland crate re-uses [`WindowId`].

mod model;
mod task;

pub use model::{WindowId, WindowInfo};
pub use task::{
    default_categories, default_idle_minutes, parse_slug, task_palette, Aggregate, GithubLink,
    Settings, Task, TaskId, TaskList, TimeEntry, TimeLog,
};
