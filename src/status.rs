//! Status bar items: the board toggle and the live mini-ork counters on the
//! left; on the right, the mini-ork version and how many ContextNest sessions
//! touched the active editor's file.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Component, Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::Result;
use gpui::{App, Context, Entity, Render, SharedString, Subscription, Task, WeakEntity, Window};
use project::Project;
use serde_json::Value;
use ui::{Divider, DividerColor, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};

use crate::{
    board::{self, Lane},
    page,
    palette::{self as p, c},
    sidebar::MiniOrkSidebar,
    store::BoardStore,
};

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let weak = cx.entity().downgrade();
        let Some(store) = BoardStore::for_workspace(&cx.entity(), cx) else {
            return;
        };
        let project = workspace.project().clone();
        let file_sessions = cx.new(|cx| MiniOrkFileSessions::new(weak.clone(), project, store.clone(), cx));
        let left = cx.new(|cx| MiniOrkStatus::new(weak, store.clone(), cx));
        let right = cx.new(|cx| MiniOrkVersion::new(store, cx));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_left_item(left, window, cx);
            status_bar.add_right_item(right, window, cx);
            status_bar.add_right_item(file_sessions, window, cx);
        });
    })
    .detach();
}

pub struct MiniOrkStatus {
    workspace: WeakEntity<Workspace>,
    store: Entity<BoardStore>,
    _store: Subscription,
}

impl MiniOrkStatus {
    fn new(workspace: WeakEntity<Workspace>, store: Entity<BoardStore>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            workspace,
            store,
            _store: sub,
        }
    }
}

fn toggle_board(workspace: &WeakEntity<Workspace>, cx: &mut App) {
    with_sidebar(workspace, cx, |sidebar, cx| sidebar.toggle_board(cx));
}

/// Open the Threads board, where working and failed runs are grouped.
fn show_board(workspace: &WeakEntity<Workspace>, cx: &mut App) {
    with_sidebar(workspace, cx, |sidebar, cx| sidebar.show_board(cx));
}

/// Run `f` on the rail, wherever the host mounted it.
///
/// The rail is a `Sidebar`, but a host that wants the project sidebar — and
/// with it Zed's thread list — registers it in a dock instead (see the `Panel`
/// impl in `sidebar.rs`). Both mountings are live, so look in both places.
fn with_sidebar(
    workspace: &WeakEntity<Workspace>,
    cx: &mut App,
    f: impl FnOnce(&mut MiniOrkSidebar, &mut Context<MiniOrkSidebar>),
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let rail = workspace
        .read(cx)
        .multi_workspace()
        .and_then(|multi_workspace| multi_workspace.upgrade())
        .and_then(|multi_workspace| {
            multi_workspace
                .read(cx)
                .sidebar()
                .and_then(|sidebar| sidebar.to_any().downcast::<MiniOrkSidebar>().ok())
        })
        .or_else(|| workspace.read(cx).panel::<MiniOrkSidebar>(cx));
    let Some(rail) = rail else {
        return;
    };
    rail.update(cx, f);
}

impl Render for MiniOrkStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self
            .store
            .read(cx)
            .board
            .as_ref()
            .map(|b| b.header.clone())
            .unwrap_or_default();
        let item = |id: &'static str, label: String, colour: u32, tip: SharedString, key: &'static str, tab: Option<&'static str>, workspace: WeakEntity<Workspace>| {
            div()
                .id(id)
                .h_5()
                .px(rems_from_px(7_f32))
                .flex()
                .items_center()
                .text_size(rems_from_px(12_f32))
                .text_color(c(colour))
                .cursor_pointer()
                .hover(|s| s.bg(c(p::HOVER)))
                .tooltip(Tooltip::text(tip))
                .child(label)
                .on_click(move |_, window, cx| {
                    let workspace = workspace.clone();
                    if key == "threads" {
                        show_board(&workspace, cx);
                        return;
                    }
                    window.defer(cx, move |window, cx| {
                        if let Some(workspace) = workspace.upgrade() {
                            page::open_page(&workspace, key.into(), tab.map(String::from), Default::default(), window, cx);
                        }
                    });
                })
        };
        let w = self.workspace.clone();
        // Spend is muted until it nears the cap: the colour is the only thing
        // here that should make you look.
        let spent = header.today_usd.unwrap_or(0.);
        let budget_colour = if header.cap_usd > 0. && spent >= header.cap_usd {
            p::RED
        } else if header.cap_usd > 0. && spent >= header.cap_usd * 0.8 {
            p::YELLOW
        } else {
            p::MUTED
        };
        h_flex()
            .gap_0p5()
            .child(
                div()
                    .id("status-board")
                    .h_5()
                    .px_1p5()
                    .flex()
                    .items_center()
                    .text_size(rems_from_px(12_f32))
                    .text_color(c(p::MUTED))
                    .cursor_pointer()
                    .hover(|s| s.bg(c(p::HOVER)))
                    .tooltip(Tooltip::text("Toggle task board"))
                    .child("▤")
                    .on_click({
                        let w = w.clone();
                        move |_, _, cx| toggle_board(&w, cx)
                    }),
            )
            .child(Divider::vertical().h_3p5().mx_1().color(DividerColor::Border))
            .child(item(
                "status-needs",
                format!("✋ {} need you", header.needs_you),
                if header.needs_you > 0 { p::ORANGE } else { p::SUB },
                "Threads and gates waiting on you — opens the Inbox".into(),
                "inbox",
                None,
                w.clone(),
            ))
            .child(item(
                "status-working",
                format!("● {} working", header.working),
                if header.working > 0 { p::BLUE } else { p::SUB },
                "Runs in flight — on the Threads board".into(),
                "threads",
                None,
                w.clone(),
            ))
            .child(item(
                "status-failed",
                format!("✗ {} failed", header.failed),
                if header.failed > 0 { p::RED } else { p::SUB },
                "Failed runs — on the Threads board".into(),
                "threads",
                None,
                w.clone(),
            ))
            .child(item(
                "status-budget",
                format!("{} / ${:.0}", money(header.today_usd), header.cap_usd),
                budget_colour,
                "Spend today against the daily cap".into(),
                "lanes",
                Some("cost"),
                w.clone(),
            ))
            .child(item(
                "status-scheduler",
                format!("◷ {}", if header.scheduler_on { "on" } else { "off" }),
                if header.scheduler_on { p::MUTED } else { p::YELLOW },
                "Project scheduler".into(),
                "autos",
                Some("schedulers"),
                w.clone(),
            ))
            // Lanes are per-model detail: they earn a place in the bar only
            // when one is failing, and then as one alert rather than five bars.
            .when_some(lane_alert(&header.lanes), |this, (label, tip)| {
                this.child(Divider::vertical().h_3p5().mx_1().color(DividerColor::Border))
                    .child(item("status-lane-alert", label, p::RED, tip, "lanes", Some("cost"), w.clone()))
            })
    }
}

/// The lanes failing half their calls (over at least three), as one alert for
/// the status bar: the count to read, the names and last error in the tooltip.
/// `None` when every lane is healthy, so a working project shows no lane noise.
fn lane_alert(lanes: &[Lane]) -> Option<(String, SharedString)> {
    let failing: Vec<&Lane> = lanes
        .iter()
        .filter(|lane| lane.calls >= 3 && lane.failed * 2 >= lane.calls)
        .collect();
    let first = failing.first()?;
    let label = match failing.len() {
        1 => "⚠ 1 lane failing".to_string(),
        n => format!("⚠ {n} lanes failing"),
    };
    let mut tip = failing
        .iter()
        .map(|lane| format!("{}: {}/{} failed · ${:.2}", lane.lane, lane.failed, lane.calls, lane.usd))
        .collect::<Vec<_>>()
        .join(" · ");
    if !first.last_error.is_empty() {
        tip.push_str(&format!(" · {}", first.last_error));
    }
    Some((label, tip.into()))
}

impl StatusItemView for MiniOrkStatus {
    fn set_active_pane_item(&mut self, _: Option<&dyn ItemHandle>, _: &mut Window, _: &mut Context<Self>) {}

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

pub struct MiniOrkVersion {
    store: Entity<BoardStore>,
    _store: Subscription,
}

impl MiniOrkVersion {
    fn new(store: Entity<BoardStore>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&store, |_, _, cx| cx.notify());
        Self { store, _store: sub }
    }
}

impl Render for MiniOrkVersion {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let version = self
            .store
            .read(cx)
            .board
            .as_ref()
            .map(|b| b.header.version.clone())
            .unwrap_or_default();
        let label: SharedString = if version.is_empty() {
            "mini-ork".into()
        } else {
            format!("mini-ork {version}").into()
        };
        div().px(rems_from_px(7_f32)).text_size(rems_from_px(12_f32)).text_color(c(p::MUTED)).child(label)
    }
}

impl StatusItemView for MiniOrkVersion {
    fn set_active_pane_item(&mut self, _: Option<&dyn ItemHandle>, _: &mut Window, _: &mut Context<Self>) {}

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

fn money(value: Option<f64>) -> String {
    value.map(|v| format!("${v:.2}")).unwrap_or_else(|| "—".to_string())
}

/// How long a file must stay active before its count is read: cycling through
/// tabs costs one engine call, for the tab landed on, not one per tab passed.
const SESSIONS_DEBOUNCE: Duration = Duration::from_millis(300);

/// A count older than this is read again the next time its file is shown:
/// sessions keep touching files while the IDE stays open for days.
const SESSIONS_FRESH_FOR: Duration = Duration::from_secs(5 * 60);

/// The `cn_file` page lists at most this many sessions (`_SESSIONS_LIMIT` in
/// `mini_ork/ide_pages/cn_file.py`), so a full list is a floor, not a count.
const SESSIONS_LISTED: usize = 30;

/// Counts to keep before dropping the lot, as the page cache in `store` does.
const SESSION_COUNTS_LIMIT: usize = 512;

/// The active editor's file as "N sessions": how many ContextNest sessions
/// touched it. Hidden for a file outside the project and whenever the count is
/// unknown; a click opens the file's `cn_file` page.
pub struct MiniOrkFileSessions {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Entity<BoardStore>,
    /// The active item's file, absolute.
    active_file: Option<PathBuf>,
    sessions: Entity<FileSessions>,
    _subscriptions: Vec<Subscription>,
}

impl MiniOrkFileSessions {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        store: Entity<BoardStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let bin = store.read(cx).bin.clone();
        let read_page: ReadPage = Rc::new(move |target: &FileTarget, cx: &mut App| {
            let args = BTreeMap::from([("path".to_string(), target.path.clone())]);
            cx.background_spawn(board::fetch_page(
                bin.clone(),
                target.home.clone(),
                "cn_file".to_string(),
                None,
                args,
            ))
        });
        let sessions = cx.new(|_| FileSessions::new(read_page));
        let subscriptions = vec![
            // The home is known only once the board has been read, which is
            // usually after the first file is already open.
            cx.observe(&store, |this, _, cx| this.retarget(cx)),
            cx.observe(&sessions, |_, _, cx| cx.notify()),
        ];
        Self {
            workspace,
            project,
            store,
            active_file: None,
            sessions,
            _subscriptions: subscriptions,
        }
    }

    fn retarget(&mut self, cx: &mut Context<Self>) {
        let target = self.target(cx);
        self.sessions.update(cx, |sessions, cx| sessions.follow(target, cx));
    }

    fn target(&self, cx: &App) -> Option<FileTarget> {
        let file = self.active_file.as_ref()?;
        // The root comes from the project, not through the workspace as
        // `projects::current_root` reads it: the status bar first asks for
        // the active item while the workspace is still mid-update.
        let worktree = self.project.read(cx).visible_worktrees(cx).next()?;
        let path = project_relative(file, &worktree.read(cx).abs_path())?;
        let home = self.store.read(cx).home.clone()?;
        Some(FileTarget { home, path })
    }
}

impl Render for MiniOrkFileSessions {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sessions = self.sessions.read(cx);
        let (Some(target), Some(count)) = (sessions.target.clone(), sessions.count()) else {
            return div().into_any_element();
        };
        let workspace = self.workspace.clone();
        let path = target.path;
        div()
            .id("status-file-sessions")
            .h_5()
            .px(rems_from_px(7_f32))
            .flex()
            .items_center()
            .text_size(rems_from_px(12_f32))
            .text_color(c(p::MUTED))
            .cursor_pointer()
            .hover(|style| style.bg(c(p::HOVER)))
            .tooltip(Tooltip::text(format!(
                "ContextNest sessions that touched {path} — opens its history"
            )))
            .child(sessions_label(count))
            .on_click(move |_, window, cx| {
                let workspace = workspace.clone();
                let args = BTreeMap::from([("path".to_string(), path.clone())]);
                window.defer(cx, move |window, cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        page::open_page(&workspace, "cn_file".into(), None, args, window, cx);
                    }
                });
            })
            .into_any_element()
    }
}

impl StatusItemView for MiniOrkFileSessions {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_file = active_pane_item
            .and_then(|item| item.project_path(cx))
            .and_then(|project_path| self.project.read(cx).absolute_path(&project_path, cx));
        // The status bar calls this on every change to the active pane, not
        // only when the tab changes.
        if active_file == self.active_file {
            return;
        }
        self.active_file = active_file;
        self.retarget(cx);
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

/// What a session count is read for: the file's path under the project, and
/// the `.mini-ork` home whose engine is asked.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FileTarget {
    home: PathBuf,
    path: String,
}

/// Session counts by file, each with when it was read.
#[derive(Default)]
struct SessionCounts(HashMap<FileTarget, (usize, Instant)>);

impl SessionCounts {
    fn get(&self, target: &FileTarget) -> Option<usize> {
        self.0.get(target).map(|(count, _)| *count)
    }

    fn is_fresh(&self, target: &FileTarget, now: Instant) -> bool {
        self.0
            .get(target)
            .is_some_and(|(_, read_at)| now.saturating_duration_since(*read_at) < SESSIONS_FRESH_FOR)
    }

    fn insert(&mut self, target: FileTarget, count: usize, now: Instant) {
        if self.0.len() >= SESSION_COUNTS_LIMIT && !self.0.contains_key(&target) {
            self.0.clear();
        }
        self.0.insert(target, (count, now));
    }
}

/// Reads one file's `cn_file` page.
type ReadPage = Rc<dyn Fn(&FileTarget, &mut App) -> Task<Result<Value>>>;

/// The session count of the file the status bar follows, read once the file
/// has stayed active for `SESSIONS_DEBOUNCE` and kept per file.
struct FileSessions {
    target: Option<FileTarget>,
    counts: SessionCounts,
    read_page: ReadPage,
    /// The pending read for `target`. Replacing it drops, and so cancels, the
    /// read for a file left before its pause ran out.
    reading: Option<Task<()>>,
}

impl FileSessions {
    fn new(read_page: ReadPage) -> Self {
        Self {
            target: None,
            counts: SessionCounts::default(),
            read_page,
            reading: None,
        }
    }

    fn follow(&mut self, target: Option<FileTarget>, cx: &mut Context<Self>) {
        if target == self.target {
            return;
        }
        self.target = target;
        self.reading = None;
        let now = cx.background_executor().now();
        if let Some(target) = self.target.clone().filter(|target| !self.counts.is_fresh(target, now)) {
            self.reading = Some(self.read_after_pause(target, cx));
        }
        cx.notify();
    }

    fn read_after_pause(&self, target: FileTarget, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SESSIONS_DEBOUNCE).await;
            let Ok(read) = this.update(cx, |this, cx| (this.read_page)(&target, cx)) else {
                return;
            };
            let page = read.await;
            this.update(cx, |this, cx| match page {
                // An unknown count is not kept, so the file is asked about
                // again the next time it is shown.
                Ok(page) => {
                    if let Some(count) = session_count(&page) {
                        let now = cx.background_executor().now();
                        this.counts.insert(target, count, now);
                        cx.notify();
                    }
                }
                Err(error) => log::warn!("reading ContextNest sessions for {}: {error:#}", target.path),
            })
            .log_err();
        })
    }

    fn count(&self) -> Option<usize> {
        self.counts.get(self.target.as_ref()?)
    }
}

/// `file`'s path under `root`, `/`-separated as the engine takes it; `None`
/// for a file outside the project, which ContextNest is never asked about.
fn project_relative(file: &Path, root: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// How many sessions a `cn_file` page lists under "Sessions", or `None` when
/// the page cannot say: the section is missing (the path was refused) or
/// failed to build, or it holds the one-line ContextNest state the engine
/// draws when ContextNest is down, off or misconfigured. A count is only
/// given when every row is understood, so an outage never reads as zero.
fn session_count(page: &Value) -> Option<usize> {
    if page.get("ok").and_then(Value::as_bool) == Some(false) || page.pointer("/errors/Sessions").is_some() {
        return None;
    }
    let section = page
        .get("sections")?
        .as_array()?
        .iter()
        .find(|section| section.get("title").and_then(Value::as_str) == Some("Sessions"))?;
    let mut sessions = 0;
    for row in section.get("items")?.as_array()? {
        if opens_session(row) {
            sessions += 1;
        } else if !is_plain_note(row) {
            return None;
        }
    }
    Some(sessions)
}

/// A session row: its Open button goes to the session's own page.
fn opens_session(row: &Value) -> bool {
    row.get("acts").and_then(Value::as_array).is_some_and(|acts| {
        acts.iter()
            .any(|act| act.pointer("/do/page").and_then(Value::as_str) == Some("cn_session"))
    })
}

/// A plain `•` line such as "No sessions touched this file yet". The
/// ContextNest state line carries a `kind` instead of a mark, and a section
/// that failed to build says so with `✗`.
fn is_plain_note(row: &Value) -> bool {
    row.get("kind").is_none() && row.get("m").and_then(Value::as_str) == Some("•")
}

fn sessions_label(count: usize) -> String {
    match count {
        1 => "1 session".to_string(),
        count if count >= SESSIONS_LISTED => format!("{SESSIONS_LISTED}+ sessions"),
        count => format!("{count} sessions"),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gpui::TestAppContext;
    use serde_json::json;

    use super::*;

    /// A `cn_file` page shaped as the engine renders one, with `rows` under
    /// "Sessions".
    fn cn_file_page(rows: Vec<Value>) -> Value {
        json!({
            "ok": true,
            "key": "cn_file",
            "title": "mini_ork/scheduler.py",
            "sub": "project-relative path · ContextNest view",
            "args": {"path": "mini_ork/scheduler.py"},
            "sections": [
                {"type": "list", "title": "Sessions", "note": "", "actions": [], "full": true, "items": rows},
                {"type": "list", "title": "Live holder", "note": "", "actions": [], "full": true, "items": [
                    {"m": "•", "mc": "sub", "t": "No live claim on this file", "tc": "text", "sub": "",
                     "mono": false, "when": "", "acts": []}
                ]},
            ],
            "errors": {},
            "label": "CN file",
        })
    }

    fn session_row(id: &str) -> Value {
        json!({
            "m": "✓", "mc": "green", "t": "Verify the r24 epic run, ship it, and close the epic set",
            "tc": "text", "sub": "last activity 2026-10-13T22:47:45.408Z · 1 matched file(s) here",
            "mono": false, "when": "",
            "acts": [{"label": "Open", "kind": "ghost", "do": {"page": "cn_session", "args": {"id": id}}}],
        })
    }

    fn no_sessions_row() -> Value {
        json!({"m": "•", "mc": "sub", "t": "No sessions touched this file yet", "tc": "text", "sub": "",
               "mono": false, "when": "", "acts": []})
    }

    fn contextnest_line(text: &str, tone: &str) -> Value {
        json!({"kind": "dot", "label": "ContextNest", "text": text, "tone": tone})
    }

    #[test]
    fn a_page_counts_one_session_per_row() {
        let page = cn_file_page(vec![session_row("s1"), session_row("s2"), session_row("s3")]);
        assert_eq!(session_count(&page), Some(3));
    }

    #[test]
    fn a_file_no_session_touched_reads_zero_sessions() {
        let page = cn_file_page(vec![no_sessions_row()]);
        assert_eq!(session_count(&page), Some(0));
        assert_eq!(sessions_label(0), "0 sessions");
    }

    #[test]
    fn contextnest_down_off_or_misconfigured_is_unknown() {
        for line in [
            contextnest_line("not running · start ContextNest or set CN_BASE_URL", "warn"),
            contextnest_line("off · MO_DISABLE_CN=1", "sub"),
            contextnest_line("config error · missing token", "error"),
        ] {
            assert_eq!(session_count(&cn_file_page(vec![line])), None);
        }
    }

    #[test]
    fn a_sessions_section_that_failed_to_build_is_unknown() {
        let failed = json!({"m": "✗", "mc": "red", "t": "Could not read this", "tc": "text",
                            "sub": "KeyError: 'matches'", "mono": false, "when": "", "acts": []});
        assert_eq!(session_count(&cn_file_page(vec![failed])), None);
        let mut page = cn_file_page(vec![no_sessions_row()]);
        page["errors"] = json!({"Sessions": "KeyError: 'matches'"});
        assert_eq!(session_count(&page), None, "the page's own error record wins over its rows");
    }

    #[test]
    fn a_page_without_a_sessions_section_is_unknown() {
        let outside = json!({
            "ok": true, "key": "cn_file", "title": "CN file", "errors": {},
            "sections": [{"type": "callout", "title": "Path is outside the project", "text_md": "…"}],
        });
        assert_eq!(session_count(&outside), None);
        let old_engine = json!({"ok": false, "error": "no page 'cn_file' (pages: run, task)"});
        assert_eq!(session_count(&old_engine), None);
    }

    #[test]
    fn the_label_reads_a_full_list_as_a_floor() {
        assert_eq!(sessions_label(1), "1 session");
        assert_eq!(sessions_label(9), "9 sessions");
        assert_eq!(sessions_label(SESSIONS_LISTED), "30+ sessions");
    }

    #[test]
    fn only_files_inside_the_project_have_a_path() {
        let root = Path::new("/work/mini-ork");
        assert_eq!(
            project_relative(Path::new("/work/mini-ork/mini_ork/scheduler.py"), root).as_deref(),
            Some("mini_ork/scheduler.py")
        );
        assert_eq!(project_relative(Path::new("/etc/hosts"), root), None);
        assert_eq!(project_relative(Path::new("/work/mini-ork-ui/src/status.rs"), root), None);
        assert_eq!(project_relative(Path::new("/work/mini-ork/../secrets.env"), root), None);
        assert_eq!(project_relative(root, root), None);
    }

    #[test]
    fn a_count_is_fresh_until_it_ages_out() {
        let target = FileTarget {
            home: PathBuf::from("/work/mini-ork/.mini-ork"),
            path: "README.md".to_string(),
        };
        let read_at = Instant::now();
        let mut counts = SessionCounts::default();
        assert!(!counts.is_fresh(&target, read_at));
        counts.insert(target.clone(), 4, read_at);
        assert_eq!(counts.get(&target), Some(4));
        assert!(counts.is_fresh(&target, read_at + SESSIONS_FRESH_FOR / 2));
        assert!(!counts.is_fresh(&target, read_at + SESSIONS_FRESH_FOR));
        let other_home = FileTarget {
            home: PathBuf::from("/work/other/.mini-ork"),
            ..target
        };
        assert_eq!(counts.get(&other_home), None);
    }

    /// An engine that records each file it is asked about and answers `page`.
    fn stub_engine(page: Value) -> (ReadPage, Rc<RefCell<Vec<String>>>) {
        let asked = Rc::new(RefCell::new(Vec::new()));
        let read_page: ReadPage = Rc::new({
            let asked = asked.clone();
            move |target: &FileTarget, _: &mut App| {
                asked.borrow_mut().push(target.path.clone());
                Task::ready(Ok(page.clone()))
            }
        });
        (read_page, asked)
    }

    fn file(path: &str) -> Option<FileTarget> {
        Some(FileTarget {
            home: PathBuf::from("/work/mini-ork/.mini-ork"),
            path: path.to_string(),
        })
    }

    fn follow(sessions: &Entity<FileSessions>, target: Option<FileTarget>, cx: &mut TestAppContext) {
        sessions.update(cx, |sessions, cx| sessions.follow(target, cx));
    }

    fn shown(sessions: &Entity<FileSessions>, cx: &mut TestAppContext) -> Option<usize> {
        sessions.read_with(cx, |sessions, _| sessions.count())
    }

    #[gpui::test]
    async fn a_file_is_read_once_it_settles_and_once_per_path(cx: &mut TestAppContext) {
        let (read_page, asked) = stub_engine(cn_file_page(vec![session_row("s1"), session_row("s2")]));
        let sessions = cx.new(|_| FileSessions::new(read_page));

        follow(&sessions, file("src/a.rs"), cx);
        cx.executor().advance_clock(SESSIONS_DEBOUNCE / 2);
        follow(&sessions, file("src/b.rs"), cx);
        cx.executor().advance_clock(SESSIONS_DEBOUNCE / 2);
        cx.run_until_parked();
        assert!(asked.borrow().is_empty(), "nothing is read before the pause runs out");
        assert_eq!(shown(&sessions, cx), None);

        cx.executor().advance_clock(SESSIONS_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(*asked.borrow(), ["src/b.rs"], "a file passed through is never read");
        assert_eq!(shown(&sessions, cx), Some(2));

        for _ in 0..3 {
            follow(&sessions, file("src/a.rs"), cx);
            cx.executor().advance_clock(SESSIONS_DEBOUNCE);
            cx.run_until_parked();
            follow(&sessions, file("src/b.rs"), cx);
            assert_eq!(shown(&sessions, cx), Some(2), "a cached count shows at once");
            cx.executor().advance_clock(SESSIONS_DEBOUNCE);
            cx.run_until_parked();
        }
        assert_eq!(*asked.borrow(), ["src/b.rs", "src/a.rs"]);

        follow(&sessions, None, cx);
        assert_eq!(shown(&sessions, cx), None, "a file outside the project shows nothing");
    }

    #[gpui::test]
    async fn an_unknown_count_hides_the_item_and_is_asked_again(cx: &mut TestAppContext) {
        let down = contextnest_line("not running · start ContextNest or set CN_BASE_URL", "warn");
        let (read_page, asked) = stub_engine(cn_file_page(vec![down]));
        let sessions = cx.new(|_| FileSessions::new(read_page));
        for path in ["src/a.rs", "src/b.rs", "src/a.rs"] {
            follow(&sessions, file(path), cx);
            cx.executor().advance_clock(SESSIONS_DEBOUNCE);
            cx.run_until_parked();
            assert_eq!(shown(&sessions, cx), None);
        }
        assert_eq!(*asked.borrow(), ["src/a.rs", "src/b.rs", "src/a.rs"]);
    }

    #[gpui::test]
    async fn a_stale_count_shows_while_it_is_read_again(cx: &mut TestAppContext) {
        let (read_page, asked) = stub_engine(cn_file_page(vec![session_row("s1")]));
        let sessions = cx.new(|_| FileSessions::new(read_page));
        follow(&sessions, file("src/a.rs"), cx);
        cx.executor().advance_clock(SESSIONS_DEBOUNCE);
        cx.run_until_parked();
        follow(&sessions, None, cx);
        cx.executor().advance_clock(SESSIONS_FRESH_FOR);
        follow(&sessions, file("src/a.rs"), cx);
        assert_eq!(shown(&sessions, cx), Some(1));
        cx.executor().advance_clock(SESSIONS_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(*asked.borrow(), ["src/a.rs", "src/a.rs"]);
    }
}
