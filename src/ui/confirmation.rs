//! Shared confirmation transitions; each operation keeps its own business result and task lifetime.
use ratatui::crossterm::event::KeyCode;

pub(super) enum Confirmation<T> {
    Loading,
    Confirm(T),
    Executing,
    Result(String),
}
pub(super) enum Decision {
    Cancel,
    Execute,
    Ignore,
}
impl<T> Confirmation<T> {
    pub fn key(&mut self, key: KeyCode) -> Decision {
        match (&self, key) {
            (Self::Loading | Self::Confirm(_), KeyCode::Esc | KeyCode::Char('n' | 'q')) => {
                Decision::Cancel
            }
            (Self::Result(_), KeyCode::Enter | KeyCode::Esc | KeyCode::Char('n' | 'q')) => {
                Decision::Cancel
            }
            (Self::Confirm(_), KeyCode::Char('y')) => {
                *self = Self::Executing;
                Decision::Execute
            }
            _ => Decision::Ignore,
        }
    }
}
