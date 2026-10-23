//! ratatui 界面。支持 Braille 进度条、Canvas 圆环和圆点状态栏。

mod app;
mod confirmation;
mod forms;
mod history;
mod home;
mod menu;
mod overview;
mod performance;
mod provider_actions;
mod provider_manager;
mod provider_setup;
mod session_browser;
mod subscription_tests;
pub(crate) mod terminal;
/// Reusable subscription quota widgets; the home Provider board uses the model catalog.
pub mod widgets;

pub use app::{run, run_history, run_history_with_query, run_with_sessions};
pub use provider_manager::{ProjectSessions, run_with_config as run_provider_manager_with_config};
