//! A run's graph, full screen (the design's "Run graph · full screen"): every
//! node on a dotted canvas with its edges, a Graph/List switch, a filter, zoom
//! and fit — and, for the selected node, an inspector whose Stream tab shows
//! the agent's session the way Claude Code's terminal does: navigable when the
//! node is done, followed live while it works, with steering.
//!
//! Node data comes from `mini-ork board node <run> <node> --view …`.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use gpui::{
    AnimationExt as _, AnyElement, App, Bounds, ClickEvent, Context, DismissEvent, EventEmitter,
    FocusHandle, Focusable, KeyDownEvent, PathBuilder, Pixels, Point, Render, ScrollHandle,
    SharedString, Subscription, Task, WeakEntity, Window, canvas, fill, point, px, size,
};
use serde_json::Value;
use ui::{CommonAnimationExt as _, DiffStat, prelude::*};
use ui_input::{ERASED_EDITOR_FACTORY, ErasedEditor, ErasedEditorEvent};
use workspace::Workspace;

use crate::{
    board,
    components::{ThrottledSpinExt as _, diff_lines},
    page::{MiniOrkPage, arr, str_of},
    select::{Look, Texts},
    palette::{self as p, c},
    store::{self, BoardStore, Origin},
};

const NW: f32 = 176.;
const NH: f32 = 76.;
const GX: f32 = 74.;
const GY: f32 = 18.;
/// The rail under the node band: an edge the columns cannot draw as a step — a
/// `retries` loop back to an earlier stage, or a forward edge that skips one —
/// leaves its node, drops into this band and runs along it. Each such edge takes
/// a lane of its own, so no two of them ever draw over one another, and the
/// canvas reserves room for the deepest lane.
const RAIL_TOP_GAP: f32 = 34.;
const RAIL_LANE_GAP: f32 = 14.;
const RAIL_BOTTOM_PAD: f32 = 16.;
/// How far past a node's own column an edge that skips a stage runs before it
/// turns down, so its stub clears the nodes in between instead of crossing them.
const DETOUR: f32 = GX / 2.;
/// The felt around the graph inside the scroll container.
const CANVAS_PAD: f32 = 28.;
/// How near a click has to land to an edge to pick it, in screen pixels. The hit
/// test divides it by the zoom, so picking an edge is as easy on a graph fitted
/// to half its size as on one at 1:1.
const EDGE_HIT: f32 = 5.;
const STREAM_POLL: Duration = Duration::from_millis(1500);
/// Height of the DAG strip at the top of a node tab — enough for a recipe's
/// deepest column plus its head labels, without crowding the detail below.
const NODE_TAB_DAG_H: f32 = 280.;
/// The node inspector, in reading order: what happened, the live work, the
/// code it changed, what it was given and produced, then the raw material,
/// and last the harness the node was wired to.
const TABS: [(&str, &str); 9] = [
    ("overview", "Overview"),
    ("stream", "Stream"),
    ("changes", "Changes"),
    ("artifacts", "Artifacts"),
    ("prompt", "Prompt"),
    ("output", "Output"),
    ("telemetry", "Telemetry"),
    ("learning", "Learning"),
    ("harness", "Harness"),
];

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub lane: String,
    pub state: String,
    pub role: String,
    pub cost: String,
    pub dur: String,
    col: usize,
    row: usize,
    len: usize,
}

/// One declared edge, as the engine sends it: two node ids and the relationship
/// between them. `kind` is empty only for the legacy fan-out (see `spans`).
#[derive(Clone, Debug)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

/// One edge resolved to the two nodes it joins, so the paint closure can draw
/// it without looking anything up.
#[derive(Clone, Debug)]
pub(crate) struct Span {
    pub from: Node,
    pub to: Node,
    pub kind: String,
}

/// One edge resolved to the polyline the canvas paints for it: the two nodes it
/// joins and the corners between them (see `Model::routes`). Every segment is
/// straight, and each end sits on a port of its own, so two edges meeting at one
/// node never draw over each other.
#[derive(Clone, Debug)]
pub(crate) struct Route {
    pub span: Span,
    pub points: Vec<(f32, f32)>,
    /// A loop rather than a step: a `retries` edge back to an earlier stage, or
    /// one that ends level with where it started. Drawn broken, so it never
    /// reads as progress.
    pub back: bool,
}

/// The graph as the run page's DAG section describes it.
#[derive(Clone, Debug, Default)]
pub struct Model {
    pub run_id: String,
    pub recipe: String,
    pub title: String,
    pub state: String,
    pub heads: Vec<String>,
    pub cols: Vec<Vec<Node>>,
    /// The recipe's declared edges. Empty against an engine that predates them.
    pub edges: Vec<Edge>,
}

impl Model {
    pub fn from_page(page: &Value, run_id: &str) -> Option<Self> {
        // Spec v2: the page carries a top-level `graph` object with the columns
        // and the run's title/recipe/state already resolved. A non-object value
        // (for example `"graph": null`) and the absent case both fall back to
        // the older `dag` section + `sub`/`chips` page fields, so the IDE
        // still runs against an engine that hasn't been upgraded.
        let (cols_source, heads_source, edges_source, recipe, title, state) = match page
            .get("graph")
            .filter(|graph| graph.is_object())
        {
            Some(graph) => (arr(graph, "cols"), arr(graph, "heads"), arr(graph, "edges"), str_of(graph, "recipe"), str_of(graph, "run_title"), str_of(graph, "state")),
            None => {
                let dag = arr(page, "sections").into_iter().find(|s| str_of(s, "type") == "dag")?;
                let sub = str_of(page, "sub");
                let mut parts = sub.split(" · ");
                let recipe = str_of(&dag, "recipe");
                let recipe = if recipe.is_empty() { parts.next().unwrap_or_default().to_string() } else { recipe };
                let title = str_of(&dag, "run_title");
                let title = if title.is_empty() { parts.next().unwrap_or_default().to_string() } else { title };
                let state = arr(page, "chips").first().map(|c| str_of(c, "t")).unwrap_or_default();
                (arr(&dag, "cols"), arr(&dag, "heads"), arr(&dag, "edges"), recipe, title, state)
            }
        };
        let mut cols: Vec<Vec<Node>> = Vec::new();
        for (ci, column) in cols_source.iter().enumerate() {
            let nodes = column.as_array().cloned().unwrap_or_default();
            let len = nodes.len();
            cols.push(
                nodes
                    .iter()
                    .enumerate()
                    .map(|(row, n)| Node {
                        id: str_of(n, "id"),
                        label: str_of(n, "label"),
                        lane: str_of(n, "lane"),
                        state: str_of(n, "state"),
                        role: str_of(n, "role"),
                        cost: str_of(n, "cost"),
                        dur: str_of(n, "dur"),
                        col: ci,
                        row,
                        len,
                    })
                    .collect(),
            );
        }
        let heads = {
            let given: Vec<String> = heads_source
                .into_iter()
                .filter_map(|h| h.as_str().map(String::from))
                .collect();
            if given.len() == cols.len() {
                given
            } else {
                (0..cols.len())
                    .map(|i| {
                        cols[i]
                            .first()
                            .map(|n: &Node| if n.role.is_empty() { format!("stage {}", i + 1) } else { n.role.clone() })
                            .unwrap_or_default()
                    })
                    .collect()
            }
        };
        // An edge the engine sent but the drawing has no room for (one end is a
        // node the run never reached, or a node talking to itself) is dropped
        // here rather than at paint time: `spans` walks the drawn nodes only.
        let edges = edges_source
            .into_iter()
            .filter_map(|e| {
                let from = str_of(&e, "from");
                let to = str_of(&e, "to");
                if from.is_empty() || to.is_empty() || from == to {
                    return None;
                }
                let kind = str_of(&e, "edge_type");
                Some(Edge {
                    from,
                    to,
                    kind: if kind.is_empty() { "depends_on".to_string() } else { kind },
                })
            })
            .collect();
        Some(Self {
            run_id: run_id.to_string(),
            recipe,
            title,
            state,
            heads,
            cols,
            edges,
        })
    }

    /// The edges to draw, resolved to their endpoints.
    ///
    /// The engine sends the recipe's own edge list. An engine that predates it
    /// sends none, and the fallback keeps the picture that engine's IDE drew:
    /// every node of one column to every node of the next, which reads as a
    /// graph even though it names no relationship.
    pub(crate) fn spans(&self) -> Vec<Span> {
        let by_id: HashMap<&str, &Node> = self.flat().into_iter().map(|n| (n.id.as_str(), n)).collect();
        let mut spans: Vec<Span> = self
            .edges
            .iter()
            .filter_map(|e| {
                let from = *by_id.get(e.from.as_str())?;
                let to = *by_id.get(e.to.as_str())?;
                (from.id != to.id).then(|| Span { from: from.clone(), to: to.clone(), kind: e.kind.clone() })
            })
            .collect();
        if spans.is_empty() {
            for pair in self.cols.windows(2) {
                for from in &pair[0] {
                    for to in &pair[1] {
                        spans.push(Span { from: from.clone(), to: to.clone(), kind: String::new() });
                    }
                }
            }
        }
        spans
    }

    fn flat(&self) -> Vec<&Node> {
        self.cols.iter().flatten().collect()
    }

    /// The node the run reached last: the furthest one in DAG order whose
    /// state says it ran. Columns are stages, so the last stage holding a
    /// non-pending node holds the most recent work — and on a live run that
    /// is the node currently working. A page opened on a run puts this node
    /// in front of the reader, the same detail a click on it would show.
    pub(crate) fn last_ran(&self) -> Option<&Node> {
        self.cols
            .iter()
            .flatten()
            .filter(|n| matches!(n.state.as_str(), "done" | "running" | "failed"))
            .last()
    }

    fn max_rows(&self) -> usize {
        self.cols.iter().map(Vec::len).max().unwrap_or(1).max(1)
    }

    fn width(&self) -> f32 {
        (self.cols.len() as f32 * (NW + GX) - GX).max(NW)
    }

    fn height(&self) -> f32 {
        // A detouring edge is drawn below the band, so the drawing is taller
        // than the nodes are and the container has to be told: the deepest lane
        // is what the canvas must fit.
        let rails = self.rail_count();
        if rails == 0 {
            self.band()
        } else {
            self.band() + RAIL_TOP_GAP + (rails - 1) as f32 * RAIL_LANE_GAP + RAIL_BOTTOM_PAD
        }
    }

    /// The bottom of the lowest node, and so the top of the rail beneath it.
    fn band(&self) -> f32 {
        26. + self.max_rows() as f32 * (NH + GY) - GY
    }

    /// How many edges need a lane of the rail: the ones the columns cannot
    /// express as a step. Counted rather than detected, because each one draws
    /// in a lane of its own, which is what `height` has to make room for.
    fn rail_count(&self) -> usize {
        self.spans().iter().filter(|s| s.to.col != s.from.col + 1).count()
    }

    /// Every drawn edge as the polyline the canvas paints, in `spans` order.
    ///
    /// A span gives an edge two node boxes and nothing else, and drawing box to
    /// box is what made a fan read as one thick line: three judges into one
    /// synthesizer all left the same point on the left and all arrived at the
    /// same point on the right. Here each edge gets a port of its own on the
    /// side it uses, and a node's ports are fanned in the order of the nodes the
    /// fan reaches, so the lines of a fan come out parallel and never cross.
    ///
    /// An edge the columns cannot express as a step — a loop back to an earlier
    /// stage, or a step that skips one — cannot be a straight line between
    /// neighbouring columns either, or it would cut through the nodes in
    /// between. Those take a lane of the rail below the band.
    pub(crate) fn routes(&self) -> Vec<Route> {
        let spans = self.spans();
        let port = |ports: &[f32], at: usize| ports.get(at).copied().unwrap_or(0.5);

        // Which side of a node each edge leaves and enters by, with the y (or x,
        // for the rail) of the other end as its sort key.
        let mut right: Vec<(usize, f32)> = Vec::new();
        let mut left: Vec<(usize, f32)> = Vec::new();
        let mut under_out: Vec<(usize, f32)> = Vec::new();
        let mut under_in: Vec<(usize, f32)> = Vec::new();
        let mut lanes: Vec<(usize, f32)> = Vec::new();
        for (i, span) in spans.iter().enumerate() {
            if span.to.col == span.from.col + 1 {
                right.push((i, self.pos(&span.to).1));
                left.push((i, self.pos(&span.from).1));
            } else {
                lanes.push((i, self.pos(&span.from).0.min(self.pos(&span.to).0)));
                if span.to.col > span.from.col {
                    right.push((i, self.pos(&span.to).1));
                    left.push((i, self.pos(&span.from).1));
                } else {
                    under_out.push((i, self.pos(&span.to).0));
                    under_in.push((i, self.pos(&span.from).0));
                }
            }
        }
        let right = spread(right, spans.len());
        let left = spread(left, spans.len());
        let under_out = spread(under_out, spans.len());
        let under_in = spread(under_in, spans.len());
        // One lane per detouring edge, in the order the edges start, so the lanes
        // stack the way the columns run and never move between renders.
        lanes.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let lane_of: HashMap<usize, usize> =
            lanes.iter().enumerate().map(|(lane, (edge, _))| (*edge, lane)).collect();

        spans
            .into_iter()
            .enumerate()
            .map(|(i, span)| {
                let (ax, ay) = self.pos(&span.from);
                let (bx, by) = self.pos(&span.to);
                let back = span.to.col <= span.from.col;
                let lane = |lane: usize| self.band() + RAIL_TOP_GAP + lane as f32 * RAIL_LANE_GAP;
                let points = if span.to.col == span.from.col + 1 {
                    let (y1, y2) = (ay + NH * port(&right, i), by + NH * port(&left, i));
                    vec![(ax + NW, y1), (bx, y2)]
                } else if back {
                    let (x1, x2) = (ax + NW * port(&under_out, i), bx + NW * port(&under_in, i));
                    let y = lane(lane_of.get(&i).copied().unwrap_or(0));
                    vec![(x1, ay + NH), (x1, y), (x2, y), (x2, by + NH)]
                } else {
                    // Over the rail and back up: it skips a stage, so it may not
                    // cross the stages in between.
                    let (y1, y2) = (ay + NH * port(&right, i), by + NH * port(&left, i));
                    let y = lane(lane_of.get(&i).copied().unwrap_or(0));
                    vec![
                        (ax + NW, y1),
                        (ax + NW + DETOUR, y1),
                        (ax + NW + DETOUR, y),
                        (bx - DETOUR, y),
                        (bx - DETOUR, y2),
                        (bx, y2),
                    ]
                };
                Route { span, points, back }
            })
            .collect()
    }

    fn pos(&self, n: &Node) -> (f32, f32) {
        let x = n.col as f32 * (NW + GX);
        let y = 26. + (self.max_rows() - n.len) as f32 * (NH + GY) / 2. + n.row as f32 * (NH + GY);
        (x, y)
    }
}

/// The zoom at which a run's whole DAG fits `width` after `gutter` of chrome.
///
/// Both hosts that show the canvas unprompted (a node tab's strip, the run
/// page's DAG section) want the same thing: the rightmost node visible without
/// panning, and never blown up past 1:1 — a three-node recipe stretched to fill
/// a window reads as a different picture of the same run.
pub(crate) fn fit_zoom(width: f32, model: &Model, gutter: f32) -> f32 {
    let w = model.width();
    if width <= gutter || w <= 0. {
        return 1.;
    }
    ((width - gutter) / w).clamp(0.4, 1.)
}

/// Where along a node's side each of the edges that use that side meets it, as
/// a fraction of the side, indexed by the edge. The fan is spread evenly, one
/// port per edge, in the order of the nodes it reaches — a node with one edge
/// out of it keeps that edge at its middle, and a node with three gets thirds.
/// Before this, every edge used the middle, which is why a fan drew as one line.
fn spread(ports: Vec<(usize, f32)>, count: usize) -> Vec<f32> {
    let mut ports = ports;
    ports.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let total = ports.len() as f32;
    let mut spread = vec![0.5; count];
    for (rank, (edge, _)) in ports.iter().enumerate() {
        if let Some(slot) = spread.get_mut(*edge) {
            *slot = (rank + 1) as f32 / (total + 1.);
        }
    }
    spread
}

/// The edge under a point of the model, if the point is on one and not on a
/// node. The canvas paints the nodes over the edges that run beneath them, so a
/// point inside a node box belongs to the node, whichever edge passes under it.
fn route_at(routes: &[Route], model: &Model, x: f32, y: f32, slop: f32) -> Option<(String, String)> {
    let on_node = model.flat().into_iter().any(|node| {
        let (nx, ny) = model.pos(node);
        x >= nx && x <= nx + NW && y >= ny && y <= ny + NH
    });
    if on_node {
        return None;
    }
    routes.iter().find_map(|route| {
        let near = route.points.windows(2).any(|segment| match segment {
            [from, to] => distance_to_segment((x, y), *from, *to) <= slop,
            _ => false,
        });
        near.then(|| (route.span.from.id.clone(), route.span.to.id.clone()))
    })
}

/// How far a point lies from a segment, in the same units: the distance to the
/// nearest point of the segment, which is the distance to its line once the
/// projection has been clamped to its two ends.
fn distance_to_segment((x, y): (f32, f32), (x1, y1): (f32, f32), (x2, y2): (f32, f32)) -> f32 {
    let (dx, dy) = (x2 - x1, y2 - y1);
    let length_squared = dx * dx + dy * dy;
    let along = if length_squared > f32::EPSILON {
        (((x - x1) * dx + (y - y1) * dy) / length_squared).clamp(0., 1.)
    } else {
        0.
    };
    let (near_x, near_y) = (x1 + along * dx, y1 + along * dy);
    ((x - near_x).powi(2) + (y - near_y).powi(2)).sqrt()
}

#[derive(Clone, Copy)]
enum NavTo {
    First,
    Prev,
    Next,
    Last,
}

/// Tool results longer than this show their first lines and "+N lines".
const COLLAPSED_LINES: usize = 4;
/// How many stream entries render at once. A live agent's transcript grows
/// for hours, and every frame lays out every entry on screen — the window
/// keeps that bounded, with "show earlier" to extend it.
const STREAM_WINDOW: usize = 400;

fn mark(state: &str) -> (&'static str, u32) {
    match state {
        "done" => ("✓", p::GREEN),
        "running" => ("●", p::BLUE),
        "failed" => ("✗", p::RED),
        "skipped" => ("–", p::DIM),
        _ => ("○", p::DIM),
    }
}

fn state_text(state: &str) -> &str {
    match state {
        "running" => "working",
        "" => "pending",
        other => other,
    }
}

pub struct MiniOrkGraph {
    workspace: WeakEntity<Workspace>,
    page: WeakEntity<MiniOrkPage>,
    focus_handle: FocusHandle,
    model: Model,
    list_view: bool,
    zoom: f32,
    filter: Arc<dyn ErasedEditor>,
    query: String,
    canvas_scroll: ScrollHandle,
    /// Where a canvas drag started (mouse, scroll offset), while panning.
    drag: std::rc::Rc<std::cell::Cell<Option<(Point<Pixels>, Point<Pixels>)>>>,
    selected: Option<String>,
    /// The edge the user picked on the canvas, by its two node ids. The canvas
    /// draws its endpoints and fades everything else until the pick is cleared,
    /// which a click on the background does (see `render_canvas`).
    selected_edge: Option<(String, String)>,
    tab: String,
    /// Detail per (node, tab), so switching back is instant.
    detail: HashMap<(String, String), Value>,
    detail_error: Option<String>,
    loading: bool,
    stream: Vec<Value>,
    stream_offset: u64,
    /// How many of the visible entries render, counted from the end. The
    /// stream's window: `STREAM_WINDOW` unless the user asked for more.
    stream_shown: usize,
    stream_scroll: ScrollHandle,
    /// Stream navigation: which kinds are shown, the selected entry (an index
    /// into `stream`), and tool results expanded past their first lines.
    stream_filter: &'static str,
    stream_cursor: Option<usize>,
    expanded: HashSet<usize>,
    /// Artifact previews the user opened (by path).
    art_open: HashSet<String>,
    steer: Arc<dyn ErasedEditor>,
    severity: &'static str,
    texts: Texts,
    /// Hosted in a node tab: the run's DAG on top (the node selected), the
    /// node's own inspector below, full width.
    pub tab_mode: bool,
    /// The node tab's DAG strip has fit its zoom to the strip's width once
    /// (re-armed by `set_model`), so the whole graph — including the
    /// rightmost node — is visible without scrolling.
    tab_fit: bool,
    /// The stream is showing full screen: in a node tab the DAG strip above it
    /// is dropped so the transcript gets the whole pane, and in the full-screen
    /// graph the inspector expands over the canvas it normally sits beside.
    /// Toggled from the stream's own header; escape clears it.
    stream_full: bool,
    _load: Task<()>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for MiniOrkGraph {}

impl Focusable for MiniOrkGraph {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl MiniOrkGraph {
    pub fn new(
        model: Model,
        selected: Option<String>,
        workspace: WeakEntity<Workspace>,
        page: WeakEntity<MiniOrkPage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let factory = ERASED_EDITOR_FACTORY.get().expect("editor factory is set at start-up");
        let filter = factory(window, cx);
        filter.set_placeholder_text("Filter nodes, lanes, status…", window, cx);
        let steer = factory(window, cx);
        steer.set_placeholder_text("Steer this agent… (Enter to send)", window, cx);
        let filter_sub = filter.subscribe(
            Box::new({
                let this = cx.entity().downgrade();
                let editor = filter.clone();
                move |event, _window, cx| {
                    if event == ErasedEditorEvent::BufferEdited {
                        let text = editor.text(cx);
                        this.update(cx, |this, cx| {
                            this.query = text.trim().to_lowercase();
                            cx.notify();
                        })
                        .ok();
                    }
                }
            }),
            window,
            cx,
        );
        let mut this = Self {
            workspace,
            page,
            focus_handle: cx.focus_handle(),
            model,
            list_view: false,
            zoom: 1.0,
            filter,
            query: String::new(),
            canvas_scroll: ScrollHandle::new(),
            drag: Default::default(),
            selected: None,
            selected_edge: None,
            tab: "overview".into(),
            detail: HashMap::new(),
            detail_error: None,
            loading: false,
            stream: Vec::new(),
            stream_offset: 0,
            stream_shown: STREAM_WINDOW,
            stream_scroll: ScrollHandle::new(),
            stream_filter: "all",
            stream_cursor: None,
            expanded: HashSet::new(),
            art_open: HashSet::new(),
            steer,
            severity: "info",
            texts: Texts::default(),
            tab_mode: false,
            tab_fit: false,
            stream_full: false,
            _load: Task::ready(()),
            _subs: vec![filter_sub],
        };
        if let Some(node) = selected {
            this.select(node, cx);
        }
        this
    }

    /// The page re-read the run: node states, costs and durations move on.
    pub fn set_model(&mut self, model: Model, cx: &mut Context<Self>) {
        self.model = model;
        if self.tab_mode {
            self.tab_fit = false; // the graph may be wider now: re-fit
        }
        cx.notify();
    }

    fn node(&self, id: &str) -> Option<&Node> {
        self.model.flat().into_iter().find(|n| n.id == id)
    }

    fn matches(&self, n: &Node) -> bool {
        self.query.is_empty()
            || [n.label.as_str(), n.lane.as_str(), n.state.as_str(), n.role.as_str()]
                .join(" ")
                .to_lowercase()
                .contains(&self.query)
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn select(&mut self, node: String, cx: &mut Context<Self>) {
        if self.selected.as_deref() != Some(node.as_str()) {
            self.stream.clear();
            self.stream_offset = 0;
            self.stream_shown = STREAM_WINDOW;
            self.stream_cursor = None;
            self.expanded.clear();
        }
        self.selected = Some(node);
        // A node and an edge are not both in focus: the node's own edges go blue
        // under its inspector, and leaving a picked edge standing would fade the
        // very graph the click asked to look at.
        self.selected_edge = None;
        self.load_detail(cx);
        cx.notify();
    }

    fn set_tab(&mut self, tab: &str, cx: &mut Context<Self>) {
        self.tab = tab.to_string();
        // Full screen belongs to the stream; leaving it drops the flag so the
        // DAG strip does not stay hidden behind another tab.
        self.stream_full = false;
        self.load_detail(cx);
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let flat: Vec<String> = self.model.flat().iter().map(|n| n.id.clone()).collect();
        if flat.is_empty() {
            return;
        }
        let ix = self
            .selected
            .as_ref()
            .and_then(|s| flat.iter().position(|id| id == s))
            .unwrap_or(0) as isize;
        let next = (ix + delta).rem_euclid(flat.len() as isize) as usize;
        self.select(flat[next].clone(), cx);
    }

    fn target(&self, cx: &App) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let workspace = self.workspace.upgrade()?;
        let store = BoardStore::for_workspace(&workspace, cx)?;
        let store = store.read(cx);
        Some((store.bin.clone(), store.home.clone()?))
    }

    /// Fetch the selected node's tab; a live stream keeps following by offset.
    fn load_detail(&mut self, cx: &mut Context<Self>) {
        let Some(node) = self.selected.clone() else {
            return;
        };
        let Some((bin, home)) = self.target(cx) else {
            return;
        };
        let (run, tab) = (self.model.run_id.clone(), self.tab.clone());
        self.loading = true;
        self.detail_error = None;
        self._load = cx.spawn(async move |this, cx| {
            // When a followed node stops being live, read it once more in full:
            // the final note and state only come with a full read.
            let mut was_live = false;
            loop {
                let offset = this
                    .update(cx, |this, _| (tab == "stream" && this.stream_offset > 0).then_some(this.stream_offset))
                    .ok()
                    .flatten();
                let result = cx
                    .background_spawn(board::fetch_node(
                        bin.clone(),
                        home.clone(),
                        run.clone(),
                        node.clone(),
                        tab.clone(),
                        offset,
                    ))
                    .await;
                let live = this
                    .update(cx, |this, cx| {
                        if this.selected.as_deref() != Some(node.as_str()) || this.tab != tab {
                            return false;
                        }
                        this.loading = false;
                        let value = match result {
                            Ok(value) => value,
                            Err(error) => {
                                let error = error.to_string();
                                // A mini-ork that predates the Overview: show the stream instead.
                                if tab == "overview" && error.contains("invalid choice") {
                                    this.tab = "stream".into();
                                    this.load_detail(cx);
                                    return false;
                                }
                                this.detail_error = Some(error);
                                cx.notify();
                                return false;
                            }
                        };
                        let live = value.get("live").and_then(Value::as_bool).unwrap_or(false);
                        if tab == "stream" {
                            let entries = arr(&value, "entries");
                            let grew = !entries.is_empty();
                            if offset.is_some() {
                                this.stream.extend(entries);
                            } else {
                                this.stream = entries;
                            }
                            this.stream_offset = value.get("offset").and_then(Value::as_u64).unwrap_or(this.stream_offset);
                            if grew && live && this.stream_cursor.is_none() {
                                this.stream_scroll.scroll_to_bottom();
                            }
                        }
                        this.detail.insert((node.clone(), tab.clone()), value);
                        cx.notify();
                        live && tab == "stream"
                    })
                    .unwrap_or(false);
                if !live {
                    if was_live && tab == "stream" {
                        was_live = false;
                        this.update(cx, |this, _| this.stream_offset = 0).ok();
                        continue;
                    }
                    break;
                }
                was_live = true;
                cx.background_executor().timer(STREAM_POLL).await;
            }
        });
    }

    /// Stream entries shown under the current kind filter (indices into `stream`).
    fn visible(&self) -> Vec<usize> {
        self.stream
            .iter()
            .enumerate()
            .filter(|(_, e)| match self.stream_filter {
                "all" => true,
                "text" => matches!(str_of(e, "k").as_str(), "text" | "user" | "note"),
                kind => str_of(e, "k") == kind,
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Move the selection among visible entries and scroll it to the top.
    fn navigate(&mut self, to: NavTo, cx: &mut Context<Self>) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let at = self.stream_cursor.and_then(|c| visible.iter().position(|&i| i == c));
        let pos = match to {
            NavTo::First => 0,
            NavTo::Last => visible.len() - 1,
            NavTo::Next => at.map_or(0, |p| (p + 1).min(visible.len() - 1)),
            NavTo::Prev => at.map_or(visible.len() - 1, |p| p.saturating_sub(1)),
        };
        self.stream_cursor = Some(visible[pos]);
        // A jump above the window extends it rather than scrolling nowhere.
        let render_from = visible.len() - visible.len().min(self.stream_shown);
        if pos < render_from {
            self.stream_shown = visible.len() - pos;
        }
        let render_from = visible.len() - visible.len().min(self.stream_shown);
        self.stream_scroll.scroll_to_top_of_item(pos - render_from);
        cx.notify();
    }

    fn set_filter(&mut self, filter: &'static str, cx: &mut Context<Self>) {
        self.stream_filter = filter;
        self.stream_cursor = None;
        self.stream_scroll.scroll_to_top_of_item(0);
        cx.notify();
    }

    fn send_steer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.steer.text(cx).trim().to_string();
        let Some(node) = self.selected.as_ref().and_then(|id| self.node(id)).cloned() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.steer.clear(window, cx);
        let role = if node.role.is_empty() { "any".to_string() } else { node.role.clone() };
        let args = vec![
            "board".to_string(),
            "steer".into(),
            self.model.run_id.clone(),
            "--role".into(),
            role,
            "--severity".into(),
            self.severity.into(),
            "--text".into(),
            text,
        ];
        if let Some(workspace) = self.workspace.upgrade() {
            let page = self.page.clone();
            window.defer(cx, move |_, cx| {
                store::run_cli(args, true, &workspace, Origin::Page(page), cx);
            });
        }
    }

    fn act(&self, action: Value) -> Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static> {
        let (workspace, page) = (self.workspace.clone(), self.page.clone());
        Box::new(move |_, window, cx| {
            let (action, workspace, page) = (action.clone(), workspace.clone(), page.clone());
            window.defer(cx, move |window, cx| {
                if let Some(workspace) = workspace.upgrade() {
                    store::perform(&action, &workspace, Origin::Page(page), window, cx);
                }
            });
        })
    }

    // ── header ────────────────────────────────────────────────────────────

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let flat = self.model.flat();
        let count = |state: &str| flat.iter().filter(|n| n.state == state).count();
        let counts = [
            (count("done"), "done", p::GREEN),
            (count("running"), "working", p::BLUE),
            (flat.iter().filter(|n| n.state.is_empty() || n.state == "pending").count(), "pending", p::SUB),
            (count("failed"), "failed", p::RED),
        ];
        let (m, mc) = mark(if self.model.state.contains("running") { "running" } else if self.model.state.contains("fail") { "failed" } else { "done" });
        let mono = p::mono_font(cx);
        let small = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .size(rems_from_px(26_f32))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .border_color(c(p::BORDER2))
                .text_color(c(p::TEXT))
                .cursor_pointer()
                .hover(|s| s.bg(c(p::HOVER)))
                .child(label)
        };
        h_flex()
            .flex_none()
            .h(rems_from_px(46_f32))
            .gap_2p5()
            .pl_4()
            .pr_3()
            .bg(c(p::CHROME))
            .border_b_1()
            .border_color(c(p::BORDER))
            .whitespace_nowrap()
            .child(p::text(m, 13., mc))
            .child(div().text_size(rems_from_px(13_f32)).font(mono.clone()).text_color(c(p::TEXT)).child(self.model.run_id.clone()))
            .when(!self.model.recipe.is_empty(), |this| {
                this.child(
                    div()
                        .px_1p5()
                        .border_1()
                        .border_color(c(p::BORDER2))
                        .text_size(rems_from_px(11_f32))
                        .font(mono.clone())
                        .text_color(c(p::MUTED))
                        .child(self.model.recipe.clone()),
                )
            })
            .child(p::text(self.model.title.clone(), 12.5, p::MUTED).max_w(rems_from_px(260_f32)).truncate())
            .children(
                counts
                    .into_iter()
                    .filter(|(n, _, _)| *n > 0)
                    .map(|(n, label, colour)| p::text(format!("{n} {label}"), 11.5, colour)),
            )
            .child(div().flex_1())
            .child(
                h_flex()
                    .border_1()
                    .border_color(c(p::BORDER2))
                    .overflow_hidden()
                    .children([(false, "Graph"), (true, "List")].into_iter().map(|(list, label)| {
                        let on = self.list_view == list;
                        div()
                            .id(SharedString::from(format!("graph-view-{label}")))
                            .h(rems_from_px(26_f32))
                            .px_2p5()
                            .flex()
                            .items_center()
                            .when(on, |this| this.bg(c(p::SELECTED)))
                            .text_size(rems_from_px(12_f32))
                            .text_color(c(if on { p::TEXT } else { p::MUTED }))
                            .cursor_pointer()
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.list_view = list;
                                cx.notify();
                            }))
                    })),
            )
            .child(
                div()
                    .w(rems_from_px(190_f32))
                    .h_7()
                    .px(rems_from_px(9_f32))
                    .flex()
                    .items_center()
                    .border_1()
                    .border_color(c(p::BORDER2))
                    .bg(c(p::BG))
                    .text_size(rems_from_px(12.5_f32))
                    .child(self.filter.render(window, cx)),
            )
            .when(!self.list_view, |this| {
                this.child(
                    h_flex()
                        .gap_0p5()
                        .child(small("graph-zoom-out", "−").on_click(cx.listener(|this, _, _, cx| {
                            this.zoom = (this.zoom - 0.1).max(0.4);
                            cx.notify();
                        })))
                        .child(
                            div()
                                .w(rems_from_px(46_f32))
                                .text_center()
                                .text_size(rems_from_px(11.5_f32))
                                .font(mono.clone())
                                .text_color(c(p::MUTED))
                                .child(format!("{}%", (self.zoom * 100.).round() as i32)),
                        )
                        .child(small("graph-zoom-in", "+").on_click(cx.listener(|this, _, _, cx| {
                            this.zoom = (this.zoom + 0.1).min(1.8);
                            cx.notify();
                        })))
                        .child(
                            div()
                                .id("graph-fit")
                                .ml_1()
                                .h(rems_from_px(26_f32))
                                .px(rems_from_px(9_f32))
                                .flex()
                                .items_center()
                                .border_1()
                                .border_color(c(p::BORDER2))
                                .text_size(rems_from_px(12_f32))
                                .text_color(c(p::TEXT))
                                .cursor_pointer()
                                .hover(|s| s.bg(c(p::HOVER)))
                                .child("Fit")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let view = this.canvas_scroll.bounds().size;
                                    let (w, h) = (this.model.width(), this.model.height());
                                    if f32::from(view.width) > 0. {
                                        let z = ((f32::from(view.width) - 56.) / w).min((f32::from(view.height) - 56.) / h);
                                        this.zoom = (z.clamp(0.4, 1.4) * 100.).round() / 100.;
                                        cx.notify();
                                    }
                                })),
                        ),
                )
            })
            .child(
                div()
                    .id("graph-close")
                    .h_7()
                    .px_2p5()
                    .flex()
                    .items_center()
                    .border_1()
                    .border_color(c(p::BORDER2))
                    .bg(c(p::HOVER))
                    .text_size(rems_from_px(12.5_f32))
                    .text_color(c(p::TEXT))
                    .cursor_pointer()
                    .child("✕ Close")
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            )
            .into_any_element()
    }
}


impl MiniOrkGraph {
    // ── canvas ────────────────────────────────────────────────────────────

    /// The run graph's canvas (see `render_canvas`), drawing the graph's own
    /// zoom, selection and pan state.
    fn render_graph(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let on_node = std::rc::Rc::new({
            let this = this.clone();
            move |node: &str, _: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| this.select(node.to_string(), cx)).ok();
            }
        });
        let on_edge = std::rc::Rc::new(move |edge: Option<(String, String)>, _: &mut Window, cx: &mut App| {
            this.update(cx, |this, cx| {
                if this.selected_edge != edge {
                    this.selected_edge = edge;
                    cx.notify();
                }
            })
            .ok();
        });
        render_canvas(
            &self.model,
            self.zoom,
            self.selected.as_deref(),
            &self.query,
            &self.canvas_scroll,
            &self.drag,
            self.selected_edge.clone(),
            on_node,
            on_edge,
            cx,
        )
    }
}

/// The DAG canvas: dot grid, straight edges, column heads, nodes. Shared by the
/// run graph overlay, a node tab's strip and the run page's DAG section, so the
/// three cannot drift into looking like three renderers of the same run.
///
/// `selected_edge` is the edge the user picked, by its two node ids: its
/// endpoints keep their weight and everything else fades behind it, until a
/// click on the background clears it. `on_node` and `on_edge` are the two things
/// the hosts disagree on: the overlay and the node tab select the node into
/// their inspector and the run page opens the node's own tab, while the edge
/// pick is the canvas' own state, held by whoever draws it and set through the
/// callback.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_canvas(
    model: &Model,
    zoom: f32,
    selected: Option<&str>,
    query: &str,
    scroll: &ScrollHandle,
    drag: &std::rc::Rc<std::cell::Cell<Option<(Point<Pixels>, Point<Pixels>)>>>,
    selected_edge: Option<(String, String)>,
    on_node: std::rc::Rc<dyn Fn(&str, &mut Window, &mut App)>,
    on_edge: std::rc::Rc<dyn Fn(Option<(String, String)>, &mut Window, &mut App)>,
    cx: &mut App,
) -> AnyElement {
    // The state this canvas reads is the renderer's, not the store's: owned
    // here so the closures below can capture it and outlive the call.
    let selected = selected.map(String::from);
    let matches = |n: &Node| {
        query.is_empty()
            || [n.label.as_str(), n.lane.as_str(), n.state.as_str(), n.role.as_str()]
                .join(" ")
                .to_lowercase()
                .contains(query)
    };
    let z = zoom;
    let model = model.clone();
    let (w, h) = (model.width() * z, model.height() * z);
    let mono = p::mono_font(cx);

    // Edges: the recipe's own list when the engine sends one, else the legacy
    // fan-out, each routed to a straight polyline with a port of its own at both
    // ends (see `Model::routes`). The selected node's edges are blue, edges into
    // pending nodes dim, and an edge the user picked is the only one at full
    // strength.
    let edge_routes = model.routes();
    // The click handler hit-tests against the same routes the painter draws, so
    // the clone is taken before the paint closure takes ownership of them.
    let hit_routes = edge_routes.clone();
    let hit_model = model.clone();
    let edge_query = query.to_string();
    let edge_selected = selected.clone();
    let edge_picked = selected_edge.clone();
    let edges = canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, _, window, _| {
            let origin = bounds.origin;
            let at = |x: f32, y: f32| -> Point<Pixels> { point(origin.x + px(x * z), origin.y + px(y * z)) };
            let matches = |n: &Node| {
                edge_query.is_empty()
                    || [n.label.as_str(), n.lane.as_str(), n.state.as_str(), n.role.as_str()]
                        .join(" ")
                        .to_lowercase()
                        .contains(&edge_query)
            };
            let scale = z.max(0.6);
            for route in &edge_routes {
                let (a, b) = (&route.span.from, &route.span.to);
                let points = &route.points;
                let Some(&(start_x, start_y)) = points.first() else { continue };
                // The arrowhead follows the last straight run, so which way an
                // edge goes is legible without tracing it back to its source.
                let Some((end_x, end_y)) = points.last().copied() else { continue };
                let Some((run_x, run_y)) = points.len().checked_sub(2).and_then(|i| points.get(i)).copied() else { continue };
                let picked = edge_picked.as_ref().is_some_and(|(from, to)| from == &a.id && to == &b.id);
                let faded = edge_picked.is_some() && !picked;
                let hot = picked
                    || (edge_picked.is_none()
                        && edge_selected.as_deref().is_some_and(|s| s == a.id || s == b.id));
                let mut colour = if hot {
                    c(p::BLUE)
                } else if b.state.is_empty() || b.state == "pending" {
                    c(p::BORDER2)
                } else if route.span.kind.is_empty() {
                    c(p::DIM)
                } else {
                    crate::thumb::edge_colour(&route.span.kind)
                };
                if faded {
                    colour = colour.opacity(0.12);
                } else if !(matches(a) && matches(b)) {
                    colour = colour.opacity(0.25);
                }
                let width = if picked { 2.4 } else if hot { 2. } else { 1.3 };
                let mut path = PathBuilder::stroke(px(width * scale));
                if route.back {
                    path = path.dash_array(&[px(5. * scale), px(4. * scale)]);
                }
                path.move_to(at(start_x, start_y));
                for (x, y) in &points[1..] {
                    path.line_to(at(*x, *y));
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, colour);
                }
                let (dx, dy) = (end_x - run_x, end_y - run_y);
                let length = (dx * dx + dy * dy).sqrt();
                if length > 0.001 {
                    let (ux, uy) = (dx / length, dy / length);
                    let (nx, ny) = (-uy, ux);
                    let (tail_x, tail_y) = (end_x - ux * 9., end_y - uy * 9.);
                    let mut head = PathBuilder::fill();
                    head.add_polygon(
                        &[
                            at(end_x, end_y),
                            at(tail_x + nx * 4.5, tail_y + ny * 4.5),
                            at(tail_x - nx * 4.5, tail_y - ny * 4.5),
                        ],
                        true,
                    );
                    if let Ok(head) = head.build() {
                        window.paint_path(head, colour);
                    }
                }
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    let heads = model.heads.iter().enumerate().map(|(i, head)| {
        div()
            .absolute()
            .left(px(i as f32 * (NW + GX) * z))
            .top_0()
            .w(px(NW * z))
            .text_size(px(10.5 * z))
            .text_color(c(p::DIM))
            .child(head.to_uppercase())
            .into_any_element()
    });

    let nodes = model.flat().into_iter().cloned().collect::<Vec<_>>().into_iter().map(|n| {
        let (x, y) = model.pos(&n);
        let on = selected.as_deref() == Some(n.id.as_str());
        // While an edge is picked its two endpoints are the only nodes at full
        // strength, and they carry the pick's own colour — the edge and the two
        // boxes it joins read as one thing.
        let on_edge = selected_edge
            .as_ref()
            .is_some_and(|(from, to)| from == &n.id || to == &n.id);
        let (m, mc) = mark(&n.state);
        let border = if on || on_edge {
            c(p::BLUE)
        } else if n.state == "running" {
            p::ca(0x74ade88c)
        } else if n.state == "failed" {
            p::ca(0xd0727799)
        } else {
            c(p::BORDER2)
        };
        let pending = n.state.is_empty() || n.state == "pending";
        let id = n.id.clone();
        div()
            .id(SharedString::from(format!("graph-node-{}", n.id)))
            .absolute()
            .left(px(x * z))
            .top(px(y * z))
            .w(px(NW * z))
            .h(px(NH * z))
            .px(px(10. * z))
            .py(px(8. * z))
            .flex()
            .flex_col()
            .gap(px(3. * z))
            .border(px(if on { 2. } else { 1. }))
            .border_color(border)
            .bg(c(if pending { 0x2a2e36 } else { 0x323843 }))
            .when(!matches(&n) || (selected_edge.is_some() && !on_edge), |this| this.opacity(0.28))
            .cursor_pointer()
            .hover(|s| s.border_color(c(p::BLUE)))
            .child(
                h_flex()
                    .gap(px(6. * z))
                    .min_w_0()
                    .child(p::lane_mark(&n.lane, 18. * z))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.5 * z))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(c(p::TEXT))
                            .child(n.label.clone()),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(5. * z))
                    .min_w_0()
                    .child(p::text_px(p::lane_label(&n.lane), 11. * z, p::TEXT).truncate())
                    .when(!n.role.is_empty(), |this| {
                        this.child(p::text_px(format!("· {}", n.role), 11. * z, p::SUB).truncate())
                    }),
            )
            .child(
                h_flex()
                    .gap(px(8. * z))
                    .font(mono.clone())
                    .text_size(px(11. * z))
                    .child(div().text_color(c(mc)).child(m))
                    .child(div().text_color(c(mc)).child(state_text(&n.state).to_string()))
                    .child(div().text_color(c(p::SUB)).child(n.cost.clone()))
                    .child(div().text_color(c(p::SUB)).child(n.dur.clone())),
            )
            .on_click({
                            let on_node = on_node.clone();
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| (on_node)(&id, window, cx)
                        })
            .into_any_element()
    });

    let dots = canvas(
        |_, _, _| (),
        |bounds: Bounds<Pixels>, _, window, _| {
            let colour = c(0x353b45);
            let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let mut y = 9.;
            while y < bh {
                let mut x = 9.;
                while x < bw {
                    window.paint_quad(
                        fill(
                            Bounds::new(point(bounds.origin.x + px(x), bounds.origin.y + px(y)), size(px(2.), px(2.))),
                            colour,
                        )
                        .corner_radii(px(1.)),
                    );
                    x += 18.;
                }
                y += 18.;
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    div()
        .relative()
        .flex_1()
        .min_w_0()
        .h_full()
        .bg(c(0x262a31))
        .child(dots)
        .child(
            div()
                .id("graph-canvas")
                .size_full()
                .overflow_scroll()
                .track_scroll(&scroll)
                // Drag the canvas to pan; the wheel/trackpad scrolls both ways.
                .on_mouse_down(gpui::MouseButton::Left, {
                    let drag = drag.clone();
                    let scroll = scroll.clone();
                    move |event: &gpui::MouseDownEvent, _, _| drag.set(Some((event.position, scroll.offset())))
                })
                .on_mouse_up(gpui::MouseButton::Left, {
                    let drag = drag.clone();
                    let scroll = scroll.clone();
                    let on_edge = on_edge.clone();
                    move |event: &gpui::MouseUpEvent, window: &mut Window, cx: &mut App| {
                        // A drag that panned the canvas is not a click, and the
                        // pick it lands on has to survive the pan.
                        let panned = drag.get().is_some_and(|(start, _)| {
                            let (dx, dy) = (
                                f32::from(event.position.x - start.x),
                                f32::from(event.position.y - start.y),
                            );
                            dx * dx + dy * dy > 9.
                        });
                        drag.set(None);
                        if panned {
                            return;
                        }
                        // Where the click landed in the model the canvas drew:
                        // the scroll container's viewport, moved by its offset
                        // and past the felt around the graph, in model units.
                        let bounds = scroll.bounds();
                        let offset = scroll.offset();
                        let x = (f32::from(event.position.x - bounds.origin.x) + f32::from(offset.x)
                            - CANVAS_PAD)
                            / z;
                        let y = (f32::from(event.position.y - bounds.origin.y) + f32::from(offset.y)
                            - CANVAS_PAD)
                            / z;
                        // A click on the background picks nothing, which is what
                        // clears a pick.
                        let hit = route_at(&hit_routes, &hit_model, x, y, EDGE_HIT / z);
                        (on_edge)(hit, window, cx);
                    }
                })
                .on_mouse_move({
                    let drag = drag.clone();
                    let scroll = scroll.clone();
                    move |event: &gpui::MouseMoveEvent, window, _| {
                        if let Some((start, offset)) = drag.get() {
                            if event.pressed_button != Some(gpui::MouseButton::Left) {
                                drag.set(None);
                                return;
                            }
                            scroll.set_offset(point(offset.x + (event.position.x - start.x), offset.y + (event.position.y - start.y)));
                            window.refresh();
                        }
                    }
                })
                .child(
                    div()
                        .flex_none()
                        .w(px(w + CANVAS_PAD * 2.))
                        .h(px(h + CANVAS_PAD * 2.))
                        .p(px(CANVAS_PAD))
                        .child(
                            div()
                                .relative()
                                .w(px(w))
                                .h(px(h))
                                .child(edges)
                                .children(heads)
                                .children(nodes),
                        ),
                ),
        )
        .into_any_element()
}

impl MiniOrkGraph {
    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let mono = p::mono_font(cx);
        let stages = self.model.cols.iter().enumerate().map(|(ci, col)| {
            let head = self.model.heads.get(ci).cloned().unwrap_or_default();
            v_flex()
                .gap_1()
                .child(p::text(format!("{} · {}", ci + 1, head).to_uppercase(), 10.5, p::DIM))
                .children(col.iter().map(|n| {
                    let on = self.selected.as_deref() == Some(n.id.as_str());
                    let (m, mc) = mark(&n.state);
                    let pending = n.state.is_empty() || n.state == "pending";
                    let id = n.id.clone();
                    h_flex()
                        .id(SharedString::from(format!("graph-row-{}", n.id)))
                        .gap_2p5()
                        .px_3()
                        .py_2()
                        .border_1()
                        .border_color(if on { c(p::BLUE) } else { c(p::BORDER2) })
                        .bg(c(if pending { 0x2a2e36 } else { 0x323843 }))
                        .when(!self.matches(n), |this| this.opacity(0.28))
                        .cursor_pointer()
                        .hover(|s| s.border_color(c(p::BLUE)))
                        .text_size(rems_from_px(12.5_f32))
                        .child(p::text(m, 11., mc).w_4().flex_none())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(c(p::TEXT))
                                .child(n.label.clone()),
                        )
                        .child(
                            h_flex()
                                .w(rems_from_px(120_f32))
                                .flex_none()
                                .gap(rems_from_px(5_f32))
                                .child(p::lane_mark(&n.lane, 11.))
                                .child(p::text(p::lane_label(&n.lane), 11.5, p::TEXT).truncate())
                                .when(!n.role.is_empty(), |this| {
                                    this.child(p::text(format!("· {}", n.role), 11.5, p::SUB).truncate())
                                }),
                        )
                        .child(p::text(state_text(&n.state).to_string(), 11.5, mc).w_20().flex_none())
                        .child(p::text(n.cost.clone(), 11.5, p::SUB).font(mono.clone()).w(rems_from_px(60_f32)).flex_none())
                        .child(p::text(n.dur.clone(), 11.5, p::SUB).font(mono.clone()).w(rems_from_px(60_f32)).flex_none())
                        .on_click(cx.listener(move |this, _, _, cx| this.select(id.clone(), cx)))
                }))
        });
        div()
            .id("graph-list")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .bg(c(0x262a31))
            .child(
                // Full width, left aligned, like every other mini-ork view:
                // Zed's own panels lay content out `w_full()` and reserve a
                // max width for individual controls, never for the column the
                // page is read in.
                v_flex()
                    .w_full()
                    .min_w_0()
                    .px_6()
                    .pt_5()
                    .pb_12()
                    .gap_3p5()
                    .children(stages),
            )
            .into_any_element()
    }

    // ── inspector ─────────────────────────────────────────────────────────

    fn render_inspector(&self, node: &Node, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let detail = self.detail.get(&(node.id.clone(), self.tab.clone())).cloned();
        let live = detail
            .as_ref()
            .and_then(|d| d.get("live").and_then(Value::as_bool))
            .unwrap_or(false);
        let (m, mc) = mark(&node.state);
        let stage_count = self.model.cols.len();
        let head = self.model.heads.get(node.col).cloned().unwrap_or_default();
        let is_stream = self.tab == "stream";
        let icon_button = |id: &'static str, glyph: &'static str| {
            div()
                .id(id)
                .size_6()
                .flex()
                .items_center()
                .justify_center()
                .text_color(c(p::MUTED))
                .cursor_pointer()
                .hover(|s| s.bg(c(p::HOVER)))
                .child(glyph)
        };
        let agent = !matches!(node.lane.as_str(), "" | "shell" | "verifier" | "rollback" | "publisher");
        let tabs = h_flex().id("node-tabs").overflow_x_scroll().gap(rems_from_px(1_f32)).border_b_1().border_color(c(p::BORDER)).children(TABS.iter().map(|(key, label)| {
            let on = self.tab == *key;
            let label = match *key {
                "stream" if node.state == "running" && live => if agent { "Live ●" } else { "Command ●" }.to_string(),
                "stream" if node.state == "running" => if agent { "Live" } else { "Command" }.to_string(),
                "stream" if !agent => "Command".to_string(),
                _ => label.to_string(),
            };
            let key = key.to_string();
            div()
                .id(SharedString::from(format!("node-tab-{key}")))
                .flex_none()
                .px_2()
                .py_1p5()
                .text_size(rems_from_px(12.5_f32))
                .text_color(c(if on { p::TEXT } else { p::SUB }))
                .border_b_2()
                .border_color(if on { c(p::BLUE) } else { p::ca(0x00000000) })
                .cursor_pointer()
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_tab(&key, cx)))
        }));
        // Built before the header because the header carries them now: the
        // node's actions used to sit in a footer bar of their own, which put the
        // panel's widest row at the bottom edge and left the copy button with
        // nothing beside it to say what it copied. The `run/node` id it copies
        // is drawn in the header row, next to the button.
        let copy_id = format!("{}/{}", self.model.run_id, node.id);
        let acts: Vec<AnyElement> = arr(detail.as_ref().unwrap_or(&Value::Null), "acts")
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let kind = str_of(b, "kind");
                let handler = b.get("do").filter(|d| !d.is_null()).map(|d| self.act(d.clone()));
                p::button(SharedString::from(format!("node-act-{i}")), str_of(b, "label"), if kind.is_empty() { "default" } else { &kind }, 26., 12.5, handler)
            })
            .collect();
        let header = v_flex()
            .flex_none()
            .px_3p5()
            .pt_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(p::text(m, 13., mc))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(rems_from_px(14_f32))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(c(p::TEXT))
                            .child(node.label.clone()),
                    )
                    .child(p::text(state_text(&node.state).to_string(), 12., mc))
                    // The run and node this panel is about, spelled out: beside
                    // it the copy button copies something the panel names. In a
                    // node tab the run id appears nowhere else — `render_header`
                    // is not rendered there — so this is the only place to read
                    // it.
                    .child(
                        p::text(copy_id.clone(), 11.5, p::MUTED)
                            .font(p::mono_font(cx))
                            .min_w_0()
                            .truncate()
                            .max_w(rems_from_px(220_f32)),
                    )
                    .child(ui::CopyButton::new("node-copy", copy_id.clone()).tooltip_label("Copy run and node id"))
                    .child(div().flex_1())
                    .child(icon_button("node-prev", "↑").on_click(cx.listener(|this, _, _, cx| this.step(-1, cx))))
                    .child(icon_button("node-next", "↓").on_click(cx.listener(|this, _, _, cx| this.step(1, cx))))
                    .when(!self.tab_mode, |this| {
                        this.child(icon_button("node-close", "✕").on_click(cx.listener(|this, _, _, cx| {
                            this.selected = None;
                            cx.notify();
                        })))
                    }),
            )
            .when(!acts.is_empty(), |this| {
                // The node's own actions, promoted from the footer to the header.
                this.child(h_flex().flex_wrap().gap_1p5().children(acts))
            })
            .child(
                h_flex()
                    .gap_1p5()
                    .child(p::lane_mark(&node.lane, 13.))
                    .child(p::text(
                        format!("{} · stage {} of {} · {}", if node.role.is_empty() { "node" } else { &node.role }, node.col + 1, stage_count, head),
                        12.,
                        p::SUB,
                    )),
            )
            .child(tabs);

        let body = match (&detail, &self.detail_error) {
            (_, Some(error)) if error.contains("invalid choice") => div()
                .p_3p5()
                .child(p::text(
                    format!("This mini-ork can't show “{}” for a node yet — it arrives with the next mini-ork update.", self.tab),
                    12.5,
                    p::SUB,
                ))
                .into_any_element(),
            (_, Some(error)) => div()
                .p_3p5()
                .child(self.texts.text("node-error", error, Look::new(12.5, c(p::RED)).mono(true), cx))
                .into_any_element(),
            (None, None) => div()
                .p_3p5()
                .child(crate::loading::loading_line(format!("board node {} {} --view {}", self.model.run_id, node.id, self.tab), cx))
                .into_any_element(),
            (Some(detail), None) if is_stream => self.render_stream(node, detail, live, window, cx),
            (Some(detail), None) if self.tab == "changes" => self.render_changes(node, detail, cx),
            (Some(detail), None) if self.tab == "overview" => self.render_overview(node, detail, cx),
            (Some(detail), None) if self.tab == "artifacts" => self.render_artifacts(detail, cx),
            (Some(detail), None) => self.render_facts(detail, cx),
        };

        v_flex()
            // In a node tab the inspector takes the space under the DAG strip;
            // in the full-screen graph it is a fixed-width side panel — unless
            // the stream is full screen, when it takes the overlay instead.
            .when(self.tab_mode, |this| this.w_full().flex_1().min_h_0())
            .when(!self.tab_mode && !self.stream_full, |this| this.flex_none().w(rems_from_px(600_f32)).h_full().border_l_1())
            .when(!self.tab_mode && self.stream_full, |this| this.flex_1().min_w_0().h_full())
            .border_color(c(p::BORDER))
            .bg(c(p::CHROME))
            .child(header)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .into_any_element()
    }

    /// Open a file in the workspace (a click handler).
    fn opener(&self, path: String) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        self.act(serde_json::json!({ "path": path }))
    }

    /// Open a file's diff in the workspace (a click handler): what the change
    /// did, rather than the file as it stands now. A file git has no status for
    /// — clean, ignored, outside the project — is opened as a file instead.
    fn differ(&self, path: String) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        self.act(serde_json::json!({ "diff": path }))
    }

    /// Changed-file rows. The row is the change link: clicking it opens that
    /// file's diff in the workspace, and "Open" opens the file itself. No diff
    /// is drawn here — the tab would otherwise repeat, in miniature, what the
    /// editor's diff view shows in full.
    fn file_rows(&self, key: &str, files: &[Value], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mono = p::mono_font(cx);
        let mut rows: Vec<AnyElement> = Vec::new();
        for (i, f) in files.iter().enumerate() {
            let path = str_of(f, "path");
            let abs = str_of(f, "abs");
            let mut row = h_flex()
                .id(SharedString::from(format!("{key}-file-{i}")))
                .gap_2p5()
                .px_1p5()
                .py(rems_from_px(3_f32))
                .hover(|s| s.bg(c(p::HOVER)))
                .child(div().flex_1().min_w_0().text_size(rems_from_px(12_f32)).font(mono.clone()).text_color(c(p::TEXT)).truncate().child(path.clone()))
                .child(
                    DiffStat::new(
                        SharedString::from(format!("{key}-file-diff-{i}")),
                        str_of(f, "added").parse::<usize>().unwrap_or(0),
                        str_of(f, "removed").parse::<usize>().unwrap_or(0),
                    )
                    .label_size(LabelSize::XSmall),
                );
            if !abs.is_empty() {
                let opener = self.opener(abs.clone());
                row = row
                    .cursor_pointer()
                    .child(p::button(
                        SharedString::from(format!("{key}-open-{i}")),
                        "Open",
                        "ghost",
                        20.,
                        11.,
                        Some(Box::new(move |event, window, cx| {
                            // The row opens the diff; the button must not too.
                            cx.stop_propagation();
                            opener(event, window, cx);
                        })),
                    ))
                    .on_click(self.differ(abs));
            }
            rows.push(row.into_any_element());
        }
        rows
    }

    /// What the node did, at a glance: one headline, its facts, its result
    /// (verdict and findings, checks, report), the files it touched, its final
    /// message, and links to its own artefacts.
    fn render_overview(&self, node: &Node, detail: &Value, cx: &mut Context<Self>) -> AnyElement {
        let heading = |text: &str| p::text(text.to_uppercase(), 11., p::SUB);
        let headline = detail.get("headline").cloned().unwrap_or(Value::Null);
        let facts = arr(detail, "facts");
        let result = detail.get("result").cloned().unwrap_or(Value::Null);
        let items = arr(&result, "items");
        let files = arr(detail, "files");
        let final_text = detail.get("final").map(|f| str_of(f, "text")).unwrap_or_default();
        let links = arr(detail, "links");

        let headline_el = (!str_of(&headline, "t").is_empty()).then(|| {
            div().child(self.texts.text(
                "ov-headline",
                str_of(&headline, "t"),
                Look::new(14., p::named(&str_of(&headline, "c"))).bold(true),
                cx,
            ))
        });
        let facts_el = (!facts.is_empty()).then(|| {
            div()
                .grid()
                .grid_cols(2)
                .gap_x_3p5()
                .gap_y_1p5()
                .children(facts.iter().enumerate().map(|(i, f)| {
                    v_flex()
                        .min_w_0()
                        .child(p::text(str_of(f, "k"), 10.5, p::SUB))
                        .child(self.texts.text(
                            format!("ov-fact-{i}"),
                            str_of(f, "v"),
                            Look::new(12.5, p::named(&str_of(f, "c"))).mono(true),
                            cx,
                        ))
                }))
        });
        let result_el = (!items.is_empty()).then(|| {
            let title = str_of(&result, "title");
            v_flex()
                .gap_0p5()
                .child(heading(if title.is_empty() { "Result" } else { &title }))
                .children(items.iter().enumerate().map(|(i, it)| {
                    let sub = str_of(it, "sub");
                    let path = str_of(it, "path");
                    let row = h_flex()
                        .id(SharedString::from(format!("ov-res-{i}")))
                        .items_start()
                        .gap_2()
                        .py_1()
                        .border_b_1()
                        .border_color(c(0x353a44))
                        .child(p::text(str_of(it, "m"), 12.5, p::SUB).text_color(p::named(&str_of(it, "mc"))).w_3().flex_none())
                        .child(
                            v_flex()
                                .min_w_0()
                                .child(self.texts.text(format!("ov-res-t{i}"), str_of(it, "t"), Look::new(12.5, p::named(&str_of(it, "tc"))), cx))
                                .when(!sub.is_empty(), |this| {
                                    this.child(self.texts.text(format!("ov-res-s{i}"), &sub, Look::new(11.5, c(p::SUB)).mono(true), cx))
                                }),
                        );
                    if path.is_empty() { row.into_any_element() } else { row.cursor_pointer().on_click(self.opener(path)).into_any_element() }
                }))
        });
        let file_rows = self.file_rows("ov", &files, cx);
        let files_el = (!files.is_empty()).then(|| v_flex().gap_0p5().child(heading("Files")).children(file_rows));
        let final_el = (!final_text.is_empty()).then(|| {
            v_flex().gap_1().child(heading("Final message")).child(
                self.texts.nested_scroll(
                    "ov-final",
                    div()
                        .id("ov-final")
                        .max_h(rems_from_px(360_f32))
                        .overflow_y_scroll()
                        .px_2p5()
                        .py_2()
                        .bg(c(p::BG))
                        .border_1()
                        .border_color(c(p::BORDER))
                        .child(self.texts.markdown("ov-final-md", &final_text, Look::new(12.5, c(p::BODY)), cx)),
                ),
            )
        });
        let links_el = (!links.is_empty()).then(|| {
            h_flex().flex_wrap().gap_1p5().children(links.iter().enumerate().map(|(i, l)| {
                p::button(
                    SharedString::from(format!("ov-link-{i}")),
                    str_of(l, "label"),
                    "ghost",
                    22.,
                    11.5,
                    Some(Box::new(self.opener(str_of(l, "path")))),
                )
            }))
        });
        let empty = headline_el.is_none() && facts_el.is_none() && items.is_empty() && final_text.is_empty();
        div()
            .id("node-overview")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3p5()
            .py_3()
            .child(
                v_flex()
                    .gap_3p5()
                    .children(headline_el)
                    .children(facts_el)
                    .children(result_el)
                    .children(files_el)
                    .children(final_el)
                    .children(links_el)
                    .when(empty, |this| {
                        this.child(p::text(format!("Nothing recorded for {} yet.", node.label), 12.5, p::SUB))
                    }),
            )
            .into_any_element()
    }

    /// What the node was given and what it produced; each artefact opens, and
    /// unfolds into a preview.
    fn render_artifacts(&self, detail: &Value, cx: &mut Context<Self>) -> AnyElement {
        let mono = p::mono_font(cx);
        let section = |title: &str, list: Vec<Value>, empty: &str, key: &str, cx: &mut Context<Self>| {
            let mut col = v_flex().gap_0p5().child(p::text(title.to_uppercase(), 11., p::SUB));
            if list.is_empty() {
                col = col.child(p::text(empty.to_string(), 12., p::SUB));
            }
            for (i, a) in list.iter().enumerate() {
                let path = str_of(a, "path");
                let open = self.art_open.contains(&path);
                let toggle_path = path.clone();
                let size = a.get("size").and_then(Value::as_u64).unwrap_or(0);
                let from = str_of(a, "from");
                col = col.child(
                    h_flex()
                        .id(SharedString::from(format!("art-{key}-{i}")))
                        .gap_2()
                        .px_1p5()
                        .py_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(c(p::HOVER)))
                        .child(p::text(if open { "▾" } else { "▸" }, 11., p::SUB).w_2p5().flex_none())
                        .child(div().flex_1().min_w_0().text_size(rems_from_px(12_f32)).font(mono.clone()).text_color(c(p::TEXT)).truncate().child(str_of(a, "name")))
                        .when(!from.is_empty(), |this| this.child(p::text(from.clone(), 11., p::SUB).flex_none()))
                        .child(p::text(human_size(size), 11., p::DIM).font(mono.clone()).flex_none())
                        .child(p::button(SharedString::from(format!("art-open-{key}-{i}")), "Open", "ghost", 20., 11., Some(Box::new(self.opener(path.clone())))))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.art_open.remove(&toggle_path) {
                                this.art_open.insert(toggle_path.clone());
                            }
                            cx.notify();
                        })),
                );
                if open {
                    let preview = str_of(a, "preview");
                    let kind = str_of(a, "kind");
                    let lines: Vec<Value> = if kind == "diff" {
                        diff_lines(&preview, 400)
                    } else {
                        preview.lines().map(|l| serde_json::json!({ "t": l, "c": "body" })).collect()
                    };
                    col = col.child(div().pl_4().child(crate::page::code_box(&self.texts, &format!("art-pv-{key}-{i}"), lines, cx)));
                }
            }
            col
        };
        let inputs = section("Given to this node", arr(detail, "inputs"), "No inputs recorded.", "in", cx);
        let outputs = section("Produced by this node", arr(detail, "outputs"), "No outputs recorded.", "out", cx);
        div()
            .id("node-artifacts")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3p5()
            .py_3()
            .child(v_flex().gap_4().child(inputs).child(outputs))
            .into_any_element()
    }

    /// What the node did to the code and what it decided: its result
    /// (verdict and findings, checks, plan, what a rollback reverted), the files
    /// changed after it, the diff, and the run's commits.
    fn render_changes(&self, node: &Node, detail: &Value, cx: &mut Context<Self>) -> AnyElement {
        let mono = p::mono_font(cx);
        let workspace = self.workspace.clone();
        // A file named anywhere under Changes — an edit, a result — is a change
        // link: it opens the file's diff, not the file as it stands now.
        let open_path = move |path: String| {
            let workspace = workspace.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let (workspace, path) = (workspace.clone(), path.clone());
                window.defer(cx, move |window, cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        store::perform(&serde_json::json!({ "diff": path }), &workspace, Origin::Page(WeakEntity::new_invalid()), window, cx);
                    }
                });
            }
        };
        let heading = |text: String| p::text(text.to_uppercase(), 11., p::SUB);
        let result = detail.get("result").cloned().unwrap_or(Value::Null);
        let result_items = arr(&result, "items");
        let files = arr(detail, "files");
        let commits = arr(detail, "commits");
        let diff = str_of(detail, "diff");
        let diff_note = str_of(detail, "diff_note");
        let commits_note = str_of(detail, "commits_note");

        let agent_edits = arr(detail, "agent_edits");
        let agent_note = str_of(detail, "agent_edits_note");
        let edits_section = (!agent_edits.is_empty() || !agent_note.is_empty()).then(|| {
            let mut col = v_flex().gap_1p5().child(heading(format!("Edited by {}", node.label)));
            if agent_edits.is_empty() {
                col = col.child(p::text(agent_note.clone(), 12., p::SUB));
            }
            for (i, e) in agent_edits.iter().enumerate() {
                let path = str_of(e, "path");
                // The row is the change link; the diff itself opens in the
                // editor, where it is whole.
                col = col.child(
                    h_flex()
                        .id(SharedString::from(format!("edit-{i}")))
                        .gap_2()
                        .cursor_pointer()
                        .on_click(open_path(path.clone()))
                        .child(div().flex_1().min_w_0().text_size(rems_from_px(12_f32)).font(mono.clone()).text_color(c(p::TEXT)).truncate().child(path.clone()))
                        .child(p::text(str_of(e, "tool"), 11., p::SUB))
                        .child(
                            DiffStat::new(
                                SharedString::from(format!("edit-diff-stat-{i}")),
                                str_of(e, "added").parse::<usize>().unwrap_or(0),
                                str_of(e, "removed").parse::<usize>().unwrap_or(0),
                            )
                            .label_size(LabelSize::XSmall),
                        ),
                );
            }
            col
        });
        let result_section = (!result_items.is_empty()).then(|| {
            v_flex()
                .gap_1()
                .child(heading(if str_of(&result, "title").is_empty() { "Result".into() } else { str_of(&result, "title") }))
                .children(result_items.iter().enumerate().map(|(i, it)| {
                    let sub = str_of(it, "sub");
                    let path = str_of(it, "path");
                    let row = h_flex()
                        .id(SharedString::from(format!("result-{i}")))
                        .items_start()
                        .gap_2()
                        .py_1()
                        .border_b_1()
                        .border_color(c(0x353a44))
                        .child(p::text(str_of(it, "m"), 12.5, p::SUB).text_color(p::named(&str_of(it, "mc"))).w_3().flex_none())
                        .child(
                            v_flex()
                                .min_w_0()
                                .child(self.texts.text(format!("res-{i}"), str_of(it, "t"), Look::new(12.5, p::named(&str_of(it, "tc"))), cx))
                                .when(!sub.is_empty(), |this| {
                                    this.child(self.texts.text(format!("res-s{i}"), &sub, Look::new(11.5, c(p::SUB)).mono(true), cx))
                                }),
                        );
                    if path.is_empty() {
                        row.into_any_element()
                    } else {
                        row.cursor_pointer().on_click(open_path(path)).into_any_element()
                    }
                }))
        });

        let changed_rows = self.file_rows("changed", &files, cx);
        let files_section = v_flex()
            .gap_0p5()
            .child(heading(format!("Files changed after {}", node.label)))
            .when(files.is_empty(), |this| {
                this.child(p::text(if diff_note.is_empty() { "No code changed by this point.".to_string() } else { diff_note.clone() }, 12., p::SUB))
            })
            .children(changed_rows);

        let diff_section = (!diff.is_empty()).then(|| {
            let lines = diff_lines(&diff, 800);
            let patch = diff.clone();
            let name = format!("mini-ork-{}-{}.patch", self.model.run_id, node.id);
            let workspace = self.workspace.clone();
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .child(heading("Diff".into()))
                        .child(div().flex_1())
                        .child(p::button(
                            "open-patch",
                            "Open as patch",
                            "ghost",
                            22.,
                            11.5,
                            Some(Box::new(move |_, window, cx| {
                                let path = std::env::temp_dir().join(&name);
                                if std::fs::write(&path, &patch).is_ok() {
                                    let (workspace, path) = (workspace.clone(), path.to_string_lossy().to_string());
                                    window.defer(cx, move |window, cx| {
                                        if let Some(workspace) = workspace.upgrade() {
                                            store::perform(&serde_json::json!({ "path": path }), &workspace, Origin::Page(WeakEntity::new_invalid()), window, cx);
                                        }
                                    });
                                }
                            })),
                        )),
                )
                .child(crate::page::code_box(&self.texts, "node-diff", lines, cx))
        });

        let commits_section = v_flex()
            .gap_0p5()
            .child(heading("Commits".into()))
            .when(commits.is_empty(), |this| {
                this.child(p::text(if commits_note.is_empty() { "No commits from this run.".to_string() } else { commits_note.clone() }, 12., p::SUB))
            })
            .children(commits.iter().enumerate().map(|(i, cm)| {
                let sha = str_of(cm, "sha");
                let short: String = sha.chars().take(8).collect();
                h_flex()
                    .gap_2p5()
                    .px_1p5()
                    .py_1()
                    .child(p::text(short, 12., p::YELLOW).font(mono.clone()).flex_none())
                    .child(div().flex_1().min_w_0().child(self.texts.text(format!("commit-{i}"), str_of(cm, "subject"), Look::new(12.5, c(p::TEXT)), cx)))
                    .child(p::text(str_of(cm, "when"), 11.5, p::SUB).flex_none())
            }));

        div()
            .id("node-changes")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3p5()
            .py_3()
            .child(
                v_flex()
                    .gap_3p5()
                    .children(edits_section)
                    .children(result_section)
                    .child(files_section)
                    .children(diff_section)
                    .child(commits_section),
            )
            .into_any_element()
    }

    fn render_facts(&self, detail: &Value, cx: &mut Context<Self>) -> AnyElement {
        let kv = arr(detail, "kv");
        let block = arr(detail, "block");
        let list = arr(detail, "list");
        div()
            .id("node-facts")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3p5()
            .py_3()
            .child(
                v_flex()
                    .gap_3()
                    .child(h_flex().flex_wrap().gap_2().children(kv.iter().map(|k| {
                        v_flex()
                            .w(rems_from_px(186_f32))
                            .px(rems_from_px(9_f32))
                            .py_1p5()
                            .bg(c(p::BG))
                            .border_1()
                            .border_color(c(p::BORDER))
                            .child(p::text(str_of(k, "k"), 10.5, p::SUB))
                            .child(self.texts.text(
                                format!("fact-{}", str_of(k, "k")),
                                str_of(k, "v"),
                                Look::new(12.5, p::named(&str_of(k, "c"))).mono(true),
                                cx,
                            ))
                    })))
                    .when_some(detail.get("markdown").filter(|m| !str_of(m, "text").is_empty()).cloned(), |this, doc| {
                        let path = str_of(&doc, "path");
                        let workspace = self.workspace.clone();
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .child(p::text(str_of(&doc, "title").to_uppercase(), 11., p::SUB))
                                        .child(div().flex_1())
                                        .when(!path.is_empty(), |this| {
                                            this.child(p::button(
                                                "fact-md-open",
                                                "Open",
                                                "ghost",
                                                22.,
                                                11.5,
                                                Some(Box::new(move |_, window, cx| {
                                                    let (workspace, path) = (workspace.clone(), path.clone());
                                                    window.defer(cx, move |window, cx| {
                                                        if let Some(workspace) = workspace.upgrade() {
                                                            store::perform(&serde_json::json!({ "path": path }), &workspace, Origin::Page(WeakEntity::new_invalid()), window, cx);
                                                        }
                                                    });
                                                })),
                                            ))
                                        }),
                                )
                                .child(
                                    self.texts.nested_scroll(
                                        "fact-md",
                                        div()
                                            .id("fact-md")
                                            .max_h(rems_from_px(480_f32))
                                            .overflow_y_scroll()
                                            .px_2p5()
                                            .py_2()
                                            .bg(c(p::BG))
                                            .border_1()
                                            .border_color(c(p::BORDER))
                                            .child(self.texts.markdown("fact-md-doc", str_of(&doc, "text"), Look::new(12.5, c(p::BODY)), cx)),
                                    ),
                                ),
                        )
                    })
                    .when(!block.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(p::text(str_of(detail, "block_title").to_uppercase(), 11., p::SUB))
                                .child(crate::page::code_box(&self.texts, "fact-block", block.clone(), cx)),
                        )
                    })
                    .when(!list.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(p::text(str_of(detail, "list_title").to_uppercase(), 11., p::SUB))
                                .children(list.iter().enumerate().map(|(i, it)| {
                                    let sub = str_of(it, "sub");
                                    h_flex()
                                        .items_start()
                                        .gap_2()
                                        .py_1()
                                        .border_b_1()
                                        .border_color(c(0x353a44))
                                        .child(p::text(str_of(it, "m"), 12.5, p::SUB).w_3().flex_none())
                                        .child(
                                            v_flex()
                                                .min_w_0()
                                                .child(self.texts.text(format!("fact-l{i}"), str_of(it, "t"), Look::new(12.5, c(p::TEXT)), cx))
                                                .when(!sub.is_empty(), |this| {
                                                    this.child(self.texts.text(format!("fact-ls{i}"), &sub, Look::new(11.5, c(p::SUB)), cx))
                                                }),
                                        )
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The active tab *is* the stream, so the DAG strip above it goes on its
    /// own: the transcript is what the reader opened the tab for, and half a
    /// tab spent on a DAG they already have is the box this view exists to
    /// avoid. The ⤢ toggle has nothing left to toggle while this holds.
    fn stream_takes_the_tab(&self) -> bool {
        self.tab_mode && self.tab == "stream"
    }

    /// The node tab on screen (`overview`, `stream`, …). Read by the run page,
    /// so a run-level row can ride with the node tab it belongs to.
    pub(crate) fn active_tab(&self) -> &str {
        &self.tab
    }

    /// The agent's session, rendered the way Claude Code's terminal shows it.
    fn render_stream(&self, node: &Node, detail: &Value, live: bool, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let mono = p::mono_font(cx);
        let status = str_of(detail, "status");
        let status_colour = match str_of(detail, "status_c").as_str() {
            "green" => p::GREEN,
            "yellow" => p::YELLOW,
            _ => p::SUB,
        };
        let visible = self.visible();
        let position = self
            .stream_cursor
            .and_then(|c| visible.iter().position(|&i| i == c))
            .map(|p| p + 1);
        let transcript_text = self.stream.iter().map(entry_text).collect::<Vec<_>>().join("\n");
        let running = live && node.state == "running";
        let source = str_of(detail, "source");
        let count = |kind: &str| -> usize {
            self.stream
                .iter()
                .filter(|e| match kind {
                    "all" => true,
                    "text" => matches!(str_of(e, "k").as_str(), "text" | "user" | "note"),
                    k => str_of(e, "k") == k,
                })
                .count()
        };
        let filters = [("all", "All"), ("tool", "Tools"), ("think", "Thinking"), ("text", "Text"), ("steer", "Steering")]
            .into_iter()
            .filter(|(key, _)| *key == "all" || count(key) > 0)
            .map(|(key, label)| {
                let on = self.stream_filter == key;
                div()
                    .id(SharedString::from(format!("stream-filter-{key}")))
                    .h_5()
                    .px(rems_from_px(7_f32))
                    .flex()
                    .items_center()
                    .border_1()
                    .border_color(c(if on { p::DIM } else { p::BORDER2 }))
                    .when(on, |this| this.bg(c(p::SELECTED)))
                    .text_size(rems_from_px(11_f32))
                    .text_color(c(if on { p::TEXT } else { p::MUTED }))
                    .cursor_pointer()
                    .child(format!("{label} {}", count(key)))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_filter(key, cx)))
            });
        let nav = |id: &'static str, glyph: &'static str, tip: &'static str, to: NavTo| {
            div()
                .id(id)
                .size(rems_from_px(22_f32))
                .flex()
                .items_center()
                .justify_center()
                .text_color(c(p::MUTED))
                .cursor_pointer()
                .hover(|s| s.bg(c(p::HOVER)).text_color(c(p::TEXT)))
                .tooltip(ui::Tooltip::text(tip))
                .child(glyph)
                .on_click(cx.listener(move |this, _, _, cx| this.navigate(to, cx)))
        };
        let header = v_flex()
            .flex_none()
            .border_b_1()
            .border_color(c(p::ROW_RULE))
            .child(
                h_flex()
                    .gap_2()
                    .px_3p5()
                    .pt(rems_from_px(7_f32))
                    .text_size(rems_from_px(11.5_f32))
                    .font(mono.clone())
                    .child(p::text("●", 11.5, status_colour))
                    .child(p::text(status, 11.5, status_colour))
                    .child(p::text(str_of(detail, "meta"), 11.5, p::DIM).truncate().min_w_0())
                    .child(div().flex_1())
                    .when(!self.stream.is_empty(), |this| {
                        this.child(
                            ui::CopyButton::new("stream-copy", transcript_text.clone())
                                .tooltip_label("Copy transcript"),
                        )
                    })
                    // No toggle on a node tab's stream: that tab is the
                    // full-screen case already, and a button that flips a flag
                    // nothing reads is worse than no button.
                    .children((!self.stream_takes_the_tab()).then(|| {
                        div()
                            .id("stream-full")
                            .flex_none()
                            .size(rems_from_px(20_f32))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(rems_from_px(4_f32))
                            .text_color(c(if self.stream_full { p::BLUE } else { p::MUTED }))
                            .cursor_pointer()
                            .hover(|s| s.bg(c(p::HOVER)).text_color(c(p::TEXT)))
                            .tooltip(ui::Tooltip::text(if self.stream_full {
                                "Exit full screen"
                            } else {
                                "Full screen"
                            }))
                            .child(
                                Icon::new(if self.stream_full { IconName::Minimize } else { IconName::MaximizeAlt })
                                    .size(IconSize::XSmall)
                                    .color(c(if self.stream_full { p::BLUE } else { p::MUTED }).into()),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.stream_full = !this.stream_full;
                                cx.notify();
                            }))
                    })),
            )
            .child(
                h_flex()
                    .gap(rems_from_px(5_f32))
                    .px_3p5()
                    .py_1p5()
                    .children(filters)
                    .child(div().flex_1())
                    .child(nav("stream-first", "⤒", "First entry", NavTo::First))
                    .child(nav("stream-prev", "↑", "Previous entry", NavTo::Prev))
                    .child(
                        div()
                            .min_w(rems_from_px(52_f32))
                            .text_center()
                            .text_size(rems_from_px(11_f32))
                            .font(mono.clone())
                            .text_color(c(p::SUB))
                            .child(match position {
                                Some(p) => format!("{p} / {}", visible.len()),
                                None => format!("– / {}", visible.len()),
                            }),
                    )
                    .child(nav("stream-next", "↓", "Next entry", NavTo::Next))
                    .child(nav("stream-last", "⤓", "Last entry", NavTo::Last)),
            );
        // The window: render only the last `stream_shown` visible entries, so a
        // hours-long transcript lays out a bounded list, not the whole session.
        let shown = visible.len().min(self.stream_shown);
        let render_from = visible.len() - shown;
        let entries: Vec<AnyElement> = visible[render_from..]
            .iter()
            .map(|&i| {
                let on = self.stream_cursor == Some(i);
                div()
                    .id(SharedString::from(format!("stream-entry-{i}")))
                    .pl_2()
                    .py_0p5()
                    .border_l_2()
                    .border_color(if on { c(p::BLUE) } else { p::ca(0x00000000) })
                    .when(on, |this| this.bg(p::ca(0x74ade80f)))
                    .child(self.stream_entry(i, &self.stream[i], cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.stream_cursor = Some(i);
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        let transcript = div()
            .id("node-stream")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.stream_scroll)
            .flex()
            .flex_col()
            .gap(rems_from_px(9_f32))
            .px_3()
            .pt_3()
            .pb(rems_from_px(18_f32))
            .font(mono.clone())
            .text_size(rems_from_px(12.5_f32))
            .line_height(rems_from_px(20_f32))
            .text_color(c(p::TEXT))
            .when(render_from > 0, |this| {
                let hidden = render_from;
                this.child(
                    div()
                        .id("stream-earlier")
                        .flex_none()
                        .py_1p5()
                        .text_center()
                        .text_size(rems_from_px(11.5_f32))
                        .text_color(c(p::BLUE))
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child(format!("▲ {hidden} earlier entries — show more"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.stream_shown = this.stream_shown.saturating_add(hidden);
                            cx.notify();
                        })),
                )
            })
            .children(entries)
            .when(visible.is_empty(), |this| {
                this.child(
                    div().py(rems_from_px(30_f32)).w_full().text_center().child(p::text(
                        if node.state == "pending" {
                            "Not dispatched yet."
                        } else if self.stream.is_empty() {
                            "No stream yet."
                        } else {
                            "Nothing of this kind."
                        },
                        12.5,
                        p::DIM,
                    )),
                )
            })
            .when(running, |this| {
                this.child(
                    h_flex()
                        .gap(rems_from_px(9_f32))
                        .pl_2p5()
                        .text_color(c(p::ORANGE))
                        .child(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::Small)
                                .color(Color::Custom(c(p::ORANGE)))
                                .with_throttled_rotate("graph-loading", 1),
                        )
                        .child("Working…")
                        .child(p::text(format!("· streaming from {source}"), 12.5, p::DIM)),
                )
            });
        let footer = if running {
            let sevs = ["info", "warn", "critical"].map(|sev| {
                let on = self.severity == sev;
                let colour = match (on, sev) {
                    (false, _) => p::SUB,
                    (true, "critical") => p::RED,
                    (true, "warn") => p::YELLOW,
                    _ => p::TEXT,
                };
                div()
                    .id(SharedString::from(format!("sev-{sev}")))
                    .h(rems_from_px(22_f32))
                    .px(rems_from_px(7_f32))
                    .flex()
                    .items_center()
                    .border_1()
                    .border_color(c(if on { p::DIM } else { p::BORDER2 }))
                    .when(on, |this| this.bg(c(p::SELECTED)))
                    .text_size(rems_from_px(11_f32))
                    .text_color(c(colour))
                    .cursor_pointer()
                    .child(sev)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.severity = sev;
                        cx.notify();
                    }))
            });
            v_flex()
                .flex_none()
                .px_3p5()
                .pt_2p5()
                .pb_3()
                .gap_1p5()
                .child(
                    h_flex()
                        .gap_2()
                        .px_2p5()
                        .py_1p5()
                        .border_1()
                        .border_color(c(p::DIM))
                        .bg(c(0x262a31))
                        .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "enter" && !event.keystroke.modifiers.shift {
                                this.send_steer(window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(p::text(">", 12.5, p::MUTED).font(mono.clone()))
                        .child(div().flex_1().min_w_0().text_size(rems_from_px(12.5_f32)).child(self.steer.render(window, cx)))
                        .children(sevs),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .text_size(rems_from_px(11_f32))
                        .font(mono.clone())
                        .text_color(c(p::DIM))
                        .child("⏵⏵ attached")
                        .child("lands as operator steering · consumed on its next turn")
                        .child("esc to detach"),
                )
                .into_any_element()
        } else {
            let note = str_of(detail, "done_note");
            div()
                .flex_none()
                .px_3p5()
                .py(rems_from_px(9_f32))
                .border_t_1()
                .border_color(c(p::ROW_RULE))
                .when(!note.is_empty(), |this| this.child(p::text(note, 12., p::SUB)))
                .into_any_element()
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .bg(c(0x1f2227))
            .border_t_1()
            .border_color(c(p::BORDER))
            .child(header)
            .child(transcript)
            .child(footer)
            .into_any_element()
    }
}

/// An entry as plain text, for "Copy".
fn entry_text(e: &Value) -> String {
    let mut text = format!("{}{}", str_of(e, "head"), str_of(e, "arg"));
    for l in arr(e, "lines") {
        text.push_str("\n  ");
        text.push_str(&str_of(&l, "t"));
    }
    text
}

impl MiniOrkGraph {
    /// One transcript entry: `user`/`steer` boxes, `note` asides, and rows —
    /// `⏺ Tool(args)` with `⎿` result lines, `✻ Thinking…`, text, todos.
    /// Every piece of text is selectable.
    fn stream_entry(&self, index: usize, e: &Value, cx: &mut Context<Self>) -> AnyElement {
        let kind = str_of(e, "k");
        let head = str_of(e, "head");
        let arg = str_of(e, "arg");
        let lines = arr(e, "lines");
        let key = |part: &str| format!("e{index}-{part}");
        match kind.as_str() {
            "user" | "steer" => {
                let steer = kind == "steer";
                div()
                    .px_2p5()
                    .py(rems_from_px(7_f32))
                    .border_1()
                    .border_color(if steer { p::ca(0xdec18480) } else { c(p::BORDER2) })
                    .bg(if steer { p::ca(0xdec18410) } else { c(0x262a31) })
                    .child(
                        h_flex()
                            .items_start()
                            .gap_1p5()
                            .child(p::text(">", 12.5, if steer { p::YELLOW } else { p::SUB }))
                            .child(
                                self.texts.nested_scroll(
                                    key("box-scroll"),
                                    div()
                                        .id(SharedString::from(key("box-scroll")))
                                        .flex_1()
                                        .min_w_0()
                                        .max_h(rems_from_px(360_f32))
                                        .overflow_y_scroll()
                                        .child(if steer {
                                            self.texts.text(key("box"), format!("{head}{arg}"), Look::new(12.5, c(p::YELLOW)).mono(true), cx)
                                        } else {
                                            // The prompt is a markdown document: render it whole.
                                            self.texts.markdown(key("box"), format!("{head}{arg}"), Look::new(12.5, c(p::TEXT)), cx)
                                        }),
                                ),
                            ),
                    )
                    .into_any_element()
            }
            "note" => div()
                .pl_5()
                .child(self.texts.text(key("note"), format!("⎿  {arg}"), Look::new(11.5, c(p::DIM)).mono(true), cx))
                .into_any_element(),
            _ => {
                let (dot, dot_colour, italic) = match kind.as_str() {
                    "think" => ("✻", p::PURPLE, true),
                    "tool" => {
                        let failed = lines.iter().any(|l| str_of(l, "c") == "red");
                        ("⏺", if failed { p::RED } else { p::GREEN }, false)
                    }
                    "out" => ("", p::DIM, false),
                    _ => ("⏺", p::TEXT, false),
                };
                let arg_text = if kind == "tool" && !arg.is_empty() { format!("({arg})") } else { arg };
                let expanded = self.expanded.contains(&index);
                let hidden = if !expanded && lines.len() > COLLAPSED_LINES + 1 { lines.len() - COLLAPSED_LINES } else { 0 };
                let mut line_els: Vec<AnyElement> = lines
                    .iter()
                    .take(if hidden > 0 { COLLAPSED_LINES } else { lines.len() })
                    .enumerate()
                    .map(|(i, l)| {
                        let bg = match str_of(l, "bg").as_str() {
                            "green-bg" => Some(p::ca(0xa1c1811f)),
                            "red-bg" => Some(p::ca(0xd072771f)),
                            _ => None,
                        };
                        let colour = str_of(l, "c");
                        div()
                            .px_1p5()
                            .when_some(bg, |this, bg| this.bg(bg))
                            .child(self.texts.text(
                                key(&format!("l{i}")),
                                str_of(l, "t"),
                                Look::new(12., if colour.is_empty() { c(p::MUTED) } else { p::named(&colour) }).mono(true).nowrap(),
                                cx,
                            ))
                            .into_any_element()
                    })
                    .collect();
                if hidden > 0 || (expanded && lines.len() > COLLAPSED_LINES + 1) {
                    let label = if hidden > 0 { format!("… +{hidden} lines") } else { "show less".to_string() };
                    line_els.push(
                        div()
                            .id(SharedString::from(format!("e{index}-more")))
                            .px_1p5()
                            .text_size(rems_from_px(11.5_f32))
                            .text_color(c(p::BLUE))
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.expanded.insert(index) {
                                    this.expanded.remove(&index);
                                }
                                cx.notify();
                            }))
                            .into_any_element(),
                    );
                }
                let head_el = (!head.is_empty())
                    .then(|| self.texts.text(key("head"), &head, Look::new(12.5, c(p::TEXT)).mono(true).bold(true).italic(italic), cx));
                let arg_el = (!arg_text.is_empty()).then(|| {
                    if kind == "text" {
                        // Agent text is markdown: whole, capped, scrollable.
                        self.texts
                            .nested_scroll(
                                key("text-scroll"),
                                div()
                                    .id(SharedString::from(key("text-scroll")))
                                    .w_full()
                                    .max_h(rems_from_px(360_f32))
                                    .overflow_y_scroll()
                                    .child(self.texts.markdown(key("arg"), &arg_text, Look::new(12.5, c(p::TEXT)), cx)),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .w_full()
                            .min_w_0()
                            .child(self.texts.text(key("arg"), &arg_text, Look::new(12.5, c(p::MUTED)).mono(true).italic(italic), cx))
                            .into_any_element()
                    }
                });
                v_flex()
                    .when(kind != "out", |this| {
                        this.child(
                            h_flex()
                                .items_start()
                                .gap(rems_from_px(9_f32))
                                .child(div().flex_none().w_3().text_size(rems_from_px(11_f32)).text_color(c(dot_colour)).child(dot))
                                .child(v_flex().flex_1().min_w_0().children(head_el).children(arg_el)),
                        )
                    })
                    .when(!line_els.is_empty(), |this| {
                        this.child(
                            h_flex()
                                .items_start()
                                .gap_2()
                                .mt(px(2.))
                                .pl(px(if kind == "out" { 22. } else { 0. }))
                                .child(div().flex_none().w_3().text_color(c(p::DIM)).child("⎿"))
                                .child(v_flex().flex_1().min_w_0().children(line_els)),
                        )
                    })
                    .into_any_element()
            }
        }
    }
}

impl Render for MiniOrkGraph {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.texts.begin();
        let viewport = window.viewport_size();
        let selected = self.selected.as_ref().and_then(|id| self.node(id)).cloned();
        if self.tab_mode {
            // A node tab leads with the run's DAG — the node highlighted, its
            // edges blue — then the node's own detail underneath. Reusing the
            // full-screen canvas keeps the two from drifting; only the height
            // is bounded so the detail below stays visible.
            if !self.tab_fit {
                // Fit the whole DAG across the strip, so the rightmost node
                // (rollback, publisher) isn't cut off behind the panel edge,
                // and never zoom in past 100%. Measured from the container the
                // canvas actually got — a node tab's strip spans the pane, and
                // the run page embeds this view inside its own padding, so the
                // window width would fit both to a box neither one has. Waits
                // for the first layout rather than guessing.
                let measured = f32::from(self.canvas_scroll.bounds().size.width);
                if measured > 0. {
                    self.zoom = fit_zoom(measured, &self.model, 0.);
                    self.tab_fit = true;
                }
            }
            // Full screen: the strip is dropped so the stream below gets the
            // whole tab — and it goes the same way when the active tab *is* the
            // stream, with no toggle to press. Built only when it will be shown:
            // the canvas lays out every node, and a hidden strip should not pay
            // for that.
            let dag = (!self.stream_full && !self.stream_takes_the_tab()).then(|| {
                div()
                    .id("node-tab-dag")
                    .flex_none()
                    .h(px(NODE_TAB_DAG_H))
                    .min_h_0()
                    .border_b_1()
                    .border_color(c(p::BORDER))
                    .child(if self.list_view { self.render_list(cx) } else { self.render_graph(cx) })
            });
            let inspector = selected.map(|node| self.render_inspector(&node, window, cx));
            self.texts.end();
            return v_flex()
                .id("mini-ork-node-tab")
                .track_focus(&self.focus_handle)
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" && this.stream_full {
                        this.stream_full = false;
                        cx.notify();
                    }
                }))
                .size_full()
                .bg(c(p::CHROME))
                .text_color(c(p::TEXT))
                .children(dag)
                .children(inspector)
                .into_any_element();
        }
        let header = self.render_header(window, cx);
        let canvas = (!self.stream_full)
            .then(|| if self.list_view { self.render_list(cx) } else { self.render_graph(cx) });
        let inspector = selected.map(|node| self.render_inspector(&node, window, cx));
        self.texts.end();
        v_flex()
            .id("mini-ork-run-graph")
            .track_focus(&self.focus_handle)
            .key_context("MiniOrkRunGraph")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    if this.stream_full {
                        this.stream_full = false;
                        cx.notify();
                    } else if this.selected.is_some() {
                        this.selected = None;
                        cx.notify();
                    } else {
                        this.close(cx);
                    }
                }
            }))
            .w(viewport.width)
            .h(viewport.height)
            .bg(c(p::BG))
            .text_color(c(p::TEXT))
            .occlude()
            .child(header)
            .child(h_flex().flex_1().min_h_0().items_stretch().children(canvas).children(inspector))
            .with_animation(
                "graph-fade-in",
                gpui::Animation::new(Duration::from_millis(180)),
                |el, delta| el.opacity(delta),
            )
            .into_any_element()
    }
}

impl MiniOrkGraph {
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus_handle, cx);
    }

    /// The node the inspector shows.
    pub fn selected_node(&self) -> Option<String> {
        self.selected.clone()
    }

    /// The overlay is already open: jump to another node's overview.
    pub fn open_node(&mut self, node: String, cx: &mut Context<Self>) {
        self.tab = "overview".into();
        self.select(node, cx);
    }
}

/// `812 B`, `14.2 KB`, `3.1 MB`.
fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.),
        b => format!("{:.1} MB", b as f64 / (1024. * 1024.)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The page the engine serves for a three-column run, with `edges` as the
    /// engine sent them.
    fn page(edges: Value) -> Value {
        json!({
            "graph": {
                "cols": [
                    [{"id": "planner", "label": "planner"}],
                    [{"id": "implementer", "label": "implementer"}],
                    [{"id": "test", "label": "test"}],
                ],
                "heads": ["planner", "implementer", "test"],
                "edges": edges,
            }
        })
    }

    fn model(edges: Value) -> Model {
        Model::from_page(&page(edges), "run-1").expect("a graph object")
    }

    fn named(model: &Model) -> Vec<(String, String, String)> {
        model
            .spans()
            .iter()
            .map(|s| (s.from.id.clone(), s.to.id.clone(), s.kind.clone()))
            .collect()
    }

    #[test]
    fn the_engines_edge_list_resolves_to_spans() {
        let model = model(json!([
            {"from": "planner", "to": "implementer", "edge_type": "depends_on"},
            {"from": "implementer", "to": "test", "edge_type": "verifies"},
        ]));
        assert_eq!(
            named(&model),
            vec![
                ("planner".into(), "implementer".into(), "depends_on".into()),
                ("implementer".into(), "test".into(), "verifies".into()),
            ]
        );
    }

    #[test]
    fn an_edge_without_a_kind_is_a_depends_on() {
        let model = model(json!([{"from": "planner", "to": "implementer"}]));
        assert_eq!(named(&model)[0].2, "depends_on");
    }

    /// `epic-runner`'s `reflector → epic_dispatcher` revise edge: the target
    /// sits in an earlier column, which is exactly the test the canvas uses to
    /// route an edge as a back arc instead of a forward step.
    #[test]
    fn a_retry_edge_lands_on_an_earlier_column() {
        let model = model(json!([{"from": "test", "to": "implementer", "edge_type": "retries"}]));
        let spans = model.spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, "retries");
        assert!(spans[0].to.col <= spans[0].from.col, "drawn as a back arc");
    }

    #[test]
    fn a_dangling_or_self_edge_is_not_drawn() {
        let model = model(json!([
            {"from": "planner", "to": "ghost", "edge_type": "depends_on"},
            {"from": "planner", "to": "planner", "edge_type": "retries"},
            {"from": "planner", "to": "implementer", "edge_type": "depends_on"},
        ]));
        assert_eq!(model.edges.len(), 2, "the self edge never leaves the parser");
        assert_eq!(named(&model), vec![("planner".into(), "implementer".into(), "depends_on".into())]);
    }

    /// An engine that predates the edge list sends `[]`: the drawing must fall
    /// back to the fan-out it used to invent, not come out empty.
    #[test]
    fn an_engine_without_edges_still_draws_the_fan_out() {
        let model = model(json!([]));
        assert!(model.edges.is_empty());
        assert_eq!(model.spans().len(), 2, "one node per column, so one edge per gap");
    }

    /// The back arc is drawn under the band, so the canvas has to be taller
    /// than the nodes are — otherwise its deepest point is clipped away and
    /// the loop disappears again.
    #[test]
    fn a_back_edge_reserves_room_under_the_band() {
        let forward = model(json!([{"from": "planner", "to": "implementer", "edge_type": "depends_on"}]));
        let loopy = model(json!([{"from": "test", "to": "implementer", "edge_type": "retries"}]));
        assert!(loopy.height() > forward.height(), "the arc needs room below the nodes");
    }

    /// Three nodes of one column into one node of the next: the fan-in the old
    /// drawing got wrong, because every edge left its source and arrived at its
    /// target at the same point, so the three drew as one thick line.
    #[test]
    fn a_fan_into_one_node_gives_every_edge_a_port_of_its_own() {
        let model = Model::from_page(&fan_page(), "run-1").expect("a graph object");
        let routes = model.routes();
        assert_eq!(routes.len(), 3);
        let arrival = |from: &str| {
            routes
                .iter()
                .find(|route| route.span.from.id == from)
                .expect("a route out of that node")
                .points
                .last()
                .copied()
                .expect("a route has two ends")
                .1
        };
        let (a, b, c) = (arrival("a"), arrival("b"), arrival("c"));
        assert!(a < b && b < c, "the fan keeps the order of the nodes it feeds from");
    }

    /// Two loops into different stages: each takes a lane of the rail, they do
    /// not share one, and the canvas is deep enough to hold the deeper lane.
    #[test]
    fn two_loops_take_a_lane_each_under_the_band() {
        let one = model(json!([{"from": "test", "to": "implementer", "edge_type": "retries"}]));
        let two = model(json!([
            {"from": "test", "to": "implementer", "edge_type": "retries"},
            {"from": "test", "to": "planner", "edge_type": "retries"},
        ]));
        let lanes: Vec<f32> = two
            .routes()
            .iter()
            .map(|route| route.points[1].1)
            .collect();
        assert_ne!(lanes[0], lanes[1], "two loops do not run in one lane");
        assert!(two.height() > one.height(), "the deeper lane needs room");
        assert!(lanes.iter().all(|y| *y > two.band() && *y < two.height()), "the lanes run under the band, inside the canvas");
    }

    /// An edge is pickable along its line, with a few pixels of slack, and a
    /// click inside a node belongs to the node — the canvas paints the node over
    /// the edge that runs under it, so it has to read the click the same way.
    #[test]
    fn an_edge_is_picked_along_its_line_and_not_through_a_node() {
        let model = model(json!([{"from": "planner", "to": "implementer", "edge_type": "depends_on"}]));
        let routes = model.routes();
        let route = routes.first().expect("a route");
        let (start_x, start_y) = route.points[0];
        let end_y = route.points.last().expect("an end").1;
        let middle = (start_x + 20., (start_y + end_y) / 2.);
        assert_eq!(
            route_at(&routes, &model, middle.0, middle.1, 2.),
            Some(("planner".into(), "implementer".into()))
        );
        assert_eq!(route_at(&routes, &model, middle.0, middle.1 + 40., 2.), None, "40px away is not the edge");
        assert_eq!(route_at(&routes, &model, start_x - 10., middle.1, 2.), None, "that point is inside the source node");
    }

    /// One column of three nodes feeding one node of the next.
    fn fan_page() -> Value {
        json!({
            "graph": {
                "cols": [
                    [{"id": "a", "label": "a"}, {"id": "b", "label": "b"}, {"id": "c", "label": "c"}],
                    [{"id": "synth", "label": "synth"}],
                ],
                "heads": ["judges", "synth"],
                "edges": [
                    {"from": "a", "to": "synth", "edge_type": "depends_on"},
                    {"from": "b", "to": "synth", "edge_type": "depends_on"},
                    {"from": "c", "to": "synth", "edge_type": "depends_on"},
                ],
            }
        })
    }
}
