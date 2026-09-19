use std::collections::BTreeMap;

use rmpv::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UiError {
    #[error("redraw event is not an array")]
    EventNotArray,
    #[error("redraw event has no name")]
    MissingName,
    #[error("redraw event `{event}` has invalid arguments")]
    InvalidArguments { event: String },
    #[error("redraw event `{event}` contains a non-integer {field}")]
    InvalidInteger { event: String, field: &'static str },
    #[error("redraw event `{event}` contains a non-string cell")]
    InvalidCell { event: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiState {
    pub grids: BTreeMap<u64, GridState>,
    pub cursor: Option<CursorPosition>,
    pub flushes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GridState {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<Vec<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorPosition {
    pub grid: u64,
    pub row: usize,
    pub column: usize,
}

impl UiState {
    /// Apply one `redraw` notification. Unknown events are ignored so a newer
    /// Neovim can add UI details without breaking the transport probe.
    pub fn apply_redraw(&mut self, events: &[Value]) -> Result<(), UiError> {
        for event in events {
            let event = event.as_array().ok_or(UiError::EventNotArray)?;
            let name = event.first().and_then(Value::as_str).ok_or(UiError::MissingName)?;
            if event.len() < 2 {
                return Err(UiError::InvalidArguments { event: name.to_owned() });
            }
            for args in &event[1..] {
                let args = args.as_array().ok_or(UiError::InvalidArguments { event: name.to_owned() })?;
                match name {
                    "grid_resize" => self.grid_resize(name, args)?,
                    "grid_clear" => self.grid_clear(name, args)?,
                    "grid_line" => self.grid_line(name, args)?,
                    "grid_cursor_goto" => self.grid_cursor_goto(name, args)?,
                    "flush" => {
                        if !args.is_empty() {
                            return Err(UiError::InvalidArguments { event: name.to_owned() });
                        }
                        self.flushes += 1;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
    fn grid_resize(&mut self, event: &str, args: &[Value]) -> Result<(), UiError> {
        if args.len() != 3 {
            return Err(UiError::InvalidArguments { event: event.to_owned() });
        }
        let grid = identifier(event, args, 0, "grid")?;
        let width = integer(event, args, 1, "width")?;
        let height = integer(event, args, 2, "height")?;
        self.grids.insert(grid, GridState::new(width, height));
        Ok(())
    }

    fn grid_clear(&mut self, event: &str, args: &[Value]) -> Result<(), UiError> {
        if args.len() != 1 {
            return Err(UiError::InvalidArguments { event: event.to_owned() });
        }
        let grid = identifier(event, args, 0, "grid")?;
        if let Some(grid) = self.grids.get_mut(&grid) {
            let (width, height) = (grid.width, grid.height);
            *grid = GridState::new(width, height);
        }
        Ok(())
    }

    fn grid_line(&mut self, event: &str, args: &[Value]) -> Result<(), UiError> {
        if args.len() < 4 {
            return Err(UiError::InvalidArguments { event: event.to_owned() });
        }
        let grid_id = identifier(event, args, 0, "grid")?;
        let row = integer(event, args, 1, "row")?;
        let mut column = integer(event, args, 2, "column")?;
        let Some(grid) = self.grids.get_mut(&grid_id) else { return Ok(()) };
        let Some(cells) = args[3].as_array() else {
            return Err(UiError::InvalidArguments { event: event.to_owned() });
        };
        let Some(line) = grid.cells.get_mut(row) else { return Ok(()) };
        for cell in cells {
            let Some(cell) = cell.as_array() else {
                return Err(UiError::InvalidCell { event: event.to_owned() });
            };
            let Some(text) = cell.first().and_then(Value::as_str) else {
                return Err(UiError::InvalidCell { event: event.to_owned() });
            };
            let repeat = cell.get(2).and_then(Value::as_u64).unwrap_or(1) as usize;
            for _ in 0..repeat {
                if let Some(destination) = line.get_mut(column) {
                    *destination = text.to_owned();
                }
                column += 1;
            }
        }
        Ok(())
    }

    fn grid_cursor_goto(&mut self, event: &str, args: &[Value]) -> Result<(), UiError> {
        if args.len() != 3 {
            return Err(UiError::InvalidArguments { event: event.to_owned() });
        }
        self.cursor = Some(CursorPosition {
            grid: identifier(event, args, 0, "grid")?,
            row: integer(event, args, 1, "row")?,
            column: integer(event, args, 2, "column")?,
        });
        Ok(())
    }
}

impl GridState {
    fn new(width: usize, height: usize) -> Self {
        Self { width, height, cells: vec![vec![String::new(); width]; height] }
    }
}

fn identifier(event: &str, args: &[Value], index: usize, field: &'static str) -> Result<u64, UiError> {
    args.get(index)
        .and_then(Value::as_u64)
        .ok_or_else(|| UiError::InvalidInteger { event: event.to_owned(), field })
}

fn integer(event: &str, args: &[Value], index: usize, field: &'static str) -> Result<usize, UiError> {
    args.get(index)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| UiError::InvalidInteger { event: event.to_owned(), field })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_apply_line_cursor_and_flush_redraws() {
        let mut state = UiState::default();
        state
            .apply_redraw(&[
                Value::Array(vec![
                    Value::from("grid_resize"),
                    Value::Array(vec![Value::from(1u64), Value::from(4u64), Value::from(2u64)]),
                ]),
                Value::Array(vec![
                    Value::from("grid_line"),
                    Value::Array(vec![
                        Value::from(1u64),
                        Value::from(0u64),
                        Value::from(0u64),
                        Value::Array(vec![
                            Value::Array(vec![Value::from("a"), Value::from(0u64), Value::from(2u64)]),
                            Value::Array(vec![Value::from("b"), Value::from(0u64), Value::from(1u64)]),
                        ]),
                        Value::from(false),
                        Value::from(false),
                    ]),
                ]),
                Value::Array(vec![
                    Value::from("grid_cursor_goto"),
                    Value::Array(vec![Value::from(1u64), Value::from(1u64), Value::from(2u64)]),
                ]),
                Value::Array(vec![Value::from("flush"), Value::Array(vec![])]),
            ])
            .unwrap();
        let grid = &state.grids[&1];
        assert_eq!(grid.cells, vec![vec!["a", "a", "b", ""], vec!["", "", "", ""]]);
        assert_eq!(state.cursor, Some(CursorPosition { grid: 1, row: 1, column: 2 }));
        assert_eq!(state.flushes, 1);
    }
}
