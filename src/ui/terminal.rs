//! Terminal entry and exit for every screen. Unlike `ratatui::try_init`, the panic hook is
//! installed once and restores the terminal only when the UI thread itself panics: a panicking
//! background task must not tear the screen down under a TUI that keeps running.
use ratatui::DefaultTerminal;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::prelude::CrosstermBackend;
use std::io::{self, stdout};
use std::sync::Mutex;
use std::thread::ThreadId;

/// The thread drawing the current screen, while one is shown.
static UI_THREAD: Mutex<Option<ThreadId>> = Mutex::new(None);

pub(crate) fn enter() -> io::Result<DefaultTerminal> {
    install_panic_hook();
    *UI_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current().id());
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    ratatui::Terminal::new(CrosstermBackend::new(stdout()))
}

pub(crate) fn leave() {
    *UI_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = None;
    ratatui::restore();
}

fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let ui = *UI_THREAD.lock().unwrap_or_else(|e| e.into_inner());
            match ui {
                Some(thread) if thread == std::thread::current().id() => {
                    leave();
                    previous(info);
                }
                // Printing would scribble over the screen; the task's own error path reports it.
                Some(_) => tracing::error!(event = "background_panic", panic = %info),
                None => previous(info),
            }
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_background_panic_leaves_the_ui_thread_registered() {
        install_panic_hook();
        *UI_THREAD.lock().unwrap() = Some(std::thread::current().id());
        let worker = std::thread::spawn(|| panic!("synthetic background panic"));
        assert!(worker.join().is_err());
        assert_eq!(
            *UI_THREAD.lock().unwrap(),
            Some(std::thread::current().id())
        );
        *UI_THREAD.lock().unwrap() = None;
    }
}
