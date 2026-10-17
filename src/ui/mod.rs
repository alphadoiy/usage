//! ratatui 界面。支持 Braille 进度条、Canvas 圆环和圆点状态栏。

mod app;
mod codex;
mod history;
mod home;
mod menu;
mod overview;
mod session_browser;
/// Reusable subscription quota widgets; the home Provider board uses the model catalog.
pub mod widgets;

pub use app::{run, run_history, run_history_with_query, run_with_sessions};
pub use codex::run as run_codex;
pub use codex::run_with_config as run_codex_with_config;
