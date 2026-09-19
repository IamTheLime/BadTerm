//! The window's root: the title strip (tabs plus window buttons), the active
//! terminal, the plugin sidebar, and the one place that executes `AppCommand`.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    AnyElement, Animation, AnimationExt, App, Bounds, Context, CursorStyle, ElementId, Entity, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, ResizeEdge, ScrollHandle,
    Size, Subscription, Window,
    div, ease_out_quint, prelude::*, px,
};
use tw_control::{ControlEvent, ControlServer};
use tw_scripting::{HostEvent, HostMessage, NodeHost, PluginState, PluginView, TabInfo};

use crate::actions::{
    self, CloseDocument, CloseTab, FocusDown, FocusLeft, FocusRight, FocusUp, MoveTabLeft, MoveTabRight,
    NewTab, NextTab, PrevTab, ReloadPlugins, ResizeDown, ResizeLeft, ResizeRight, ResizeUp, SplitDown, SplitRight,
};
use crate::command::{AppCommand, Direction, PaneId, TabId};
use crate::markdown::{self, Document};
use crate::nvim_view::NvimPanel;
use crate::terminal_view::{TerminalView, TerminalViewEvent};
use crate::theme;
use crate::titlebar::WindowDrag;
use crate::widgets::{self, Dispatch};


const LOG_LINES: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SplitId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SplitDirection {
    Right,
    Down,

}
enum PaneNode {
    Leaf(PaneId),
    Split {
        id: SplitId,
        direction: SplitDirection,
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

impl PaneNode {
    fn contains(&self, id: PaneId) -> bool {
        match self {
            Self::Leaf(leaf) => *leaf == id,
            Self::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    fn split(&mut self, target: PaneId, direction: SplitDirection, new: PaneId, split_id: SplitId) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    id: split_id,
                    direction,
                    ratio: 0.5,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, direction, new, split_id) || second.split(target, direction, new, split_id)
            }
        }
    }

    fn without(self, target: PaneId) -> (Option<Self>, bool) {
        match self {
            Self::Leaf(id) => (if id == target { None } else { Some(Self::Leaf(id)) }, id == target),
            Self::Split { id, direction, ratio, first, second } => {
                let (first, removed) = first.without(target);
                if removed {
                    return (
                        Some(match first {
                            Some(first) => Self::Split { id, direction, ratio, first: Box::new(first), second },
                            None => *second,
                        }),
                        true,
                    );
                }
                let (second, removed) = second.without(target);
                if removed {
                    return (
                        Some(match second {
                            Some(second) => Self::Split {
                                id,
                                direction,
                                ratio,
                                first: Box::new(first.expect("unremoved pane tree must remain")),
                                second: Box::new(second),
                            },
                            None => first.expect("unremoved pane tree must remain"),
                        }),
                        true,
                    );
                }
                (
                    Some(Self::Split {
                        id,
                        direction,
                        ratio,
                        first: Box::new(first.expect("unremoved pane tree must remain")),
                        second: Box::new(second.expect("unremoved pane tree must remain")),
                    }),
                    false,
                )
            }
        }
    }
}

impl PaneNode {
    fn first_leaf(&self) -> PaneId {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { first, .. } => first.first_leaf(),
        }
    }

    fn last_leaf(&self) -> PaneId {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { second, .. } => second.last_leaf(),
        }
    }

    fn focus_neighbor(&self, target: PaneId, direction: SplitDirection, toward_second: bool) -> Option<PaneId> {
        match self {
            Self::Leaf(_) => None,
            Self::Split { direction: axis, first, second, .. } => {
                if first.contains(target) {
                    if *axis == direction && toward_second {
                        first.focus_neighbor(target, direction, toward_second).or_else(|| Some(second.first_leaf()))
                    } else {
                        first.focus_neighbor(target, direction, toward_second)
                    }
                } else if second.contains(target) {
                    if *axis == direction && !toward_second {
                        second.focus_neighbor(target, direction, toward_second).or_else(|| Some(first.last_leaf()))
                    } else {
                        second.focus_neighbor(target, direction, toward_second)
                    }
                } else {
                    None
                }
            }
        }
    }

    fn resize(&mut self, target: PaneId, direction: SplitDirection, delta: f32) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split { direction: axis, ratio, first, second, .. } => {
                let in_first = first.contains(target);
                let in_second = second.contains(target);
                if !in_first && !in_second {
                    return false;
                }
                if *axis == direction && (first.resize(target, direction, delta) || second.resize(target, direction, delta)) {
                    return true;
                }
                if *axis == direction {
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    true
                } else if in_first {
                    first.resize(target, direction, delta)
                } else {
                    second.resize(target, direction, delta)
                }
            }
        }
    }
    fn set_ratio(&mut self, split_id: SplitId, ratio: f32) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split { id, ratio: current, first, second, .. } => {
                if *id == split_id {
                    *current = ratio.clamp(0.1, 0.9);
                    true
                } else {
                    first.set_ratio(split_id, ratio) || second.set_ratio(split_id, ratio)
                }
            }
        }
    }
}


struct Pane {
    view: Entity<TerminalView>,
    _events: Subscription,
}

struct Tab {
    id: TabId,
    root: PaneNode,
    panes: HashMap<PaneId, Pane>,
    active_pane: PaneId,
}
/// The Node process that runs the plugins is either up or explains why not.
enum Plugins {
    Running(NodeHost),
    Stopped { error: String },
}

/// The control socket is either listening or explains why not.
enum Control {
    Listening(ControlServer),
    Failed(String),
}

/// A tab being dragged along the strip.
#[derive(Clone)]
struct DraggedTab {
    index: usize,
    title: String,
}

/// Gap between tabs in the strip; the slide animation needs it to predict the new layout.
const TAB_GAP: f32 = 4.0;
const TAB_SLIDE: Duration = Duration::from_millis(220);

/// The slide of the current layout change: for each tab, where it was
/// relative to where it now is. Tabs start at that offset and ease to zero.
/// `generation` restarts the animation on every change.
#[derive(Default)]
struct TabMotion {
    generation: u64,
    offsets: HashMap<TabId, Pixels>,
}

/// The pill that follows the pointer while a tab is dragged.
struct TabDragPreview {
    title: String,
    position: Point<Pixels>,
}

struct SplitDrag {
    id: SplitId,
    direction: SplitDirection,
    start: Point<Pixels>,
    ratio: f32,
}

fn resize_edge(position: Point<Pixels>, size: Size<Pixels>) -> Option<ResizeEdge> {
    const EDGE: f32 = 8.0;
    let left = f32::from(position.x) <= EDGE;
    let right = f32::from(position.x) >= f32::from(size.width) - EDGE;
    let top = f32::from(position.y) <= EDGE;
    let bottom = f32::from(position.y) >= f32::from(size.height) - EDGE;
    match (top, left, right, bottom) {
        (true, true, _, _) => Some(ResizeEdge::TopLeft),
        (true, _, true, _) => Some(ResizeEdge::TopRight),
        (_, true, _, true) => Some(ResizeEdge::BottomLeft),
        (_, _, true, true) => Some(ResizeEdge::BottomRight),
        (true, _, _, _) => Some(ResizeEdge::Top),
        (_, _, _, true) => Some(ResizeEdge::Bottom),
        (_, true, _, _) => Some(ResizeEdge::Left),
        (_, _, true, _) => Some(ResizeEdge::Right),
        _ => None,
    }
}

impl Render for TabDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().pl(self.position.x - px(30.0)).pt(self.position.y - px(12.0)).child(
            div()
                .px_3()
                .py_1()
                .rounded_md()
                .bg(theme::raised())
                .text_sm()
                .text_color(theme::text())
                .shadow_md()
                .child(self.title.clone()),
        )
    }
}

pub struct Workspace {
    tabs: Vec<Tab>,
    active: usize,
    next_tab_id: u64,
    next_pane_id: u64,
    next_split_id: u64,
    plugin_dir: PathBuf,
    plugins: Plugins,
    plugin_names: Vec<String>,
    /// The last trees the host rendered; the sidebar draws these.
    plugin_views: Vec<PluginView>,
    control: Control,
    nvim_panel: NvimPanel,
    /// The markdown document shown at the top of the sidebar, if any.
    document: Option<Document>,
    log: VecDeque<String>,
    focus_handle: FocusHandle,
    /// Scroll position of the tab strip; follows the active tab.
    tab_scroll: ScrollHandle,
    /// Where each tab was painted last frame, measured after layout.
    tab_bounds: HashMap<TabId, Bounds<Pixels>>,
    tab_motion: TabMotion,
    split_bounds: HashMap<SplitId, Bounds<Pixels>>,
    split_drag: Option<SplitDrag>,
    /// Set while the title label is being dragged.
    window_drag: Option<WindowDrag>,
}

impl Workspace {
    pub fn new(plugin_dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut workspace = Self {
            tabs: Vec::new(),
            next_split_id: 1,
            active: 0,
            next_tab_id: 1,
            next_pane_id: 1,
            plugin_dir,
            plugins: Plugins::Stopped { error: "not started".to_owned() },
            plugin_names: Vec::new(),
            plugin_views: Vec::new(),
            control: Control::Failed("not started".to_owned()),
            document: None,
            nvim_panel: NvimPanel::default(),
            log: VecDeque::new(),
            focus_handle: cx.focus_handle(),
            tab_scroll: ScrollHandle::new(),
            tab_bounds: HashMap::new(),
            split_bounds: HashMap::new(),
            split_drag: None,
            tab_motion: TabMotion::default(),
            window_drag: None,
        };
        workspace.start_plugins(window, cx);
        workspace.start_control(window, cx);
        workspace.execute(AppCommand::NewTab, window, cx);
        workspace
    }

    /// The single dispatch point. Every producer ends up here.
    pub fn execute(&mut self, command: AppCommand, window: &mut Window, cx: &mut Context<Self>) {
        let tabs_change = matches!(
            &command,
            AppCommand::NewTab
                | AppCommand::CloseTab(_)
                | AppCommand::CloseActiveTab
                | AppCommand::SelectTab(_)
                | AppCommand::NextTab
                | AppCommand::PrevTab
                | AppCommand::MoveActiveTab(_)
                | AppCommand::MoveTab { .. }
        );
        match command {
            AppCommand::NewTab => self.open_tab(window, cx),
            AppCommand::CloseActiveTab => {
                if let Some(tab) = self.tabs.get(self.active) {
                    let id = tab.id;
                    self.close_tab(id, window, cx);
                }
            }
            AppCommand::CloseTab(id) => self.close_tab(id, window, cx),
            AppCommand::SelectTab(index) => self.select_tab(index, window, cx),
            AppCommand::NextTab => {
                if !self.tabs.is_empty() {
                    self.select_tab((self.active + 1) % self.tabs.len(), window, cx);
                }
            }
            AppCommand::PrevTab => {
                if !self.tabs.is_empty() {
                    self.select_tab((self.active + self.tabs.len() - 1) % self.tabs.len(), window, cx);
                }
            }
            AppCommand::WriteToTerminal { tab, text } => {
                let target = match tab {
                    Some(id) => self.tabs.iter().find(|t| t.id == id),
                    None => self.tabs.get(self.active),
                };
                match target {
                    Some(tab) => tab
                        .panes
                        .get(&tab.active_pane)
                        .expect("active pane must belong to tab")
                        .view
                        .update(cx, |view, cx| view.write(&text, cx)),
                    None => self.push_log("no tab to write to".to_owned()),
                }
            }
            AppCommand::MoveActiveTab(direction) => {
                let to = match direction {
                    Direction::Left => self.active.checked_sub(1),
                    Direction::Right => Some(self.active + 1).filter(|to| *to < self.tabs.len()),
                };
                if let Some(to) = to {
                    self.move_tab(self.active, to);
                }
            }
            AppCommand::MoveTab { from, to } => self.move_tab(from, to),
            AppCommand::ReloadPlugins => self.reload_plugins(window, cx),
            AppCommand::Log(message) => self.push_log(message),
            AppCommand::ShowDocument { title, markdown } => {
                let document = Document::new(title, &markdown);
                log::info!("document: {} ({} blocks)", document.title, document.blocks.len());
                self.document = Some(document);
            }
            AppCommand::CloseDocument => self.document = None,
            AppCommand::NvimState(state) => self.nvim_panel.update(state),
            AppCommand::NvimExited { pid } => self.nvim_panel.exited(pid),
        }
        if tabs_change {
            self.refresh_plugins(cx);
        }
        cx.notify();
    }
    fn open_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab_id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        let pane_id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        let view = cx.new(|cx| TerminalView::new(pane_id, cx));
        let events = cx.subscribe_in(&view, window, Self::on_terminal_event);
        let mut panes = HashMap::new();
        panes.insert(pane_id, Pane { view, _events: events });
        self.tabs.push(Tab {
            id: tab_id,
            root: PaneNode::Leaf(pane_id),
            panes,
            active_pane: pane_id,
        });
        self.select_tab(self.tabs.len() - 1, window, cx);
    }

    fn on_terminal_event(
        &mut self,
        view: &Entity<TerminalView>,
        event: &TerminalViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalViewEvent::TitleChanged(_) => {
                self.refresh_plugins(cx);
                cx.notify();
            }
            TerminalViewEvent::Exited => {
                self.close_pane(view.read(cx).id, window, cx);
            }
        }
    }

    fn close_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else { return };
        let before = self.tab_bounds.clone();
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            cx.quit();
            return;
        }
        self.animate_tab_layout(&before);
        let next = if index < self.active { self.active - 1 } else { self.active };
        self.select_tab(next.min(self.tabs.len() - 1), window, cx);
    }

    fn close_pane(&mut self, pane_id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.panes.contains_key(&pane_id)) else { return };
        if self.tabs[index].panes.len() == 1 {
            let tab_id = self.tabs[index].id;
            self.close_tab(tab_id, window, cx);
            return;
        }
        let tab = &mut self.tabs[index];
        let root = std::mem::replace(&mut tab.root, PaneNode::Leaf(pane_id));
        let (root, removed) = root.without(pane_id);
        if !removed {
            unreachable!("pane registry and pane tree must agree");
        }
        tab.root = root.expect("closing the last pane closes the tab");
        tab.panes.remove(&pane_id);
        if tab.active_pane == pane_id {
            tab.active_pane = tab.panes.keys().next().copied().expect("surviving pane must exist");
        }
        let active = tab.active_pane;
        let handle = tab.panes.get(&active).expect("active pane must exist").view.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    fn split_active(&mut self, direction: SplitDirection, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.tabs[self.active].active_pane;
        let pane_id = PaneId(self.next_pane_id);
        let split_id = SplitId(self.next_split_id);
        self.next_split_id += 1;
        self.next_pane_id += 1;
        let view = cx.new(|cx| TerminalView::new(pane_id, cx));
        let events = cx.subscribe_in(&view, window, Self::on_terminal_event);
        let tab = &mut self.tabs[self.active];
        let inserted = tab.root.split(active, direction, pane_id, split_id);
        debug_assert!(inserted);
        tab.panes.insert(pane_id, Pane { view, _events: events });
        tab.active_pane = pane_id;
        let handle = tab.panes.get(&pane_id).expect("new pane must exist").view.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    fn focus_pane(&mut self, pane_id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        if !tab.panes.contains_key(&pane_id) {
            return;
        }
        tab.active_pane = pane_id;
        let handle = tab.panes.get(&pane_id).expect("focused pane must exist").view.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    fn focus_direction(&mut self, direction: SplitDirection, toward_second: bool, window: &mut Window, cx: &mut Context<Self>) {
        let tab = &self.tabs[self.active];
        let Some(target) = tab.root.focus_neighbor(tab.active_pane, direction, toward_second) else { return };
        self.focus_pane(target, window, cx);
    }

    fn resize_active(&mut self, direction: SplitDirection, delta: f32, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[self.active];
        if tab.root.resize(tab.active_pane, direction, delta) {
            cx.notify();
        }
    }


    /// Keeps the same tab active after the reorder.
    fn move_tab(&mut self, from: usize, to: usize) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let before = self.tab_bounds.clone();
        let active_id = self.tabs[self.active].id;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = self.tabs.iter().position(|t| t.id == active_id).unwrap_or(0);
        self.animate_tab_layout(&before);
    }

    /// FLIP: predict where each remaining tab lands (widths do not change on a
    /// reorder or a close) and remember how far it has to slide from where it was.
    fn animate_tab_layout(&mut self, before: &HashMap<TabId, Bounds<Pixels>>) {
        let Some(first_x) = before.values().map(|b| b.origin.x).min_by(|a, b| a.partial_cmp(b).expect("finite")) else {
            return;
        };
        let mut x = first_x;
        let mut offsets = HashMap::new();
        for tab in &self.tabs {
            let Some(was) = before.get(&tab.id) else { return };
            let offset = was.origin.x - x;
            if offset != px(0.0) {
                offsets.insert(tab.id, offset);
            }
            x += was.size.width + px(TAB_GAP);
        }
        if offsets.is_empty() {
            return;
        }
        self.tab_motion = TabMotion { generation: self.tab_motion.generation + 1, offsets };
    }

    /// Called after the tab strip's children are laid out, in tab order.
    fn record_tab_bounds(&mut self, bounds: Vec<Bounds<Pixels>>) {
        self.tab_bounds = self.tabs.iter().zip(bounds).map(|(tab, b)| (tab.id, b)).collect();
    }


    fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else { return };
        self.active = index;
        let handle = tab
            .panes
            .get(&tab.active_pane)
            .expect("active pane must belong to tab")
            .view
            .read(cx)
            .focus_handle(cx);
        window.focus(&handle);
        self.tab_scroll.scroll_to_item(index);
        cx.notify();
    }

    // --- control socket ---------------------------------------------------------

    fn start_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match ControlServer::bind(ControlServer::default_path()) {
            Ok((server, mut events)) => {
                self.control = Control::Listening(server);
                cx.spawn_in(window, async move |this, cx| {
                    while let Some(event) = events.next().await {
                        let alive = this.update_in(cx, |ws, window, cx| ws.on_control_event(event, window, cx)).is_ok();
                        if !alive {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(error) => {
                self.push_log(format!("control socket: {error}"));
                self.control = Control::Failed(error.to_string());
            }
        }
    }

    fn on_control_event(&mut self, event: ControlEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            ControlEvent::Command(command) => self.execute(AppCommand::from(command), window, cx),
            ControlEvent::BadLine { line, error } => self.push_log(format!("control: {error}: {line}")),
        }
        cx.notify();
    }

    // --- plugins ---------------------------------------------------------------

    fn start_plugins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match NodeHost::spawn(&self.plugin_dir) {
            Ok((host, mut events)) => {
                self.plugins = Plugins::Running(host);
                cx.spawn_in(window, async move |this, cx| {
                    while let Some(event) = events.next().await {
                        let alive = this.update_in(cx, |ws, window, cx| ws.on_host_event(event, window, cx)).is_ok();
                        if !alive {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(error) => {
                self.push_log(format!("plugins: {error}"));
                self.plugins = Plugins::Stopped { error: error.to_string() };
            }
        }
    }

    fn on_host_event(&mut self, event: HostEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            HostEvent::Message(message) => match message {
                HostMessage::Ready { plugins } => {
                    self.plugin_names = plugins;
                    self.refresh_plugins(cx);
                }
                HostMessage::Rendered { views, .. } => self.plugin_views = views,
                HostMessage::Actions { commands, .. } => {
                    for command in commands {
                        self.execute(AppCommand::from(command), window, cx);
                    }
                }
                HostMessage::Failed { message, .. } => self.push_log(format!("plugin action failed: {message}")),
                HostMessage::Changed { plugins } => {
                    self.push_log(format!("plugins reloaded: {}", plugins.join(", ")));
                    self.plugin_names = plugins;
                    self.refresh_plugins(cx);
                }
                HostMessage::Log { message } => self.push_log(message),
            },
            HostEvent::Exited(reason) => {
                self.push_log(reason.clone());
                self.plugins = Plugins::Stopped { error: reason };
                self.plugin_views.clear();
            }
        }
        cx.notify();
    }

    /// Ask the host to render again; the answer arrives as `Rendered`.
    fn refresh_plugins(&mut self, cx: &App) {
        let state = self.plugin_state(cx);
        let sent = match &mut self.plugins {
            Plugins::Running(host) => host.render(&state).map(drop),
            Plugins::Stopped { .. } => Ok(()),
        };
        if let Err(error) = sent {
            self.push_log(format!("plugins: {error}"));
        }
    }

    fn reload_plugins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sent = match &mut self.plugins {
            Plugins::Running(host) => host.reload(),
            Plugins::Stopped { .. } => {
                self.start_plugins(window, cx);
                Ok(())
            }
        };
        if let Err(error) = sent {
            self.push_log(format!("reload plugins: {error}"));
        }
    }

    fn plugin_action(&mut self, plugin: &str, action: &str, cx: &mut Context<Self>) {
        let state = self.plugin_state(cx);
        let sent = match &mut self.plugins {
            Plugins::Running(host) => host.action(plugin, action, &state).map(drop),
            Plugins::Stopped { .. } => Ok(()),
        };
        if let Err(error) = sent {
            self.push_log(format!("{plugin}: {error}"));
        }
    }

    fn plugin_state(&self, cx: &App) -> PluginState {
        PluginState {
            active_tab: self.active,
            tabs: self
                .tabs
                .iter()
                .map(|tab| {
                    let view = tab.panes.get(&tab.active_pane).expect("active pane must belong to tab").view.read(cx);
                    TabInfo { id: tab.id.0, title: view.title().to_owned() }
                })
                .collect(),
        }
    }

    fn push_log(&mut self, message: String) {
        log::info!("{message}");
        self.log.push_back(message);
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    fn update_split_drag(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.split_drag.as_ref() else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some(bounds) = self.split_bounds.get(&drag.id).copied() else { return };
        let span = match drag.direction {
            SplitDirection::Right => f32::from(bounds.size.width).max(1.0),
            SplitDirection::Down => f32::from(bounds.size.height).max(1.0),
        };
        let delta = match drag.direction {
            SplitDirection::Right => f32::from(event.position.x - drag.start.x) / span,
            SplitDirection::Down => f32::from(event.position.y - drag.start.y) / span,
        };
        let ratio = (drag.ratio + delta).clamp(0.1, 0.9);
        if self.tabs[self.active].root.set_ratio(drag.id, ratio) {
            cx.notify();
        }
    }

    fn on_drag_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.split_drag.is_some() {
            if event.pressed_button == Some(MouseButton::Left) {
                self.update_split_drag(event, cx);
            } else {
                self.split_drag = None;
            }
            return;
        }
        match (&self.window_drag, event.pressed_button) {
            (Some(drag), Some(MouseButton::Left)) => drag.follow_pointer(),
            (Some(_), _) => self.window_drag = None,
            (None, _) => {}
        }
    }

    fn on_drag_end(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.split_drag = None;
        self.window_drag = None;
    }

    /// `cmd-1`..`cmd-9` select a tab; everything else bubbles up untouched.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if !keystroke.modifiers.platform {
            return;
        }
        let Some(number) = keystroke.key.parse::<usize>().ok().filter(|n| (1..=9).contains(n)) else { return };
        self.execute(AppCommand::SelectTab(number - 1), window, cx);
        cx.stop_propagation();
    }

    // --- rendering ---------------------------------------------------------------

    /// The strip where macOS would draw the title: brand, tabs, and our own
    /// minimize/close buttons. Only the title label moves the window (by
    /// hand, see `titlebar.rs`); dragging a tab reorders it; the tab list
    /// scrolls sideways when it overflows.
    fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let last = self.tabs.len().saturating_sub(1);
        let generation = self.tab_motion.generation;
        let tabs = self.tabs.iter().enumerate().map(|(index, tab)| {
            let active = index == self.active;
            let id = tab.id;
            let title = tab
                .panes
                .get(&tab.active_pane)
                .expect("active pane must belong to tab")
                .view
                .read(cx)
                .title()
                .to_owned();
            let dragged = DraggedTab { index, title: title.clone() };
            let close = div()
                .id(("close-tab", index))
                .size(px(16.0))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme::muted())
                .cursor_pointer()
                .hover(|s| s.bg(theme::error()).text_color(theme::bg()))
                .child("×")
                // A press on × must not start a tab drag or count as selecting the tab.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |ws, _, window, cx| {
                    cx.stop_propagation();
                    ws.execute(AppCommand::CloseTab(id), window, cx)
                }));
            let element = div()
                .id(("tab", index))
                .flex_none()
                .flex()
                .items_center()
                .gap_2()
                .pl_3()
                .pr_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_sm()
                .when(active, |el| el.bg(theme::raised()).text_color(theme::text()))
                .when(!active, |el| el.text_color(theme::muted()).hover(|s| s.bg(theme::panel())))
                .child(format!("{}  {}", index + 1, title))
                .child(close)
                .on_click(cx.listener(move |ws, _, window, cx| ws.execute(AppCommand::SelectTab(index), window, cx)))
                .on_drag(dragged, |tab: &DraggedTab, position, _, cx| {
                    let title = tab.title.clone();
                    cx.new(|_| TabDragPreview { title, position })
                })
                .drag_over::<DraggedTab>(|style, _, _, _| {
                    style.bg(theme::accent().opacity(0.25)).border_l_2().border_color(theme::accent())
                })
                .on_drop(cx.listener(move |ws, tab: &DraggedTab, window, cx| {
                    ws.execute(AppCommand::MoveTab { from: tab.index, to: index }, window, cx)
                }));
            // A tab that just changed slot starts where it was and eases into place.
            match self.tab_motion.offsets.get(&id).copied() {
                Some(slide) => element
                    .with_animation(
                        ElementId::NamedInteger(format!("tab-slide-{}", id.0).into(), generation),
                        Animation::new(TAB_SLIDE).with_easing(ease_out_quint()),
                        move |tab, delta| tab.relative().left(slide * (1.0 - delta)),
                    )
                    .into_any_element(),
                None => element.into_any_element(),
            }
        });
        let measure = cx.entity();
        let window_button = |id: &'static str, glyph: &'static str, hover: gpui::Hsla| {
            div()
                .id(id)
                .size(px(22.0))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme::muted())
                .cursor_pointer()
                .hover(move |s| s.bg(hover).text_color(theme::bg()))
                .child(glyph)
        };
        div()
            .id("titlebar")
            .h(px(theme::TITLEBAR_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .px_3()
            .gap_3()
            .bg(theme::titlebar())
            .border_b_1()
            .border_color(theme::border())
            .on_mouse_down(MouseButton::Left, |event, window, cx| {
                if event.click_count >= 2 {
                    window.zoom_window();
                    cx.stop_propagation();
                }
            })
            .child(
                // The window's grab handle.
                div()
                    .id("brand")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(theme::accent())
                    .cursor(CursorStyle::OpenHand)
                    .hover(|s| s.bg(theme::panel()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|ws, _, window, _| ws.window_drag = WindowDrag::begin(window)),
                    )
                    .child("◆ terminal_workflows"),
            )
            .child(
                div()
                    // Measured before it becomes stateful: the callback only exists on the plain Div.
                    .on_children_prepainted(move |bounds, _, cx| measure.update(cx, |ws, _| ws.record_tab_bounds(bounds)))
                    .id("tabs")
                    .flex()
                    .items_center()
                    .gap_1()
                    .min_w_0()
                    .overflow_x_scroll()
                    .track_scroll(&self.tab_scroll)
                    .children(tabs),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .id("new-tab")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_color(theme::muted())
                            .hover(|s| s.bg(theme::panel()))
                            .child("+")
                            .on_click(cx.listener(|ws, _, window, cx| ws.execute(AppCommand::NewTab, window, cx))),
                    ),
            )
            .child(
                // Empty space: drop a tab on it to send the tab to the end.
                div()
                    .id("titlebar-space")
                    .flex_1()
                    .h_full()
                    .drag_over::<DraggedTab>(|style, _, _, _| style.bg(theme::accent().opacity(0.1)))
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .on_drop(cx.listener(move |ws, tab: &DraggedTab, window, cx| {
                        ws.execute(AppCommand::MoveTab { from: tab.index, to: last }, window, cx)
                    })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(window_button("minimize", "–", theme::warn()).on_click(|_, window, _| window.minimize_window()))
                    .child(window_button("maximize", "□", theme::accent()).on_click(|_, window, _| window.zoom_window()))
                    .child(window_button("close", "×", theme::error()).on_click(|_, window, _| window.remove_window())),
            )
    }

    fn render_document(&self, document: &Document, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().text_color(theme::accent()).child(document.title.clone()))
                    .child(
                        div()
                            .id("close-document")
                            .size(px(18.0))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .text_color(theme::muted())
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::raised()).text_color(theme::text()))
                            .child("×")
                            .on_click(cx.listener(|ws, _, window, cx| ws.execute(AppCommand::CloseDocument, window, cx))),
                    ),
            )
            .child(markdown::render(document, &window.text_style()))
    }

    fn render_sidebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let listener = cx.listener(|ws, (plugin, action): &(String, String), _, cx| ws.plugin_action(plugin, action, cx));
        let dispatch: Dispatch =
            Rc::new(move |plugin: &str, action: &str, window, cx| listener(&(plugin.to_owned(), action.to_owned()), window, cx));

        let status = match &self.plugins {
            Plugins::Running(_) => div()
                .text_xs()
                .text_color(theme::muted())
                .child(format!("node · {} loaded · cmd-r reloads", self.plugin_names.len())),
            Plugins::Stopped { error } => div()
                .text_xs()
                .text_color(theme::error())
                .child(format!("plugins stopped · cmd-r restarts\n{error}")),
        };

        let socket = match &self.control {
            Control::Listening(server) => div()
                .text_xs()
                .text_color(theme::muted())
                .child(format!("socket  ·  {}", server.path().display())),
            Control::Failed(error) => div().text_xs().text_color(theme::error()).child(format!("socket  ·  {error}")),
        };

        div()
            .id("sidebar")
            .w(px(theme::SIDEBAR_WIDTH))
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .border_l_1()
            .border_color(theme::border())
            .overflow_y_scroll()
            .children(self.document.as_ref().map(|document| self.render_document(document, window, cx)))
            .child(socket)
            .child(div().text_xs().text_color(theme::muted()).child(format!("plugins  ·  {}", self.plugin_dir.display())))
            .child(status)
            .children(self.plugin_views.iter().cloned().map(|view| widgets::plugin_card(view, dispatch.clone())))
            .child(self.nvim_panel.render())
            .child(div().text_xs().text_color(theme::muted()).child("log"))
    }
    fn render_node(&self, tab: &Tab, node: &PaneNode, _active: PaneId, cx: &mut Context<Self>) -> AnyElement {
        match node {
            PaneNode::Leaf(id) => {
                let view = tab.panes.get(id).expect("pane tree and registry must agree").view.clone();
                let pane = div()
                    .id(("pane", id.0))
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .capture_any_mouse_down(cx.listener({
                        let id = *id;
                        move |ws, _, window, cx| ws.focus_pane(id, window, cx)
                    }))
                    .child(view);
                pane.into_any_element()
            }
            PaneNode::Split { id: split_id, direction, ratio, first, second } => {
                let split_id = *split_id;
                let direction = *direction;
                let workspace = cx.entity();
                let mut container = div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .on_children_prepainted(move |bounds, _, cx| {
                        let (Some(first), Some(last)) = (bounds.first().copied(), bounds.last().copied()) else { return };
                        let bounds = Bounds {
                            origin: first.origin,
                            size: Size {
                                width: last.origin.x + last.size.width - first.origin.x,
                                height: last.origin.y + last.size.height - first.origin.y,
                            },
                        };
                        workspace.update(cx, |workspace, _| {
                            workspace.split_bounds.insert(split_id, bounds);
                        });
                    });
                container = match direction {
                    SplitDirection::Right => container.flex_row(),
                    SplitDirection::Down => container.flex_col(),
                };
                let first = Self::pane_slot(self.render_node(tab, first, _active, cx), *ratio);
                let second = Self::pane_slot(self.render_node(tab, second, _active, cx), 1.0 - *ratio);
                let drag_ratio = *ratio;
                let divider = match direction {
                    SplitDirection::Right => div()
                        .id(("split-divider", split_id.0))
                        .flex_none()
                        .w(px(4.0))
                        .h_full()
                        .cursor(CursorStyle::ResizeLeftRight)
                        .bg(theme::border())
                        .on_mouse_down(MouseButton::Left, cx.listener(move |workspace, event: &MouseDownEvent, _, cx| {
                            workspace.split_drag = Some(SplitDrag {
                                id: split_id,
                                direction,
                                start: event.position,
                                ratio: drag_ratio,
                            });
                            cx.stop_propagation();
                        })),
                    SplitDirection::Down => div()
                        .id(("split-divider", split_id.0))
                        .h(px(4.0))
                        .w_full()
                        .cursor(CursorStyle::ResizeUpDown)
                        .bg(theme::border())
                        .on_mouse_down(MouseButton::Left, cx.listener(move |workspace, event: &MouseDownEvent, _, cx| {
                            workspace.split_drag = Some(SplitDrag {
                                id: split_id,
                                direction,
                                start: event.position,
                                ratio: drag_ratio,
                            });
                            cx.stop_propagation();
                        })),
                };
                container.child(first).child(divider).child(second).into_any_element()
            }
        }
    }

    fn pane_slot(element: AnyElement, weight: f32) -> AnyElement {
        let mut slot = div()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .flex_shrink_0()
            .flex_basis(px(0.0));
        slot.style().flex_grow = Some(weight.max(0.01));
        slot.child(element).into_any_element()
    }
}


impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminal = self
            .tabs
            .get(self.active)
            .map(|tab| self.render_node(tab, &tab.root, tab.active_pane, cx));
        div()
            .key_context(actions::WORKSPACE)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|ws, _: &NewTab, window, cx| ws.execute(AppCommand::NewTab, window, cx)))
            .on_action(cx.listener(|ws, _: &CloseTab, window, cx| ws.execute(AppCommand::CloseActiveTab, window, cx)))
            .on_action(cx.listener(|ws, _: &NextTab, window, cx| ws.execute(AppCommand::NextTab, window, cx)))
            .on_action(cx.listener(|ws, _: &PrevTab, window, cx| ws.execute(AppCommand::PrevTab, window, cx)))
            .on_action(cx.listener(|ws, _: &ReloadPlugins, window, cx| ws.execute(AppCommand::ReloadPlugins, window, cx)))
            .on_action(cx.listener(|ws, _: &MoveTabLeft, window, cx| ws.execute(AppCommand::MoveActiveTab(Direction::Left), window, cx)))
            .on_action(cx.listener(|ws, _: &MoveTabRight, window, cx| ws.execute(AppCommand::MoveActiveTab(Direction::Right), window, cx)))
            .on_action(cx.listener(|ws, _: &CloseDocument, window, cx| ws.execute(AppCommand::CloseDocument, window, cx)))
            .on_action(cx.listener(|ws, _: &SplitRight, window, cx| ws.split_active(SplitDirection::Right, window, cx)))
            .on_action(cx.listener(|ws, _: &SplitDown, window, cx| ws.split_active(SplitDirection::Down, window, cx)))
            .on_action(cx.listener(|ws, _: &FocusLeft, window, cx| ws.focus_direction(SplitDirection::Right, false, window, cx)))
            .on_action(cx.listener(|ws, _: &FocusRight, window, cx| ws.focus_direction(SplitDirection::Right, true, window, cx)))
            .on_action(cx.listener(|ws, _: &FocusUp, window, cx| ws.focus_direction(SplitDirection::Down, false, window, cx)))
            .on_action(cx.listener(|ws, _: &FocusDown, window, cx| ws.focus_direction(SplitDirection::Down, true, window, cx)))
            .on_action(cx.listener(|ws, _: &ResizeLeft, _, cx| ws.resize_active(SplitDirection::Right, -0.05, cx)))
            .on_action(cx.listener(|ws, _: &ResizeRight, _, cx| ws.resize_active(SplitDirection::Right, 0.05, cx)))
            .on_action(cx.listener(|ws, _: &ResizeUp, _, cx| ws.resize_active(SplitDirection::Down, -0.05, cx)))
            .on_action(cx.listener(|ws, _: &ResizeDown, _, cx| ws.resize_active(SplitDirection::Down, 0.05, cx)))
            .capture_any_mouse_down(cx.listener(|_, event: &MouseDownEvent, window, cx| {
                if event.button != MouseButton::Left {
                    return;
                }
                if let Some(edge) = resize_edge(event.position, window.bounds().size) {
                    window.start_window_resize(edge);
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_move(cx.listener(Self::on_drag_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_drag_end))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_drag_end))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .child(self.render_titlebar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .children(terminal),
                    )
                    .child(self.render_sidebar(window, cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{PaneId, PaneNode, SplitDirection, SplitId};

    #[test]
    fn split_navigation_and_resize_stay_on_the_same_axis() {
        let mut root = PaneNode::Leaf(PaneId(1));
        assert!(root.split(PaneId(1), SplitDirection::Right, PaneId(2), SplitId(1)));
        assert_eq!(root.focus_neighbor(PaneId(1), SplitDirection::Right, true), Some(PaneId(2)));
        assert_eq!(root.focus_neighbor(PaneId(2), SplitDirection::Right, false), Some(PaneId(1)));
        assert!(root.resize(PaneId(1), SplitDirection::Right, 0.1));
        assert!(root.set_ratio(SplitId(1), 0.8));
        match root {
            PaneNode::Split { ratio, .. } => assert!((ratio - 0.8).abs() < f32::EPSILON),
            PaneNode::Leaf(_) => panic!("split must remain a split"),
        }
    }

    #[test]
    fn removing_a_pane_collapses_its_parent() {
        let mut root = PaneNode::Leaf(PaneId(1));
        root.split(PaneId(1), SplitDirection::Down, PaneId(2), SplitId(1));
        let (root, removed) = root.without(PaneId(1));
        assert!(removed);
        assert!(matches!(root, Some(PaneNode::Leaf(PaneId(2)))));
    }
}
