//! ratatui 界面。支持 Braille 进度条、Canvas 圆环和圆点状态栏。

mod app;
mod history;
mod session_browser;
mod widgets;

pub use app::{run, run_history, run_history_with_query, run_with_sessions};
