//! The native "Open Folder" dialog.

use std::path::PathBuf;

/// Shows the system's folder picker and returns the folder chosen, or `None` when the
/// engineer cancels. Blocks until the dialog closes.
///
/// # Panics
///
/// Panics when called off the main thread; the event loop, its only caller, runs there.
#[cfg(target_os = "macos")]
pub(crate) fn choose_folder() -> Option<PathBuf> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSModalResponseOK, NSOpenPanel};
    use objc2_foundation::NSString;

    let main_thread = MainThreadMarker::new()
        .expect("the folder dialog is opened from the event loop, which runs on the main thread");
    let panel = NSOpenPanel::openPanel(main_thread);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setAllowsMultipleSelection(false);
    panel.setPrompt(Some(&NSString::from_str("Open")));
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    let url = panel.URLs().firstObject()?;
    Some(PathBuf::from(url.path()?.to_string()))
}

/// No dialog off macOS yet: Postil ships on macOS first (docs/design.md, decision 3). The
/// welcome screen still opens projects from the recent list, a dropped folder, or the
/// command line.
#[cfg(not(target_os = "macos"))]
pub(crate) fn choose_folder() -> Option<PathBuf> {
    None
}
