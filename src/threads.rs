//! Zed's agent threads, read through `agent_ui`'s thread store.
//!
//! The rail's board lists the engine's runs. The threads the agent panel
//! creates — including every one mini-ork serves over ACP — are a separate
//! store owned by `agent_ui`, and nothing in this crate read it: a thread you
//! created from the agent panel was in the database and on no surface of the
//! rail. This module is that read, plus the one thing a list is for: opening
//! the thread it names.

use std::cmp::Reverse;
use std::path::Path;

use agent_ui::{
    Agent, AgentPanel, AgentThreadSource,
    thread_metadata_store::{ThreadMetadata, ThreadMetadataStore},
};
use gpui::{App, Entity, TaskExt as _, Window};
use workspace::Workspace;

/// The threads stored for `root`, newest first.
///
/// A thread is listed when its stored folder paths include `root`, so the rail
/// shows the project's threads and not every thread on the machine. Archived
/// threads are left out, as they are in Zed's own sidebar. A window with no
/// folder open (`root` is `None`) lists none: without a folder there is no
/// project to answer for, and guessing one would put another project's threads
/// in the board.
pub fn for_root(root: Option<&Path>, cx: &App) -> Vec<ThreadMetadata> {
    let Some(root) = root else {
        return Vec::new();
    };
    let Some(store) = ThreadMetadataStore::try_global(cx) else {
        return Vec::new();
    };
    let mut threads: Vec<ThreadMetadata> = store
        .read(cx)
        .entries()
        .filter(|thread| !thread.archived && thread.references_folder_path(root))
        .cloned()
        .collect();
    // `interacted_at` moves when you send a message; `updated_at` moves on any
    // write, including a title the agent generated. A thread you last messaged
    // an hour ago should not be pushed down by a title another agent rewrote
    // just now.
    threads.sort_by_key(|thread| Reverse(thread.interacted_at.unwrap_or(thread.updated_at)));
    threads
}

/// Open `metadata`'s thread in this workspace's agent panel.
///
/// Follows `crates/sidebar/src/sidebar.rs::load_agent_thread_in_workspace`: an
/// existing agent panel takes the thread, and only a workspace without one
/// loads the panel first. The panel is a dock panel here, not a view the rail
/// owns, so nothing about opening it belongs to the board.
pub fn open(
    workspace: &Entity<Workspace>,
    metadata: &ThreadMetadata,
    focus: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let load_thread = |agent_panel: Entity<AgentPanel>,
                       metadata: &ThreadMetadata,
                       focus: bool,
                       window: &mut Window,
                       cx: &mut App| {
        agent_panel.update(cx, |panel, cx| {
            panel.load_agent_thread(
                Agent::from(metadata.agent_id.clone()),
                metadata.thread_id,
                Some(metadata.folder_paths().clone()),
                metadata.title.clone(),
                focus,
                AgentThreadSource::Sidebar,
                window,
                cx,
            );
        });
    };

    let mut existing_panel = None;
    workspace.update(cx, |workspace, cx| {
        if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
            existing_panel = Some(panel);
        }
    });

    if let Some(agent_panel) = existing_panel {
        load_thread(agent_panel, metadata, focus, window, cx);
        workspace.update(cx, |workspace, cx| {
            if focus {
                workspace.focus_panel::<AgentPanel>(window, cx);
            } else {
                workspace.reveal_panel::<AgentPanel>(window, cx);
            }
        });
        return;
    }

    let workspace = workspace.downgrade();
    let metadata = metadata.clone();
    let mut async_window_cx = window.to_async(cx);
    cx.spawn(async move |_cx| {
        let panel = AgentPanel::load(workspace.clone(), async_window_cx.clone()).await?;

        workspace.update_in(&mut async_window_cx, |workspace, window, cx| {
            let panel = workspace.panel::<AgentPanel>(cx).unwrap_or_else(|| {
                workspace.add_panel(panel.clone(), window, cx);
                panel.clone()
            });
            load_thread(panel, &metadata, focus, window, cx);
            if focus {
                workspace.focus_panel::<AgentPanel>(window, cx);
            } else {
                workspace.reveal_panel::<AgentPanel>(window, cx);
            }
        })?;

        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}
