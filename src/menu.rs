//! The macOS menu bar: Postil, File, Edit, View and Window, as a Mac app lays them out.
//!
//! Predefined items (Copy, Paste, Hide, Minimize, Full Screen) are AppKit's own and act on the
//! focused view, the webview included. The app's own commands ([`Command`]) do not act here:
//! the event loop asks the page to run them (`window.postil.menu`), so they share the page's
//! guards, such as refusing to quit while a draft note would be lost.

use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};

/// A menu command that the page carries out.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Command {
    OpenFolder,
    SwitchProject,
    GoToFile,
    CloseWindow,
    Settings,
    Usage,
    SetupWizard,
    Quit,
    FocusMode,
    Files,
    Map,
    MapMarkers,
}

impl Command {
    const ALL: [Self; 12] = [
        Self::OpenFolder,
        Self::SwitchProject,
        Self::GoToFile,
        Self::CloseWindow,
        Self::Settings,
        Self::Usage,
        Self::SetupWizard,
        Self::Quit,
        Self::FocusMode,
        Self::Files,
        Self::Map,
        Self::MapMarkers,
    ];

    /// The menu item's id, which is also the name `window.postil.menu` takes in `ui/app.js`.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::OpenFolder => "open-folder",
            Self::SwitchProject => "switch-project",
            Self::GoToFile => "go-to-file",
            Self::CloseWindow => "close-window",
            Self::Settings => "settings",
            Self::Usage => "usage",
            Self::SetupWizard => "setup-wizard",
            Self::Quit => "quit",
            Self::FocusMode => "focus-mode",
            Self::Files => "files",
            Self::Map => "map",
            Self::MapMarkers => "map-markers",
        }
    }

    /// The command a clicked item stands for; `None` for AppKit's own items, whose ids muda
    /// makes up.
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.id() == id)
    }

    fn item(self, text: &str, accelerator: Accelerator) -> MenuItem {
        MenuItem::with_id(self.id(), text, true, Some(accelerator))
    }

    fn plain_item(self, text: &str) -> MenuItem {
        MenuItem::with_id(self.id(), text, true, None)
    }
}

/// Builds the menu bar and makes it the app's. The returned menu must live as long as the app:
/// dropping it removes the items' handlers.
pub(crate) fn install() -> Menu {
    // muda fails to add an item only to a menu built wrongly, and this one is fixed: a failure
    // is a bug here, seen at the first launch.
    build().expect("the menu bar is fixed, so muda accepts every item")
}

fn build() -> muda::Result<Menu> {
    let command = |key| Accelerator::new(Modifiers::META, key);
    let shift_command = |key| Accelerator::new(Modifiers::META | Modifiers::SHIFT, key);
    let separator = PredefinedMenuItem::separator;

    let about = AboutMetadata {
        name: Some("Postil".to_owned()),
        version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        // macOS shows neither `comments` nor `license` (muda 0.21.2 docs), only `credits`.
        credits: Some(format!(
            "{}\n{} license",
            env!("CARGO_PKG_DESCRIPTION"),
            env!("CARGO_PKG_LICENSE")
        )),
        ..AboutMetadata::default()
    };
    let app = Submenu::with_items(
        "Postil",
        true,
        &[
            &PredefinedMenuItem::about(Some("About Postil"), Some(about)),
            &separator(),
            &Command::Settings.item("Settings…", command(Code::Comma)),
            &Command::SetupWizard.plain_item("Setup Wizard…"),
            &separator(),
            &PredefinedMenuItem::services(None),
            &separator(),
            &PredefinedMenuItem::hide(Some("Hide Postil")),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &separator(),
            &Command::Quit.item("Quit Postil", command(Code::KeyQ)),
        ],
    )?;
    let file = Submenu::with_items(
        "File",
        true,
        &[
            &Command::OpenFolder.item("Open Folder…", command(Code::KeyO)),
            &Command::SwitchProject.item("Switch Project…", shift_command(Code::KeyO)),
            &Command::GoToFile.item("Go to File…", command(Code::KeyP)),
            &separator(),
            &Command::CloseWindow.item("Close Window", command(Code::KeyW)),
        ],
    )?;
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
        ],
    )?;
    let view = Submenu::with_items(
        "View",
        true,
        &[
            &Command::Files.item("Files", command(Code::KeyB)),
            &Command::Map.item("Map", shift_command(Code::KeyM)),
            &Command::Usage.item("Usage…", shift_command(Code::KeyU)),
            // A plain item, not a check item: muda ticks a check item before the page hears of
            // the click, and the page's session flag resets with every reload, so a tick would
            // drift from what the code shows.
            &Command::MapMarkers.plain_item("Map Markers"),
            &Command::FocusMode.item("Focus Mode", command(Code::Backslash)),
            &separator(),
            &PredefinedMenuItem::fullscreen(None),
        ],
    )?;
    let window = Submenu::with_items(
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(Some("Zoom")),
            &separator(),
            &PredefinedMenuItem::bring_all_to_front(None),
        ],
    )?;

    let menu = Menu::with_items(&[&app, &file, &edit, &view, &window])?;
    menu.init_for_nsapp();
    // AppKit lists the open windows in this menu.
    window.set_as_windows_menu_for_nsapp();
    Ok(menu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_reads_back_from_its_id_and_nothing_else_does() {
        // `from_id` returns the first command with the id, so two commands sharing one would
        // still read back; Close Window would then run Quit. Unique ids rule that out.
        let ids: std::collections::HashSet<_> = Command::ALL.map(Command::id).into();
        assert_eq!(
            ids.len(),
            Command::ALL.len(),
            "two commands share a menu id"
        );
        for command in Command::ALL {
            assert!(Command::from_id(command.id()).is_some(), "{}", command.id());
        }
        assert!(
            Command::from_id("1").is_none(),
            "an id muda made up for a predefined item"
        );
    }

    #[test]
    fn the_page_handles_every_command() {
        // The page's dispatch in ui/app.js names each command; a renamed id would make its
        // menu item do nothing.
        let page = include_str!("../ui/app.js");
        for command in Command::ALL {
            assert!(
                page.contains(&format!("command === \"{}\"", command.id())),
                "{}",
                command.id()
            );
        }
    }
}
