//! Cancellable preview, explicit confirmation and one in-flight account operation.
mod status;
use super::confirmation::{Confirmation, Decision};
use crate::provider_actions::{OperationHandle, Outcome, Report};
use ratatui::{
    Frame,
    crossterm::event::KeyCode,
    layout::Rect,
    widgets::{Block, Clear, Paragraph, Wrap},
};
pub(super) use status::Statuses;
use std::{sync::mpsc, time::Duration};
use tokio::runtime::Handle;

type Stage = Confirmation<Report>;
pub(super) struct Panel {
    name: String,
    operation: OperationHandle,
    stage: Stage,
    tx: mpsc::Sender<Result<Report, String>>,
    rx: mpsc::Receiver<Result<Report, String>>,
    task: Option<tokio::task::JoinHandle<()>>,
    refresh: bool,
    started_at: chrono::DateTime<chrono::Utc>,
    status_update: Option<Result<Report, String>>,
}
impl Drop for Panel {
    fn drop(&mut self) {
        if matches!(self.stage, Stage::Loading)
            && let Some(task) = &self.task
        {
            task.abort();
        }
    }
}
impl Panel {
    pub fn open(name: String, operation: OperationHandle, handle: &Handle) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut panel = Self {
            name,
            operation,
            stage: Stage::Loading,
            tx,
            rx,
            task: None,
            refresh: false,
            started_at: chrono::Utc::now(),
            status_update: None,
        };
        panel.start(handle, false);
        panel
    }
    fn start(&mut self, handle: &Handle, execute: bool) {
        self.started_at = chrono::Utc::now();
        let operation = self.operation.clone();
        let tx = self.tx.clone();
        self.task = Some(handle.spawn(async move {
            let future = async {
                if execute {
                    operation.0.execute().await
                } else {
                    operation.0.preview().await
                }
            };
            let result = tokio::time::timeout(Duration::from_secs(30), future)
                .await
                .unwrap_or_else(|_| Err("签到操作超时，结果尚未确认；请重新查询状态".into()));
            let _ = tx.send(result);
        }));
    }
    pub fn poll(&mut self) -> bool {
        let Ok(result) = self.rx.try_recv() else {
            return false;
        };
        self.task = None;
        if self.operation.0.automatic_status() || matches!(self.stage, Stage::Executing) {
            self.status_update = Some(result.clone());
        }
        self.stage = match result {
            Ok(report)
                if matches!(self.stage, Stage::Loading) && report.outcome == Outcome::Ready =>
            {
                Stage::Confirm(report)
            }
            Ok(report) => {
                self.refresh = matches!(report.outcome, Outcome::Claimed | Outcome::Done);
                Stage::Result(report.message())
            }
            Err(error) => Stage::Result(error),
        };
        true
    }
    pub fn update_status(&mut self, statuses: &mut Statuses) {
        if let Some(result) = self.status_update.take() {
            statuses.record(&self.operation, self.started_at, result);
        }
    }
    pub fn take_refresh(&mut self) -> bool {
        std::mem::take(&mut self.refresh)
    }
    /// True closes the panel. Execution cannot be dismissed or submitted twice.
    pub fn key(&mut self, key: KeyCode, handle: &Handle) -> bool {
        match self.stage.key(key) {
            Decision::Cancel => return true,
            Decision::Execute => self.start(handle, true),
            Decision::Ignore => {}
        }
        false
    }
    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(76);
        let (text, hint) = match &self.stage {
            Stage::Loading => ("正在查询今日签到状态…".into(), "Esc 取消"),
            Stage::Executing => ("正在提交签到，请稍候…".into(), "请勿重复操作"),
            Stage::Confirm(report) => (report.message(), "y 确认签到 · n / Esc 返回"),
            Stage::Result(message) => (message.clone(), "Enter / Esc 返回"),
        };
        let lines = super::overview::wrap(
            vec![
                ratatui::text::Line::from(super::overview::clean(&text)),
                "".into(),
                hint.into(),
            ],
            width.saturating_sub(2),
        );
        let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title(format!(
                    " {} › {} ",
                    super::overview::clean(&self.name),
                    self.operation.0.label()
                )))
                .wrap(Wrap { trim: false }),
            popup,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_actions::AccountOperation;
    use async_trait::async_trait;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Operation(Arc<AtomicUsize>);
    #[async_trait]
    impl AccountOperation for Operation {
        fn id(&self) -> &str {
            "checkin"
        }
        fn label(&self) -> &str {
            "每日签到"
        }
        fn cache_key(&self) -> String {
            "fixture".into()
        }
        async fn preview(&self) -> Result<Report, String> {
            Ok(Report {
                outcome: Outcome::Ready,
                credit: Some(10.0),
                streak: None,
                delayed: false,
            })
        }
        async fn execute(&self) -> Result<Report, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(Report {
                outcome: Outcome::Claimed,
                credit: Some(10.0),
                streak: Some(1),
                delayed: false,
            })
        }
    }
    async fn poll(panel: &mut Panel) {
        for _ in 0..200 {
            if panel.poll() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("account operation did not complete");
    }
    #[tokio::test]
    async fn preview_can_cancel_and_confirmation_submits_once_then_refreshes_once() {
        let count = Arc::new(AtomicUsize::new(0));
        let operation = OperationHandle(Arc::new(Operation(count.clone())));
        let mut cancelled = Panel::open("WorkBuddy".into(), operation.clone(), &Handle::current());
        poll(&mut cancelled).await;
        assert!(cancelled.key(KeyCode::Esc, &Handle::current()));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let mut panel = Panel::open("WorkBuddy".into(), operation, &Handle::current());
        poll(&mut panel).await;
        assert!(!panel.key(KeyCode::Char('y'), &Handle::current()));
        assert!(!panel.key(KeyCode::Char('y'), &Handle::current()));
        assert!(!panel.key(KeyCode::Esc, &Handle::current()));
        poll(&mut panel).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(panel.take_refresh());
        assert!(!panel.take_refresh());
        for width in [32, 100] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 20)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(Paragraph::new("首页"), frame.area());
                    panel.render(frame);
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            let text: String = text
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '│')
                .collect();
            assert!(text.contains("首页"));
            assert!(text.contains("签到成功"));
            assert!(text.contains("10.00Credits"));
        }
        assert!(panel.key(KeyCode::Enter, &Handle::current()));
    }
}
