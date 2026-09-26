//! Dock badge (DESIGN §8.3, M3 §4): the number of live sessions whose agent
//! state needs attention, as `NSApp.dockTile.badgeLabel`. Set on the main
//! thread, and only when the number changes; a failure is logged and
//! otherwise ignored (the badge is a convenience).

/// The badge text for `count` sessions; `None` hides the badge.
pub fn label(count: usize) -> Option<String> {
    (count > 0).then(|| count.to_string())
}

/// The badge as last set.
#[derive(Debug, Default)]
pub struct DockBadge {
    shown: Option<Option<String>>,
}

impl DockBadge {
    /// Show `count` (no call when unchanged).
    pub fn update(&mut self, count: usize) {
        let want = label(count);
        if self.shown.as_ref() == Some(&want) {
            return;
        }
        match set(want.as_deref()) {
            Ok(read_back) => {
                tracing::info!(badge = ?want, ?read_back, "dock badge set");
                self.shown = Some(want);
            }
            Err(e) => tracing::warn!(badge = ?want, "dock badge not set: {e}"),
        }
    }
}

/// Set the badge; returns the label read back from the dock tile.
#[cfg(target_os = "macos")]
fn set(label: Option<&str>) -> Result<Option<String>, &'static str> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    let tile = NSApplication::sharedApplication(mtm).dockTile();
    let text = label.map(NSString::from_str);
    tile.setBadgeLabel(text.as_deref());
    Ok(tile.badgeLabel().map(|s| s.to_string()))
}

#[cfg(not(target_os = "macos"))]
fn set(_label: Option<&str>) -> Result<Option<String>, &'static str> {
    Err("no dock on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(label(0), None);
        assert_eq!(label(1).as_deref(), Some("1"));
        assert_eq!(label(12).as_deref(), Some("12"));
    }
}
