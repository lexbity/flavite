//! Top-level shell destinations for the application.

/// Current application destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    /// Vault overview and recent activity.
    #[default]
    Workspace,
    /// Editor surface host.
    Editor,
    /// Read-mode surface host.
    Read,
    /// Graph surface host.
    Graph,
    /// Vault user settings.
    Settings,
}

impl Pane {
    /// Sidebar navigation order.
    pub const NAV: &'static [Pane] = &[
        Self::Workspace,
        Self::Editor,
        Self::Read,
        Self::Graph,
        Self::Settings,
    ];

    /// Human-readable navigation label.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Workspace => "Workspace",
            Self::Editor => "Editor",
            Self::Read => "Read",
            Self::Graph => "Graph",
            Self::Settings => "Settings",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_lists_every_pane_once_in_sidebar_order() {
        let mut seen = Vec::new();
        for pane in Pane::NAV {
            assert!(!seen.contains(pane));
            assert!(!pane.title().is_empty());
            seen.push(*pane);
        }
        assert_eq!(seen.len(), 5);
    }
}
