//! The mini-ork IDE shell, drawn to `mini-ork IDE.dc.html`.
//!
//! Everything mini-ork-specific in the IDE lives in this crate. Zed's own
//! sources are touched only by the small hook commit that registers the crate
//! (workspace `Cargo.toml`, `crates/zed/Cargo.toml`), calls [`init`] and mounts
//! [`MiniOrkSidebar`] in the right dock — so a Zed upgrade is a rebase of that
//! hook commit, and API drift stays inside this crate. The rail is a `Sidebar`
//! as well as a `Panel`: a host that wants it in the sidebar slot still can,
//! but the slot that carries Zed's thread list is no longer the rail's.
//!
//! - `sidebar`: activity bar + Threads board (a right-dock panel)
//! - `threads`: Zed's own agent threads, read from `agent_ui`'s thread store
//! - `title_bar`: replaces Zed's title bar item
//! - `status`: status bar items
//! - `page`: every activity page and run tab, rendered from
//!   `mini-ork board page <key>` JSON
//! - `store`: one `mini-ork board` poller per workspace + page actions
//!
//! mini-ork stays the source of truth: the shell is a view over its CLI.

mod board;
mod components;
mod documents;
mod flow;
mod graph;
mod ide_http;
mod jump;
mod loading;
mod node_tab;
mod page;
mod palette;
mod projects;
mod run_panel;
mod sections;
mod select;
mod sidebar;
mod status;
mod store;
mod threads;
mod thumb;
mod title_bar;

pub use sidebar::MiniOrkSidebar;

use gpui::{App, BorrowAppContext as _};
use settings::{MergeFromTrait as _, SettingsContent, SettingsStore};

/// The IDE's own defaults, layered under the user's settings (which stay
/// shared with stock Zed and are never written). Every dock button keeps its
/// stock Zed default, so the status bar carries the same panel toggles: the
/// activity bar is mini-ork's own drawer and does not stand in for them. The
/// agent panel starts in the left dock; the rail and its boards take the right
/// one, so both are on screen at once.
const IDE_DEFAULTS: &str = r#"{
  "agent": { "dock": "left" }
}"#;

pub fn init(cx: &mut App) {
    // Every IDE colour follows the active Zed theme, and re-colours on change.
    palette::sync_theme(cx);
    cx.observe_global::<theme::GlobalTheme>(|cx| {
        palette::sync_theme(cx);
        cx.refresh_windows();
    })
    .detach();
    store::init(cx);
    title_bar::init(cx);
    status::init(cx);
    jump::init(cx);
    run_panel::init(cx);
    apply_ide_defaults(cx);
}

fn apply_ide_defaults(cx: &mut App) {
    let overlay = match serde_json::from_str::<SettingsContent>(IDE_DEFAULTS) {
        Ok(overlay) => overlay,
        Err(error) => {
            log::error!("mini-ork IDE defaults: {error}");
            return;
        }
    };
    if cx.has_global::<SettingsStore>() {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_default_settings(cx, |defaults| defaults.merge_from(&overlay));
        });
    }
}
