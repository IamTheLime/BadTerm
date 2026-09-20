use std::collections::VecDeque;

use gpui::{div, prelude::*};
use tw_scripting::NvimState;

const MAX_EVENTS: usize = 24;

#[derive(Clone, Debug)]
pub enum NvimSessionState {
    Waiting,
    Running { pid: u32 },
    Exited,
}

pub struct NvimPanel {
    state: NvimSessionState,
    snapshot: Option<NvimState>,
    events: VecDeque<String>,
}

impl Default for NvimPanel {
    fn default() -> Self {
        Self {
            state: NvimSessionState::Waiting,
            snapshot: None,
            events: VecDeque::from(["waiting · run nn --integration".to_owned()]),
        }
    }
}

impl NvimPanel {
    pub fn update(&mut self, state: NvimState) {
        let event = match self.snapshot.as_ref().map(|previous| previous.pid) {
            None => format!("session started · pid {}", state.pid),
            Some(pid) if pid != state.pid => format!("session switched · pid {}", state.pid),
            Some(_) => format!("{} · {}:{}", state.mode, state.file, state.line),
        };
        self.snapshot = Some(state.clone());
        self.state = NvimSessionState::Running { pid: state.pid };
        self.push_event(event);
    }
    pub fn is_running(&self) -> bool {
        matches!(self.state, NvimSessionState::Running { .. })
    }

    pub fn exited(&mut self, pid: u32) {
        if self.snapshot.as_ref().is_some_and(|state| state.pid == pid) {
            self.state = NvimSessionState::Exited;
            self.push_event(format!("session exited · pid {pid}"));
        }
    }

    pub fn render(&self) -> impl IntoElement {
        let status = match &self.state {
            NvimSessionState::Waiting => "waiting · run nn --integration".to_owned(),
            NvimSessionState::Running { pid } => format!("running · pid {pid}"),
            NvimSessionState::Exited => "exited · run nn --integration".to_owned(),
        };
        let snapshot = self.snapshot.as_ref().map(|state| {
            format!(
                "{}:{}:{} · {}{} · {} lines\n{:?}\n{}",
                state.file,
                state.line,
                state.column,
                state.mode,
                if state.modified { " · modified" } else { "" },
                state.lines,
                state.line_text,
                state.cwd,
            )
        });

        div()
            .id("nvim-event-viewer")
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_md()
            .bg(crate::theme::panel())
            .border_1()
            .border_color(crate::theme::border())
            .child(div().text_xs().text_color(crate::theme::accent()).child("shell Neovim"))
            .child(div().text_xs().text_color(crate::theme::muted()).child(status))
            .children(snapshot.map(|snapshot| div().text_xs().text_color(crate::theme::muted()).child(snapshot)))
            .child(div().mt_1().text_xs().text_color(crate::theme::muted()).child("events"))
            .children(self.events.iter().rev().take(8).map(|event| div().text_xs().text_color(crate::theme::text()).child(event.clone())))
    }

    fn push_event(&mut self, event: String) {
        self.events.push_back(event);
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }
}
