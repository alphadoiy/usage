//! Selected conversation reads are cancellable and scoped to the exact Home and generation.
use super::*;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub key: sessions::SessionKey,
    home: PathBuf,
    recency: i64,
}
impl Request {
    fn for_session(session: &sessions::Session) -> Self {
        Self {
            key: session.key(),
            home: session.home.clone(),
            recency: session.recency_at_ms,
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    generation: u64,
    request: Option<Request>,
    task: Option<Task>,
    cached: std::collections::VecDeque<(
        Request,
        Result<sessions::preview::Preview, String>,
        std::time::Instant,
    )>,
}

impl Dashboard {
    pub(super) fn ensure_session_preview(&mut self, handle: &Handle) {
        let selected = self
            .browser_state
            .panel
            .as_ref()
            .and_then(|b| b.selected_session())
            .cloned();
        let desired = selected.as_ref().map(Request::for_session);
        self.preview
            .cached
            .retain(|(_, _, at)| at.elapsed() < Duration::from_secs(10));
        if desired == self.preview.request
            && (desired.is_none()
                || self.preview.task.is_some()
                || desired
                    .as_ref()
                    .is_some_and(|r| self.preview.cached.iter().any(|(key, _, _)| key == r)))
        {
            return;
        }
        let selection_changed = desired != self.preview.request;
        self.preview.task = None;
        self.preview.generation = self.preview.generation.wrapping_add(1);
        self.preview.request = desired.clone();
        if let Some(browser) = &mut self.browser_state.panel {
            if selection_changed {
                browser.preview = None;
            }
            browser.preview_loading = false;
        }
        let (Some(session), Some(request)) = (selected, desired) else {
            return;
        };
        if let Some((_, result, _)) = self
            .preview
            .cached
            .iter()
            .find(|(key, _, _)| key == &request)
        {
            if let Some(browser) = &mut self.browser_state.panel {
                browser.preview = Some(result.clone());
            }
            return;
        }
        if let Some(browser) = &mut self.browser_state.panel {
            browser.preview_loading = true;
        }
        let generation = self.preview.generation;
        let tx = self.tx.clone();
        self.preview.task = Some(Task::blocking(
            handle,
            move |cancel| sessions::preview::load(&session, &cancel),
            move |result| {
                let _ = tx.send(AppEvent::SessionPreview {
                    generation,
                    request,
                    result,
                });
            },
        ));
    }

    pub(super) fn accept_session_preview(
        &mut self,
        generation: u64,
        request: Request,
        result: Result<sessions::preview::Preview, String>,
    ) -> bool {
        if generation != self.preview.generation || self.preview.request.as_ref() != Some(&request)
        {
            return false;
        }
        self.preview.task = None;
        if self.preview.cached.len() >= 12 {
            self.preview.cached.pop_front();
        }
        self.preview
            .cached
            .push_back((request.clone(), result.clone(), std::time::Instant::now()));
        if let Some(browser) = &mut self.browser_state.panel
            && browser
                .selected_session()
                .map(Request::for_session)
                .as_ref()
                == Some(&request)
        {
            browser.preview_loading = false;
            browser.preview = Some(result);
            return true;
        }
        false
    }
}
