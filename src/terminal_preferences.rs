//! Live, device-local terminal preferences shared by existing and new panes.
use std::sync::atomic::{AtomicBool, Ordering};

// Preserve existing installations' selection behavior until changed in Settings.
static COPY_ON_SELECT: AtomicBool = AtomicBool::new(true);

pub(crate) fn copy_on_select() -> bool {
    COPY_ON_SELECT.load(Ordering::Relaxed)
}

pub(crate) fn set_copy_on_select(enabled: bool) {
    COPY_ON_SELECT.store(enabled, Ordering::Relaxed);
}
