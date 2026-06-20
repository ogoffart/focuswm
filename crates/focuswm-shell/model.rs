//! Shared data models describing client windows.

/// Stable identifier for a top-level window, assigned by the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u64);

/// Information about a top-level client window, used to drive the desktop view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowInfo {
    pub id: WindowId,
    pub app_id: String,
    pub title: String,
}

impl Default for WindowId {
    fn default() -> Self {
        WindowId(0)
    }
}

impl WindowInfo {
    pub fn new(id: WindowId) -> Self {
        Self {
            id,
            app_id: String::new(),
            title: String::new(),
        }
    }

    /// A human-friendly label: the title, falling back to the app id, falling
    /// back to a placeholder.
    pub fn label(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else if !self.app_id.is_empty() {
            &self.app_id
        } else {
            "(untitled)"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_prefers_title_then_app_id() {
        let mut w = WindowInfo::new(WindowId(1));
        assert_eq!(w.label(), "(untitled)");
        w.app_id = "foot".into();
        assert_eq!(w.label(), "foot");
        w.title = "vim".into();
        assert_eq!(w.label(), "vim");
    }

    #[test]
    fn window_ids_are_ordered() {
        assert!(WindowId(1) < WindowId(2));
    }
}
