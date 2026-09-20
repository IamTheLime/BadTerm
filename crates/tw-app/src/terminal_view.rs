//! One tab: a `Session` painted on a canvas, with keyboard, mouse, clipboard
//! and a find bar.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};
use futures::StreamExt;
use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, ContentMask, Context, Corners, CursorStyle, EventEmitter, FocusHandle,
    Focusable, Font, FontStyle, FontWeight, Hsla, KeyDownEvent, Modifiers, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, Render, RenderImage, ScrollDelta, ScrollWheelEvent, ShapedLine, SharedString, StrikethroughStyle,
    TextRun, UnderlineStyle, Window, canvas, div, fill, font, outline, point, prelude::*, px, size,
};
use tw_terminal::{
    Attrs, CellContent, CellWidth, CursorShape, Grid, ImageLayer, ImagePixels, ImagePlacement, KeyInput, Mods,
    MouseButton, MouseInput, MousePhase, PtyRead, PtySpec, Row, SearchMatch, Session, TerminalEvent, UnderlineKind,
};

use crate::actions::{CloseFind, Copy, Find, FindNext, FindPrev, Paste, SelectAll};
use crate::command::PaneId;
use crate::theme;

const MIN_UNSHAPED_GAP: usize = 3;
const SEGMENT_CACHE_LIMIT: usize = 8192;

/// What the workspace needs to know about a tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalViewEvent {
    TitleChanged(String),
    Exited,
}

/// The shell either runs or never started; a tab shows either way.
enum Shell {
    Running(Session),
    Failed(String),
}

/// Where the grid was painted last frame, for turning mouse positions into cells.
#[derive(Clone, Copy)]
struct Layout {
    bounds: Bounds<Pixels>,
    cell_width: f32,
    line_height: f32,
}

struct FindState {
    query: String,
    matches: Vec<SearchMatch>,
    current: usize,
}

struct PointingHands {
    word: String,
}

/// A Kitty image uploaded to the GPU, valid while its generation matches.
#[derive(Clone)]
struct CachedImage {
    generation: u64,
    width: u32,
    height: u32,
    image: Arc<RenderImage>,
    /// Frame counter of the last frame that painted it; unplaced images age out by it.
    last_used: u64,
}

struct CachedTextRow {
    row: Row<Hsla>,
    block_cursor: Option<(usize, Hsla)>,
    segments: Vec<Segment>,
}

#[derive(PartialEq, Eq)]
struct SegmentKey {
    text: String,
    runs: Vec<TextRun>,
    font_size_bits: u32,
}

// GPUI's TextRun has equality but no Hash implementation.
impl Hash for SegmentKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text.hash(state);
        self.font_size_bits.hash(state);
        self.runs.len().hash(state);
        for run in &self.runs {
            run.len.hash(state);
            run.font.hash(state);
            run.color.hash(state);
            run.background_color.hash(state);
            run.underline.hash(state);
            run.strikethrough.hash(state);
        }
    }
}

/// How many images stay on the GPU after they leave the screen, so a picker
/// stepping back to a recent preview does not upload it again.
const RECENT_IMAGES: usize = 8;

pub struct TerminalView {
    pub id: PaneId,
    shell: Shell,
    focus_handle: FocusHandle,
    font: Font,
    title: String,
    layout: Option<Layout>,
    find: Option<FindState>,
    hovered_word: Option<String>,
    last_mouse_position: Option<Point<Pixels>>,
    pointing: Option<PointingHands>,
    pointing_image: Arc<RenderImage>,
    /// Kitty images by id: everything placed this frame plus a few recent ones.
    images: HashMap<u32, CachedImage>,
    /// Last shaped rows, reused when Neovim redraws the same contents.
    text_rows: Vec<CachedTextRow>,
    shaped_segments: HashMap<SegmentKey, ShapedLine>,
    frame: u64,
    last_resize: Option<Instant>,
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    pub fn new(id: PaneId, cx: &mut Context<Self>) -> Self {
        let shell = match Session::spawn(PtySpec::login_shell(80, 24)) {
            Ok((session, mut output)) => {
                cx.spawn(async move |this, cx| {
                    while let Some(first) = output.next().await {
                        // Drain the burst so one frame covers everything that arrived.
                        let mut batch = vec![first];
                        while let Ok(more) = output.try_recv() {
                            batch.push(more);
                        }
                        let alive = this
                            .update(cx, |view, cx| {
                                if let Shell::Running(session) = &mut view.shell {
                                    let bytes: usize = batch.iter().map(PtyRead::len).sum();
                                    let started = std::time::Instant::now();
                                    for read in batch {
                                        session.feed(read);
                                    }
                                    let took = started.elapsed();
                                    if took.as_millis() >= 5 {
                                        log::debug!("tab {:?}: fed {} bytes in {} ms", view.id, bytes, took.as_millis());
                                    } else {
                                        log::debug!("tab {:?}: {} bytes from the shell", view.id, bytes);
                                    }
                                }
                                view.after_output(cx);
                            })
                            .is_ok();
                        if !alive {
                            break;
                        }
                    }
                })
                .detach();
                Shell::Running(session)
            }
            Err(error) => Shell::Failed(error.to_string()),
        };
        Self {
            id,
            shell,
            focus_handle: cx.focus_handle(),
            font: font(theme::FONT_FAMILY),
            title: String::new(),
            layout: None,
            find: None,
            hovered_word: None,
            last_mouse_position: None,
            pointing: None,
            pointing_image: pointing_hand_image(),
            images: HashMap::new(),
            text_rows: Vec::new(),
            shaped_segments: HashMap::new(),
            frame: 0,
            last_resize: None,
        }
    }

    pub fn title(&self) -> &str {
        if self.title.is_empty() { "shell" } else { &self.title }
    }

    /// Send text as if typed (used by plugins and, later, the socket).
    pub fn write(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Shell::Running(session) = &mut self.shell {
            if let Err(error) = session.write(text.as_bytes()) {
                log::warn!("write to shell: {error}");
            }
            cx.notify();
        }
    }

    fn after_output(&mut self, cx: &mut Context<Self>) {
        let Shell::Running(session) = &mut self.shell else { return };
        for event in session.take_events() {
            match event {
                TerminalEvent::TitleChanged(title) => {
                    self.title = title.clone();
                    cx.emit(TerminalViewEvent::TitleChanged(title));
                }
                TerminalEvent::PwdChanged(_) | TerminalEvent::Bell => {}
                TerminalEvent::Exited(_) => cx.emit(TerminalViewEvent::Exited),
            }
        }
        cx.notify();
    }

    // --- keyboard -----------------------------------------------------------
    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // Let GPUI's multi-key workspace binding consume the prefix, but keep it out of the shell.
        if keystroke.key == "b" && keystroke.modifiers.control {
            return;
        }
        if keystroke.key == "p" && keystroke.modifiers.shift && (keystroke.modifiers.control || keystroke.modifiers.platform) {
            return;
        }
        if keystroke.key == "f8" {
            self.toggle_pointing_hands(cx);
            cx.stop_propagation();
            return;
        }
        // cmd combinations belong to the app (tabs, quit, copy), never to the shell.
        if keystroke.modifiers.platform {
            return;
        }
        if self.find.is_some() {
            let edited = match (keystroke.key.as_str(), typed_text(keystroke.key_char.as_deref())) {
                ("backspace", _) => self.find.as_mut().map(|find| find.query.pop()).is_some(),
                (_, Some(text)) => {
                    if let Some(find) = &mut self.find {
                        find.query.push_str(&text);
                    }
                    true
                }
                (_, None) => false,
            };
            if edited {
                cx.stop_propagation();
                self.run_search(cx);
            }
            return;
        }

        let Shell::Running(session) = &mut self.shell else { return };
        let input = KeyInput::from_name(&keystroke.key, mods_from(&keystroke.modifiers), typed_text(keystroke.key_char.as_deref()));
        if let Err(error) = session.key(&input) {
            log::warn!("key: {error}");
        }
        session.scroll_to_bottom();
        cx.stop_propagation();
        cx.notify();
    }
    pub(crate) fn toggle_pointing_hands(&mut self, cx: &mut Context<Self>) {
        let Some(word) = self.hovered_word.clone() else { return };
        let same_word = self.pointing.as_ref().is_some_and(|pointing| pointing.word == word);
        self.pointing = (!same_word).then_some(PointingHands { word });
        cx.notify();
    }

    fn update_hovered_word(&mut self, position: Point<Pixels>) {
        self.hovered_word = self.word_at_position(position);
    }

    pub(crate) fn toggle_pointing_hands_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.update_hovered_word(position);
        self.toggle_pointing_hands(cx);
    }

    fn word_at_position(&mut self, position: Point<Pixels>) -> Option<String> {
        let layout = self.layout?;
        let x = f32::from(position.x - layout.bounds.origin.x);
        let y = f32::from(position.y - layout.bounds.origin.y);
        let col = (x / layout.cell_width).floor() as usize;
        let row = (y / layout.line_height).floor() as usize;
        let Shell::Running(session) = &mut self.shell else { return None };
        let grid = session.grid(|_| ()).ok()?;
        word_at(&grid, row, col)
    }

    // --- mouse ---------------------------------------------------------------

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.last_mouse_position = Some(event.position);
        window.focus(&self.focus_handle);
        let Some(button) = button_from(event.button) else { return };
        self.send_mouse(MousePhase::Press(button), event.position, &event.modifiers, cx);
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(button) = button_from(event.button) else { return };
        self.send_mouse(MousePhase::Release(button), event.position, &event.modifiers, cx);
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.last_mouse_position = Some(event.position);
        self.update_hovered_word(event.position);
        let held = event.pressed_button.and_then(button_from);
        self.send_mouse(MousePhase::Move { held }, event.position, &event.modifiers, cx);
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / (theme::FONT_SIZE * theme::LINE_HEIGHT_FACTOR),
        };
        if lines.abs() < 0.01 {
            return;
        }
        self.send_mouse(MousePhase::Wheel { lines }, event.position, &event.modifiers, cx);
    }

    fn send_mouse(&mut self, phase: MousePhase, position: Point<Pixels>, modifiers: &Modifiers, cx: &mut Context<Self>) {
        let Some(input) = self.mouse_input(phase, position, modifiers) else { return };
        let Shell::Running(session) = &mut self.shell else { return };
        if let Err(error) = session.mouse(&input) {
            log::warn!("mouse: {error}");
        }
        // Plain pointer motion changes nothing visible; everything else may.
        if !matches!(phase, MousePhase::Move { held: None }) {
            cx.notify();
        }
    }

    fn mouse_input(&self, phase: MousePhase, position: Point<Pixels>, modifiers: &Modifiers) -> Option<MouseInput> {
        let layout = self.layout?;
        let x = f32::from(position.x - layout.bounds.origin.x).max(0.0);
        let y = f32::from(position.y - layout.bounds.origin.y).max(0.0);
        Some(MouseInput {
            phase,
            col: (x / layout.cell_width).floor() as u16,
            row: (y / layout.line_height).floor() as u16,
            x,
            y,
            mods: mods_from(modifiers),
        })
    }

    // --- clipboard and find ----------------------------------------------------

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let Shell::Running(session) = &self.shell else { return };
        match session.selected_text() {
            Ok(Some(text)) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            Ok(None) => {}
            Err(error) => log::warn!("copy: {error}"),
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        let Shell::Running(session) = &mut self.shell else { return };
        if let Err(error) = session.paste(&text) {
            log::warn!("paste: {error}");
        }
        session.scroll_to_bottom();
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        let Shell::Running(session) = &mut self.shell else { return };
        if let Err(error) = session.select_all() {
            log::warn!("select all: {error}");
        }
        cx.notify();
    }

    fn find(&mut self, _: &Find, _: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            self.find = Some(FindState { query: String::new(), matches: Vec::new(), current: 0 });
        }
        cx.notify();
    }

    fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step_find(1, cx);
    }

    fn find_prev(&mut self, _: &FindPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step_find(-1, cx);
    }

    fn close_find(&mut self, _: &CloseFind, _: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        if let Shell::Running(session) = &mut self.shell {
            if let Err(error) = session.clear_selection() {
                log::warn!("clear selection: {error}");
            }
            session.scroll_to_bottom();
        }
        cx.notify();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let (Some(find), Shell::Running(session)) = (&mut self.find, &mut self.shell) else { return };
        match session.search(&find.query) {
            Ok(matches) => {
                find.matches = matches;
                find.current = 0;
                let shown = match find.matches.first() {
                    Some(first) => session.show_match(first),
                    None => session.clear_selection(),
                };
                if let Err(error) = shown {
                    log::warn!("find: {error}");
                }
            }
            Err(error) => log::warn!("find: {error}"),
        }
        cx.notify();
    }

    fn step_find(&mut self, delta: isize, cx: &mut Context<Self>) {
        let (Some(find), Shell::Running(session)) = (&mut self.find, &mut self.shell) else { return };
        let count = find.matches.len() as isize;
        if count == 0 {
            return;
        }
        find.current = (find.current as isize + delta).rem_euclid(count) as usize;
        if let Err(error) = session.show_match(&find.matches[find.current]) {
            log::warn!("find: {error}");
        }
        cx.notify();
    }

    // --- layout and paint --------------------------------------------------------

    /// Runs inside the canvas prepaint: fit the grid to the bounds, then
    /// snapshot it into something paint can draw without touching `self`.
    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> Option<PaintPlan> {
        let Shell::Running(session) = &mut self.shell else { return None };
        let font_size = px(theme::FONT_SIZE);
        let font_id = cx.text_system().resolve_font(&self.font);
        let cell_width = cx
            .text_system()
            .advance(font_id, font_size, 'M')
            .map(|advance| advance.width)
            .unwrap_or(font_size * 0.6);
        let line_height = (font_size * theme::LINE_HEIGHT_FACTOR).round();
        if self.layout.is_some_and(|old| old.cell_width != f32::from(cell_width) || old.line_height != f32::from(line_height)) {
            self.text_rows.clear();
        }
        self.layout = Some(Layout { bounds, cell_width: f32::from(cell_width), line_height: f32::from(line_height) });

        let cols = (f32::from(bounds.size.width) / f32::from(cell_width)).floor().max(2.0) as u16;
        let rows = (f32::from(bounds.size.height) / f32::from(line_height)).floor().max(1.0) as u16;
        // Window drags can produce a stream of dimensions. Do not synchronously
        // reflow Ghostty and resize the PTY for every intermediate frame.
        let now = Instant::now();
        let resize_allowed = self.last_resize.map_or(true, |last| now.duration_since(last) >= Duration::from_millis(50));
        if resize_allowed {
            match session.resize(cols, rows, f32::from(cell_width) as u16, f32::from(line_height) as u16) {
                Ok(true) => log::debug!("tab {:?}: grid {cols}x{rows}, cell {cell_width:?}x{line_height:?}", self.id),
                Ok(false) => {}
                Err(error) => log::warn!("resize: {error}"),
            }
            self.last_resize = Some(now);
        }

        let started = std::time::Instant::now();
        let grid = match session.grid(theme::term) {
            Ok(grid) => grid,
            Err(error) => {
                log::warn!("grid: {error}");
                return None;
            }
        };
        let grid_took = started.elapsed();
        let mut plan = PaintPlan::build(
            &grid,
            &self.font,
            font_size,
            cell_width,
            line_height,
            &mut self.text_rows,
            &mut self.shaped_segments,
            self.pointing_image.clone(),
            window,
        );
        plan.cell_width = cell_width;
        plan.pointing = self
            .pointing
            .as_ref()
            .map(|pointing| visible_word_matches(&grid, &pointing.word))
            .unwrap_or_default();
        let plan_took = started.elapsed() - grid_took;
        if grid_took.as_millis() + plan_took.as_millis() >= 10 {
            log::debug!(
                "tab {:?}: grid {} ms, text layout {} ms, {} quads, {} segments",
                self.id,
                grid_took.as_millis(),
                plan_took.as_millis(),
                plan.quads.len(),
                plan.segments.len(),
            );
        }

        // Upload new or changed images once, keep the ones still placed plus a few recent
        // ones, forget the rest. One image usually has many placements (a virtual placement
        // is one run per row), so entries added earlier in this same pass count as hits too.
        self.frame += 1;
        let frame = self.frame;
        let mut kept: HashMap<u32, CachedImage> = HashMap::new();
        let mut placed = 0;
        for placement in &grid.images {
            let hit = kept
                .get(&placement.image_id)
                .or_else(|| self.images.get(&placement.image_id))
                .filter(|c| c.generation == placement.generation)
                .cloned();
            let mut cached = match hit {
                Some(cached) => cached,
                None => match session.image_pixels(placement.image_id) {
                    Ok(Some(pixels)) => {
                        let started = std::time::Instant::now();
                        let (w, h) = (pixels.width, pixels.height);
                        let cached = upload(pixels, frame);
                        log::debug!("tab {:?}: uploaded image {} ({w}x{h}) in {} ms", self.id, placement.image_id, started.elapsed().as_millis());
                        cached
                    }
                    Ok(None) => continue,
                    Err(error) => {
                        log::warn!("kitty image {}: {error}", placement.image_id);
                        continue;
                    }
                },
            };
            cached.last_used = frame;
            plan.images.push(ImagePaint::new(placement, &cached, cell_width, line_height));
            if kept.insert(placement.image_id, cached).is_none() {
                placed += 1;
            }
        }
        // Carry over the most recently used unplaced images, newest first, up to the limit.
        let mut recent: Vec<(u32, CachedImage)> =
            self.images.iter().filter(|(id, _)| !kept.contains_key(id)).map(|(id, c)| (*id, c.clone())).collect();
        recent.sort_by_key(|(_, c)| std::cmp::Reverse(c.last_used));
        for (id, cached) in recent.into_iter().take(RECENT_IMAGES.saturating_sub(placed)) {
            kept.insert(id, cached);
        }
        let previously_placed = self.images.values().filter(|c| c.last_used == frame - 1).count();
        if placed != previously_placed {
            log::debug!("tab {:?}: {placed} kitty image(s) on screen", self.id);
        }
        // Textures that fell out of the cache (or were re-transmitted) leave the GPU atlas.
        for (id, old) in std::mem::replace(&mut self.images, kept) {
            let still_cached = self.images.get(&id).is_some_and(|new| Arc::ptr_eq(&new.image, &old.image));
            if !still_cached && let Err(error) = window.drop_image(old.image) {
                log::debug!("drop image {id}: {error}");
            }
        }
        Some(plan)
    }

    fn render_find_bar(&self, find: &FindState) -> impl IntoElement {
        let status = match (find.query.is_empty(), find.matches.len()) {
            (true, _) => "type to search".to_owned(),
            (false, 0) => "no matches".to_owned(),
            (false, total) => format!("{} of {total}", find.current + 1),
        };
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_1()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .text_sm()
            .child(div().text_color(theme::accent()).child("find"))
            .child(
                div()
                    .min_w(px(220.0))
                    .px_2()
                    .rounded_sm()
                    .bg(theme::bg())
                    .text_color(theme::text())
                    .font_family(theme::FONT_FAMILY)
                    .child(format!("{}▏", find.query)),
            )
            .child(div().text_color(theme::muted()).child(status))
            .child(div().text_xs().text_color(theme::muted()).child("enter next · shift-enter previous · esc close"))
    }
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let key_context = if self.find.is_some() { "Terminal Find" } else { "Terminal" };
        let find_bar = self.find.as_ref().map(|find| self.render_find_bar(find));
        let body = match &self.shell {
            Shell::Running(_) => {
                let view = cx.entity();
                canvas(
                    move |bounds, window, cx| view.update(cx, |this, cx| this.layout(bounds, window, cx)),
                    |bounds, plan, window, cx| {
                        if let Some(plan) = plan {
                            plan.paint(bounds, window, cx);
                        }
                    },
                )
                .size_full()
                .into_any_element()
            }
            Shell::Failed(error) => div()
                .p_4()
                .text_color(theme::error())
                .child(format!("could not start the shell: {error}"))
                .into_any_element(),
        };
        div()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_prev))
            .on_action(cx.listener(Self::close_find))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(gpui::MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_down(gpui::MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(gpui::MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up(gpui::MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(gpui::MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .cursor(CursorStyle::IBeam)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .children(find_bar)
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// Only text the shell should receive as typed characters. Control characters
/// ("\n" for enter, "\t" for tab) and macOS private-use code points for arrows
/// are dropped so Ghostty's encoder decides what those keys send.
fn typed_text(key_char: Option<&str>) -> Option<String> {
    key_char
        .filter(|text| !text.is_empty() && text.chars().all(|c| !c.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&c)))
        .map(str::to_owned)
}

fn row_chars<C>(row: &Row<C>) -> Vec<char> {
    let mut chars = Vec::new();
    for cell in &row.cells {
        match &cell.content {
            CellContent::Blank => chars.push(' '),
            CellContent::Text(text) => chars.extend(text.chars()),
        }
    }
    chars
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn word_at_chars(chars: &[char], col: usize) -> Option<String> {
    if col >= chars.len() || !is_word_char(chars[col]) {
        return None;
    }
    let start = (0..=col).rev().find(|&index| index == 0 || !is_word_char(chars[index - 1]))?;
    let end = (col..chars.len()).find(|&index| !is_word_char(chars[index])).unwrap_or(chars.len());
    Some(chars[start..end].iter().collect())
}

fn word_at<C>(grid: &Grid<C>, row: usize, col: usize) -> Option<String> {
    grid.rows.get(row).map(row_chars).and_then(|chars| word_at_chars(&chars, col))
}

fn word_matches_in_chars(chars: &[char], word: &str, row: u32) -> Vec<SearchMatch> {
    let needle: Vec<char> = word.chars().collect();
    if needle.is_empty() || chars.len() < needle.len() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for col in 0..=chars.len() - needle.len() {
        if chars[col..col + needle.len()] != needle[..] {
            continue;
        }
        let left_boundary = col == 0 || !is_word_char(chars[col - 1]);
        let right = col + needle.len();
        let right_boundary = right == chars.len() || !is_word_char(chars[right]);
        if left_boundary && right_boundary {
            matches.push(SearchMatch { row, col: col as u16, len: needle.len() as u16 });
        }
    }
    matches
}

fn visible_word_matches<C>(grid: &Grid<C>, word: &str) -> Vec<SearchMatch> {
    let mut matches = Vec::new();
    for (row_index, row) in grid.rows.iter().enumerate() {
        matches.extend(word_matches_in_chars(&row_chars(row), word, row_index as u32));
    }
    matches
}

fn mods_from(modifiers: &Modifiers) -> Mods {
    let mut mods = Mods::empty();
    if modifiers.shift {
        mods |= Mods::SHIFT;
    }
    if modifiers.control {
        mods |= Mods::CTRL;
    }
    if modifiers.alt {
        mods |= Mods::ALT;
    }
    if modifiers.platform {
        mods |= Mods::SUPER;
    }
    mods
}

#[cfg(test)]
mod tests {
    use super::{word_at_chars, word_matches_in_chars};

    #[test]
    fn hovered_word_uses_identifier_boundaries() {
        let chars: Vec<char> = "  nvim_point word42  ".chars().collect();
        assert_eq!(word_at_chars(&chars, 4).as_deref(), Some("nvim_point"));
        assert_eq!(word_at_chars(&chars, 16).as_deref(), Some("word42"));
        assert_eq!(word_at_chars(&chars, 0), None);
    }

    #[test]
    fn pointing_matches_skip_substrings() {
        let chars: Vec<char> = "word sword word".chars().collect();
        assert_eq!(
            word_matches_in_chars(&chars, "word", 3),
            vec![
                tw_terminal::SearchMatch { row: 3, col: 0, len: 4 },
                tw_terminal::SearchMatch { row: 3, col: 11, len: 4 },
            ]
        );
    }
}

/// gpui's buttons are an open set (it also has navigation buttons); only these three matter here.
fn button_from(button: gpui::MouseButton) -> Option<MouseButton> {
    match button {
        gpui::MouseButton::Left => Some(MouseButton::Left),
        gpui::MouseButton::Right => Some(MouseButton::Right),
        gpui::MouseButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

/// Everything paint needs, computed in prepaint where `self` is available.
struct PaintPlan {
    line_height: Pixels,
    cell_width: Pixels,
    background: Hsla,
    quads: Vec<(Bounds<Pixels>, Hsla)>,
    segments: Vec<Segment>,
    cursor: Option<CursorPaint>,
    images: Vec<ImagePaint>,
    pointing: Vec<SearchMatch>,
    pointing_image: Arc<RenderImage>,
}

/// One placed Kitty image. gpui paints whole images, so the full image is
/// scaled so that the visible source rectangle lands on `clip`, and the paint
/// is clipped to `clip`.
struct ImagePaint {
    layer: ImageLayer,
    clip: Bounds<Pixels>,
    full: Bounds<Pixels>,
    image: Arc<RenderImage>,
}

impl ImagePaint {
    fn new(placement: &ImagePlacement, cached: &CachedImage, cell_width: Pixels, line_height: Pixels) -> Self {
        let origin = point(
            cell_width * f32::from(placement.col) + px(placement.x_offset_px as f32),
            line_height * f32::from(placement.row) + px(placement.y_offset_px as f32),
        );
        let clip = Bounds { origin, size: size(cell_width * placement.cols as f32, line_height * placement.rows as f32) };
        let scale_x = f32::from(clip.size.width) / placement.source.width as f32;
        let scale_y = f32::from(clip.size.height) / placement.source.height as f32;
        let full = Bounds {
            origin: point(
                origin.x - px(placement.source.x as f32 * scale_x),
                origin.y - px(placement.source.y as f32 * scale_y),
            ),
            size: size(px(cached.width as f32 * scale_x), px(cached.height as f32 * scale_y)),
        };
        Self { layer: placement.layer, clip, full, image: cached.image.clone() }
    }
}

/// gpui textures are BGRA with straight alpha (see how its `img` element
/// converts decoded frames), so swap the red and blue channels.
fn upload(pixels: ImagePixels, frame: u64) -> CachedImage {
    let ImagePixels { width, height, generation, mut rgba } = pixels;
    let started = std::time::Instant::now();
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    log::debug!("bgra swap of {}x{} in {} ms", width, height, started.elapsed().as_millis());
    let buffer = image::RgbaImage::from_raw(width, height, rgba).expect("session checked the buffer length");
    CachedImage {
        generation,
        width,
        height,
        image: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
        last_used: frame,
    }
}

/// A run of cells shaped as one line. Wide characters get their own segment so
/// a fallback glyph with an odd advance cannot shift the cells after it.
#[derive(Clone)]
struct Segment {
    origin: Point<Pixels>,
    line: ShapedLine,
}

enum CursorPaint {
    Filled(Bounds<Pixels>, Hsla),
    Hollow(Bounds<Pixels>, Hsla),
}

/// Text being accumulated for the current segment.
struct SegmentBuilder {
    origin: Point<Pixels>,
    text: String,
    runs: Vec<TextRun>,
    last_key: Option<(Attrs, Hsla)>,
    has_text: bool,
}

impl SegmentBuilder {
    fn new(origin: Point<Pixels>) -> Self {
        Self { origin, text: String::new(), runs: Vec::new(), last_key: None, has_text: false }
    }

    fn push(&mut self, piece: &str, attrs: Attrs, color: Hsla, base: &Font) {
        let key = (attrs, color);
        match (&mut self.runs.last_mut(), self.last_key == Some(key)) {
            (Some(run), true) => run.len += piece.len(),
            _ => {
                self.runs.push(text_run(piece.len(), attrs, color, base));
                self.last_key = Some(key);
            }
        }
        self.text.push_str(piece);
    }

    fn finish(
        self,
        font_size: Pixels,
        window: &Window,
        shaped_segments: &mut HashMap<SegmentKey, ShapedLine>,
    ) -> Option<Segment> {
        if !self.has_text {
            return None;
        }
        let key = SegmentKey { text: self.text, runs: self.runs, font_size_bits: f32::from(font_size).to_bits() };
        let line = if let Some(line) = shaped_segments.get(&key) {
            line.clone()
        } else {
            let line = window.text_system().shape_line(SharedString::from(key.text.clone()), font_size, &key.runs, None);
            if shaped_segments.len() >= SEGMENT_CACHE_LIMIT {
                shaped_segments.clear();
            }
            shaped_segments.insert(key, line.clone());
            line
        };
        Some(Segment { origin: self.origin, line })
    }
}

impl PaintPlan {
    fn build(
        grid: &Grid<Hsla>,
        base: &Font,
        font_size: Pixels,
        cell_width: Pixels,
        line_height: Pixels,
        text_rows: &mut Vec<CachedTextRow>,
        shaped_segments: &mut HashMap<SegmentKey, ShapedLine>,
        pointing_image: Arc<RenderImage>,
        window: &Window,
    ) -> Self {
        let mut quads = Vec::new();
        let mut segments = Vec::new();
        let block_cursor_at = grid
            .cursor
            .as_ref()
            .filter(|c| c.shape == CursorShape::Block)
            .map(|c| (usize::from(c.row), usize::from(c.col)));

        for (row_index, row) in grid.rows.iter().enumerate() {
            let y = line_height * row_index as f32;
            let block_cursor = block_cursor_at.filter(|(cursor_row, _)| *cursor_row == row_index).map(|(_, col)| (col, grid.background));
            let cached_segments = text_rows
                .get(row_index)
                .filter(|cached| cached.row == *row && cached.block_cursor == block_cursor)
                .map(|cached| cached.segments.clone());
            let segments_start = segments.len();
            let mut background_run: Option<(usize, usize, Hsla)> = None;
            let mut flush_background = |(start, end, color): (usize, usize, Hsla)| {
                quads.push((
                    Bounds {
                        origin: point(cell_width * start as f32, y),
                        size: size(cell_width * (end - start) as f32, line_height),
                    },
                    color,
                ));
            };
            let mut segment = SegmentBuilder::new(point(px(0.0), y));
            let mut spaces = Vec::new();
            for (col, cell) in row.cells.iter().enumerate() {
                let x = cell_width * col as f32;
                let span = match cell.width {
                    CellWidth::Single => 1,
                    CellWidth::Double => 2,
                    CellWidth::Spacer => continue,
                };
                match (background_run.take(), cell.bg) {
                    (Some((start, end, color)), Some(bg)) if end == col && color == bg => {
                        background_run = Some((start, col + span, color));
                    }
                    (Some(run), Some(bg)) => {
                        flush_background(run);
                        background_run = Some((col, col + span, bg));
                    }
                    (Some(run), None) => flush_background(run),
                    (None, Some(bg)) => background_run = Some((col, col + span, bg)),
                    (None, None) => {}
                }
                if cached_segments.is_some() {
                    continue;
                }
                let color = if block_cursor_at == Some((row_index, col)) {
                    grid.background
                } else if cell.attrs.faint {
                    cell.fg.opacity(0.6)
                } else {
                    cell.fg
                };
                let plain_space = matches!((&cell.content, cell.width), (CellContent::Blank, _) | (CellContent::Text(_), CellWidth::Single))
                    && match &cell.content {
                        CellContent::Blank => true,
                        CellContent::Text(text) => text == " ",
                    }
                    && !cell.attrs.strikethrough
                    && cell.attrs.underline == UnderlineKind::None;
                if plain_space {
                    spaces.push((cell.attrs, color));
                    continue;
                }
                // Short gaps stay in a run; longer ones are positioned by cell coordinates.
                if spaces.len() >= MIN_UNSHAPED_GAP {
                    let done = std::mem::replace(&mut segment, SegmentBuilder::new(point(x, y)));
                    segments.extend(done.finish(font_size, window, shaped_segments));
                    spaces.clear();
                } else {
                    for (attrs, color) in spaces.drain(..) {
                        segment.push(" ", attrs, color, base);
                    }
                }
                match (&cell.content, cell.width) {
                    (CellContent::Blank, _) => {
                        segment.has_text = true;
                        segment.push(" ", cell.attrs, color, base);
                    }
                    (CellContent::Text(text), CellWidth::Single) => {
                        segment.has_text = true;
                        segment.push(text, cell.attrs, color, base);
                    }
                    (CellContent::Text(text), CellWidth::Double | CellWidth::Spacer) => {
                        // Close the narrow run, shape the wide glyph alone at its own x, start a fresh run after it.
                        let done = std::mem::replace(&mut segment, SegmentBuilder::new(point(x, y)));
                        segments.extend(done.finish(font_size, window, shaped_segments));
                        segment.has_text = true;
                        segment.push(text, cell.attrs, color, base);
                        let done = std::mem::replace(&mut segment, SegmentBuilder::new(point(x + cell_width * 2.0, y)));
                        segments.extend(done.finish(font_size, window, shaped_segments));
                    }
                }
            }
            if let Some(run) = background_run {
                flush_background(run);
            }
            if let Some(cached_segments) = cached_segments {
                segments.extend(cached_segments);
            } else {
                segments.extend(segment.finish(font_size, window, shaped_segments));
                let cached = CachedTextRow { row: row.clone(), block_cursor, segments: segments[segments_start..].to_vec() };
                if let Some(slot) = text_rows.get_mut(row_index) {
                    *slot = cached;
                } else {
                    text_rows.push(cached);
                }
            }
        }
        text_rows.truncate(grid.rows.len());

        let cursor = grid.cursor.as_ref().map(|c| {
            let origin = point(cell_width * f32::from(c.col), line_height * f32::from(c.row));
            let cell = Bounds { origin, size: size(cell_width, line_height) };
            let color = c.color.opacity(0.5);
            match c.shape {
                CursorShape::Block => CursorPaint::Filled(cell, color),
                CursorShape::Bar => CursorPaint::Filled(Bounds { origin, size: size(px(2.0), line_height) }, color),
                CursorShape::Underline => CursorPaint::Filled(
                    Bounds { origin: point(origin.x, origin.y + line_height - px(2.0)), size: size(cell_width, px(2.0)) },
                    color,
                ),
                CursorShape::Hollow => CursorPaint::Hollow(cell, color),
            }
        });

        Self {
            line_height,
            cell_width,
            background: grid.background,
            quads,
            segments,
            cursor,
            images: Vec::new(),
            pointing: Vec::new(),
            pointing_image,
        }
    }

    fn paint(self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let origin = bounds.origin;
        let place = |b: Bounds<Pixels>| Bounds { origin: b.origin + origin, size: b.size };
        let paint_images = |layer: ImageLayer, window: &mut Window| {
            for image in self.images.iter().filter(|image| image.layer == layer) {
                let clip = place(image.clip).intersect(&bounds);
                window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                    if let Err(error) = window.paint_image(place(image.full), Corners::default(), image.image.clone(), 0, false) {
                        log::warn!("paint image: {error}");
                    }
                });
            }
        };

        window.paint_quad(fill(bounds, self.background));
        paint_images(ImageLayer::BelowBackground, window);
        for (b, color) in &self.quads {
            window.paint_quad(fill(place(*b), *color));
        }
        paint_images(ImageLayer::BelowText, window);
        if let Some(CursorPaint::Filled(b, color)) = &self.cursor {
            window.paint_quad(fill(place(*b), *color));
        }
        for segment in &self.segments {
            if let Err(error) = segment.line.paint(segment.origin + origin, self.line_height, window, cx) {
                log::warn!("paint line: {error}");
            }
        }
        paint_images(ImageLayer::AboveText, window);
        for found in &self.pointing {
            paint_pointing_hand(window, bounds, origin, self.cell_width, self.line_height, *found, self.pointing_image.clone());
        }
        if let Some(CursorPaint::Hollow(b, color)) = &self.cursor {
            window.paint_quad(outline(place(*b), *color, BorderStyle::Solid));
        }
    }
}

fn paint_pointing_hand(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    canvas_origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    found: SearchMatch,
    image: Arc<RenderImage>,
) {
    let scale = (f32::from(line_height) / 20.0).clamp(0.65, 1.5);
    let width = 40.0 * scale;
    let height = width;
    let word_x = f32::from(cell_width) * f32::from(found.col);
    let tip_x = word_x + f32::from(cell_width) * 0.5;
    let max_x = (f32::from(bounds.size.width) - width).max(0.0);
    let x = (tip_x - width * 21.0 / 24.0).clamp(0.0, max_x);
    let row_y = f32::from(line_height) * found.row as f32;
    let max_y = (f32::from(bounds.size.height) - height).max(0.0);
    let y = (row_y + f32::from(line_height) * 0.65 - height * 21.0 / 24.0).clamp(0.0, max_y);
    let image_bounds = Bounds {
        origin: point(canvas_origin.x + px(x), canvas_origin.y + px(y)),
        size: size(px(width), px(height)),
    };
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        if let Err(error) = window.paint_image(image_bounds, Corners::default(), image, 0, false) {
            log::warn!("paint pointer image: {error}");
        }
    });
}
/// The pointer asset supplied for the references overlay.
const POINTER_SVG: &[u8] = br##"<?xml version="1.0" encoding="utf-8"?>
<!-- License: MIT. Made by basicons: https://basicons.xyz/ -->
<svg width="800px" height="800px" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg">
<path d="M22 10.2069L3 3L10.2069 22L13.4828 13.4828L22 10.2069Z" transform="rotate(180 12 12)" fill="#ffffff" stroke="#000000" stroke-width="0.6" stroke-linecap="round" stroke-linejoin="round"/>
</svg>"##;

fn pointing_hand_image() -> Arc<RenderImage> {
    let tree = resvg::usvg::Tree::from_data(POINTER_SVG, &resvg::usvg::Options::default())
        .expect("the embedded pointer SVG must parse");
    let source_size = tree.size();
    let scale = 96.0 / source_size.width().max(source_size.height());
    let width = (source_size.width() * scale).ceil() as u32;
    let height = (source_size.height() * scale).ceil() as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).expect("the pointer SVG must have a valid size");
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let mut image = image::RgbaImage::from_raw(width, height, pixmap.take())
        .expect("the pointer SVG dimensions must match its pixel buffer");
    for pixel in image.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![image::Frame::new(image)]))
}


fn text_run(len: usize, attrs: Attrs, color: Hsla, base: &Font) -> TextRun {
    let underline = match attrs.underline {
        UnderlineKind::None => None,
        UnderlineKind::Single | UnderlineKind::Double | UnderlineKind::Dotted | UnderlineKind::Dashed => {
            Some(UnderlineStyle { thickness: px(1.0), color: Some(color), wavy: false })
        }
        UnderlineKind::Curly => Some(UnderlineStyle { thickness: px(1.0), color: Some(color), wavy: true }),
    };
    TextRun {
        len,
        font: Font {
            weight: if attrs.bold { FontWeight::BOLD } else { FontWeight::NORMAL },
            style: if attrs.italic { FontStyle::Italic } else { FontStyle::Normal },
            ..base.clone()
        },
        color,
        background_color: None,
        underline,
        strikethrough: attrs.strikethrough.then(|| StrikethroughStyle { thickness: px(1.0), color: Some(color) }),
    }
}
