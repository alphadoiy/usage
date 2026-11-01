//! Read-only connection observations. These totals never claim ownership of a Usage Run.
use ratatui::{
    Frame,
    crossterm::event::KeyCode,
    layout::Rect,
    widgets::{Block, Clear, Paragraph, Wrap},
};
use serde_json::Value;
use std::sync::mpsc;
use tokio::runtime::Handle;

pub(super) struct Panel {
    id: String,
    pub name: String,
    rows: Vec<Value>,
    total: usize,
    offset: usize,
    selected: usize,
    route: Option<Value>,
    error: Option<String>,
    rx: mpsc::Receiver<Result<Value, String>>,
    task: Option<crate::background::Task>,
    loading: bool,
    route_loading: bool,
    root: std::path::PathBuf,
}
impl Panel {
    pub fn new(handle: &Handle, id: String, name: String) -> Self {
        let (_, rx) = mpsc::channel();
        let mut panel = Self {
            id,
            name,
            rows: Vec::new(),
            total: 0,
            offset: 0,
            selected: 0,
            route: None,
            error: None,
            rx,
            task: None,
            loading: false,
            route_loading: false,
            root: crate::codex_manager::default_root(),
        };
        panel.load(handle, None);
        panel
    }
    fn load(&mut self, handle: &Handle, session: Option<String>) {
        let (tx, rx) = mpsc::channel();
        self.rx = rx;
        self.loading = true;
        self.route_loading = session.is_some();
        self.error = None;
        let root = self.root.clone();
        let id = self.id.clone();
        let offset = self.offset.to_string();
        self.task = Some(crate::background::Task::query(
            handle,
            async move {
                let connection = crate::background::blocking(move || {
                    crate::magpie::saved_connection(&root, &id)
                })
                .await
                .map_err(str::to_owned)??;
                let http = crate::magpie::http();
                match session {
                    Some(session) => {
                        connection
                            .get(http.as_ref(), "v1/magpie/route", &[("session", &session)])
                            .await
                    }
                    None => {
                        connection
                            .get(
                                http.as_ref(),
                                "v1/magpie/usage/requests",
                                &[
                                    ("period", "today"),
                                    ("via", "through"),
                                    ("offset", &offset),
                                    ("limit", "20"),
                                ],
                            )
                            .await
                    }
                }
            },
            move |value| {
                let _ = tx.send(value);
            },
        ));
    }
    pub fn poll(&mut self) -> bool {
        match self.rx.try_recv() {
            Ok(result) => {
                self.loading = false;
                self.task = None;
                match result {
                    Err(e) => self.error = Some(e),
                    Ok(value) if self.route_loading => self.route = Some(value),
                    Ok(value) => {
                        self.rows = value["rows"].as_array().cloned().unwrap_or_default();
                        self.total = value["total"].as_u64().unwrap_or(0) as usize;
                        self.selected = 0;
                        self.route = None;
                    }
                }
                true
            }
            Err(mpsc::TryRecvError::Disconnected) if self.loading => {
                self.loading = false;
                self.error = Some("Magpie 查询任务中断".into());
                true
            }
            _ => false,
        }
    }
    pub fn key(&mut self, key: KeyCode, handle: &Handle) -> bool {
        match key {
            KeyCode::Esc if self.route.is_some() => {
                self.route = None;
            }
            KeyCode::Esc | KeyCode::Char('q') => return true,
            KeyCode::Char('r') => self.load(handle, None),
            KeyCode::Right if !self.loading && self.offset + 20 < self.total => {
                self.offset += 20;
                self.load(handle, None);
            }
            KeyCode::Left if !self.loading => {
                self.offset = self.offset.saturating_sub(20);
                self.load(handle, None);
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1))
            }
            KeyCode::Enter if !self.loading => {
                if let Some(session) = self
                    .rows
                    .get(self.selected)
                    .and_then(|r| r["session"].as_str())
                    .filter(|s| !s.is_empty())
                {
                    self.load(handle, Some(session.to_owned()));
                } else {
                    self.error = Some("此请求没有会话标识，无法查询会话路由".into());
                }
            }
            _ => {}
        }
        false
    }
    pub fn render(&self, frame: &mut Frame) {
        let outer = frame.area();
        let area = Rect::new(
            outer.x + 1,
            outer.y + 1,
            outer.width.saturating_sub(2),
            outer.height.saturating_sub(2),
        );
        frame.render_widget(Clear, area);
        let block = Block::bordered().title(format!(
            " Magpie 请求与路由 · {} ",
            super::overview::clean(&self.name)
        ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = vec![
            "连接范围 · 今日经过 Magpie 的请求 · 不等同于当前实例用量".to_string(),
            "↑↓ 选择 · Enter 会话最新路由 · ←→ 翻页 · r 刷新 · Esc 返回".into(),
        ];
        if self.loading {
            lines.push("读取中…".into());
        }
        if let Some(error) = &self.error {
            lines.push(super::overview::clean(error));
        }
        if let Some(route) = &self.route {
            lines.push("会话最新路由（可能晚于所选历史请求）".into());
            let route = &route["route"];
            if route.is_null() {
                lines.push("此会话暂无路由记录".into());
            } else {
                for (label, key) in [
                    ("请求", "asked"),
                    ("路由组", "group"),
                    ("实际模型", "model"),
                    ("上游响应模型", "served"),
                    ("推理强度", "effort"),
                ] {
                    if let Some(value) = route[key].as_str() {
                        lines.push(format!("{label}：{value}"));
                    }
                }
                if let Some(rule) = route.get("rule").filter(|v| !v.is_null()) {
                    let conditions = rule["when"]
                        .as_array()
                        .map(|values| {
                            values
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("、")
                        })
                        .unwrap_or_default();
                    lines.push(format!("规则 {}：{}", scalar(&rule["n"]), conditions));
                    if let Some(intent) = rule["classified"]["intent"].as_str() {
                        lines.push(format!("判断意图：{intent}"));
                    }
                    if rule["unready"] == true {
                        lines.push("规则目标当前不可用，按组内后备规则执行".into());
                    }
                }
                if let Some(tries) = route["tries"].as_array() {
                    for t in tries {
                        lines.push(format!(
                            "尝试：{} · HTTP {}",
                            t["model"].as_str().unwrap_or("—"),
                            t["status"]
                        ));
                    }
                }
            }
        } else {
            lines.push(format!(
                "共 {} 条 · 第 {} 页",
                self.total,
                self.offset / 20 + 1
            ));
            if self.rows.is_empty() && !self.loading {
                lines.push("尚无请求记录".into());
            }
            let capacity = usize::from(inner.height.saturating_sub(4) / 4).max(1);
            let first = self.selected.saturating_sub(capacity - 1);
            for (index, row) in self.rows.iter().enumerate().skip(first).take(capacity) {
                lines.push(format!(
                    "{} {} · {}/{} · HTTP {}",
                    if index == self.selected { "›" } else { " " },
                    row["t"].as_str().unwrap_or(""),
                    row["provider"].as_str().unwrap_or(""),
                    row["model"].as_str().unwrap_or(""),
                    row["status"]
                ));
                lines.push(format!(
                    "  请求 {} · 输入 {} / 输出 {} · 首字 {} ms · 耗时 {} ms",
                    row["req"].as_str().unwrap_or("—"),
                    row["in"],
                    row["out"],
                    scalar(&row["ttft_ms"]),
                    scalar(&row["ms"])
                ));
                lines.push(format!(
                    "  账号 {} · 会话 {}",
                    row["providerAccount"].as_str().unwrap_or("—"),
                    row["session"].as_str().unwrap_or("—")
                ));
                lines.push(if row["priced"] == true {
                    format!("  Magpie 估算费用 {} USD", scalar(&row["cost"]))
                } else {
                    "  费用：未定价".into()
                });
            }
        }
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .map(|s| super::overview::clean(&s))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .wrap(Wrap { trim: false }),
            inner,
        );
    }
}
fn scalar(value: &Value) -> String {
    if value.is_null() {
        "—".into()
    } else if let Some(s) = value.as_str() {
        s.into()
    } else {
        value.to_string()
    }
}
