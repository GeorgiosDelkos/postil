//! The macOS menu bar: Postil, File, Edit, View and Window, as a Mac app lays them out.
//!
//! Predefined items (Copy, Paste, Hide, Minimize, Full Screen) are AppKit's own and act on the
//! focused view, the webview included. The app's own commands ([`Command`]) do not act here:
//! the event loop asks the page to run them (`window.postil.menu`), so they share the page's
//! guards, such as refusing to quit while a draft note would be lost.

use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};

/// A menu command that the page carries out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    OpenFolder,
    SwitchProject,
    GoToFile,
    CloseWindow,
    Quit,
}

impl Command {
    const ALL: [Self; 5] = [
        Self::OpenFolder,
        Self::SwitchProject,
        Self::GoToFile,
        Self::CloseWindow,
        Self::Quit,
    ];

    /// The menu item's id, which is also the name `window.postil.menu` takes in `ui/app.js`.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::OpenFolder => "open-folder",
            Self::SwitchProject => "switch-project",
            Self::GoToFile => "go-to-file",
            Self::CloseWindow => "close-window",
            Self::Quit => "quit",
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
}

/// Builds the menu bar and makes it the app's. The returned menu must live as long as the app:
/// dropping it removes the items' handlers.
///
/// # Errors
///
/// Fails when muda cannot add an item to a menu, which happens only for a menu built wrongly
/// in this function.
pub(crate) fn install() -> muda::Result<Menu> {
    let command = |key| Accelerator::new(Modifiers::META, key);
    let shift_command = |key| Accelerator::new(Modifiers::META | Modifiers::SHIFT, key);
    let separator = PredefinedMenuItem::separator;

    let about = AboutMetadata {
        name: Some("Postil".to_owned()),
        version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        comments: Some(env!("CARGO_PKG_DESCRIPTION").to_owned()),
        license: Some(env!("CARGO_PKG_LICENSE").to_owned()),
        ..AboutMetadata::default()
    };
    let app = Submenu::with_items(
        "Postil",
        true,
        &[
            &PredefinedMenuItem::about(Some("About Postil"), Some(about)),
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
    let view = Submenu::with_items("View", true, &[&PredefinedMenuItem::fullscreen(None)])?;
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
        for command in Command::ALL {
            assert_eq!(Command::from_id(command.id()), Some(command));
        }
        assert_eq!(
            Command::from_id("1"),
            None,
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
                page.contains(&format!("\"{}\"", command.id())),
                "{}",
                command.id()
            );
        }
    }
}
