//! Performance dialog state contains summaries and identity references, never log bodies.
use crate::performance::{Binding, Period, Snapshot};
use ratatui::crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Style, Stylize},
    widgets::{Block, Clear, Paragraph},
};

pub(super) const HOME_TTL: std::time::Duration = std::time::Duration::from_secs(300);
pub(super) struct CachedSummary {
    pub result: Result<Option<crate::performance::Summary>, String>,
    pub updated: std::time::Instant,
}

pub(super) fn summary_lines(entry: Option<&CachedSummary>) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::Line;
    let mut lines = vec![Line::from("性能均值 · 近 24 小时 · v 详情").dim()];
    let Some(entry) = entry else {
        lines.push(Line::from("  正在读取性能摘要…").dim());
        return lines;
    };
    match &entry.result {
        Err(error) => {
            lines.push(Line::from(format!("  {}", super::overview::clean(error))).yellow())
        }
        Ok(None) => lines.push(Line::from("  尚未生成统计").dim()),
        Ok(Some(s)) if s.calls == 0 => lines.push(Line::from("  暂无可用样本").dim()),
        Ok(Some(s)) => {
            lines.push(Line::from(format!(
                "  {} 次请求 · 平均首字 {}",
                s.calls,
                mean_ms(s.mean_ttft_ms)
            )));
            lines.push(Line::from(format!(
                "  生成速度 {}",
                s.speed()
                    .map_or_else(|| "—".into(), |v| format!("{v:.1} tok/s"))
            )));
            lines.push(Line::from(format!(
                "  速度下限 {}",
                s.floor()
                    .map_or_else(|| "—".into(), |v| format!("≥{v:.1} tok/s"))
            )));
        }
    }
    if entry.updated.elapsed() >= HOME_TTL {
        lines.push(Line::from("  数据待更新").dim());
    }
    lines
}

pub(super) enum Action {
    Stay,
    Reload,
    Rescan,
    Close,
}
pub(super) struct Panel {
    pub binding: Binding,
    pub name: String,
    pub period: Period,
    pub generation: u64,
    pub snapshot: Option<Snapshot>,
    pub loading: bool,
    pub error: Option<String>,
    pub selected: usize,
    pub detail: Option<String>,
    pub page: u32,
    pub scroll: u16,
    pub task: Option<crate::performance::LoadTask>,
    pub refreshed: std::time::Instant,
}
impl Panel {
    pub fn new(binding: Binding, name: String) -> Self {
        Self {
            binding,
            name,
            period: Period::Day,
            generation: 0,
            snapshot: None,
            loading: true,
            error: None,
            selected: 0,
            detail: None,
            page: 0,
            scroll: 0,
            task: None,
            refreshed: std::time::Instant::now(),
        }
    }
    pub fn stop(&mut self) {
        self.task = None;
    }
    pub fn key(&mut self, key: KeyCode) -> Action {
        match key {
            KeyCode::Esc if self.detail.is_some() => {
                self.detail = None;
                self.page = 0;
                self.scroll = 0;
                return Action::Reload;
            }
            KeyCode::Esc | KeyCode::Char('q') => return Action::Close,
            KeyCode::Char('r') => return Action::Rescan,
            KeyCode::Left | KeyCode::Right => {
                self.period = self.period.next(key == KeyCode::Left);
                self.page = 0;
                self.scroll = 0;
                return Action::Reload;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.detail.is_some() {
                    self.scroll = self.scroll.saturating_sub(1)
                } else {
                    self.selected = self.selected.saturating_sub(1)
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.detail.is_some() {
                    self.scroll = self.scroll.saturating_add(1).min(200)
                } else if let Some(s) = &self.snapshot {
                    self.selected = (self.selected + 1).min(s.models.len().saturating_sub(1));
                }
            }
            KeyCode::Enter if self.detail.is_none() => {
                if let Some(model) = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.models.get(self.selected))
                {
                    self.detail = Some(model.model.clone());
                    self.page = 0;
                    self.scroll = 0;
                    return Action::Reload;
                }
            }
            KeyCode::PageDown if self.detail.is_some() => {
                if self.snapshot.as_ref().is_some_and(|s| s.recent.len() == 40) {
                    self.page += 1;
                    self.scroll = 0;
                    return Action::Reload;
                }
            }
            KeyCode::PageUp if self.detail.is_some() => {
                self.page = self.page.saturating_sub(1);
                self.scroll = 0;
                return Action::Reload;
            }
            _ => {}
        }
        Action::Stay
    }
}
impl Drop for Panel {
    fn drop(&mut self) {
        self.stop();
    }
}
fn mean_ms(value: Option<f64>) -> String {
    value.map_or_else(|| "—".into(), |v| format!("{:.2}s", v / 1000.0))
}
fn rate(value: Option<(u64, i64)>, prefix: &str) -> String {
    value.map_or_else(
        || "—".into(),
        |(n, ms)| format!("{prefix}{:.1} tok/s", n as f64 * 1000.0 / ms as f64),
    )
}
fn ms(value: Option<i64>, estimated: bool) -> String {
    value.map_or_else(
        || "—".into(),
        |n| {
            format!(
                "{}{:.2}s",
                if estimated { "≈" } else { "" },
                n as f64 / 1000.0
            )
        },
    )
}
pub(super) fn draw(frame: &mut Frame, p: &Panel) {
    let area = frame.area();
    let area = Rect::new(
        area.x + area.width.saturating_sub(104) / 2,
        area.y + area.height.saturating_sub(28) / 2,
        area.width.min(104),
        area.height.min(28),
    );
    Clear.render(area, frame.buffer_mut());
    let block = Block::bordered()
        .title(format!(" {} · 性能 ", super::overview::clean(&p.name)))
        .border_style(Style::new().cyan());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut focus_line = 0;
    let mut lines = vec![
        format!(
            "最近 {} 天 · {} · {}",
            p.period.days(),
            if p.loading {
                "后台增量更新中"
            } else {
                "已读取索引"
            },
            if p.binding.gateway_observations {
                "网关与会话日志"
            } else {
                "会话日志"
            }
        ),
        if p.binding.gateway_observations {
            "网关请求优先；历史会话日志补充。生成速度与 ≥ 下限分开统计。"
        } else {
            "仅使用会话日志。生成速度与 ≥ 下限分开统计。"
        }
        .into(),
    ];
    if let Some(error) = &p.error {
        lines.push(format!("读取失败：{}", super::overview::clean(error)));
    }
    if let Some(snapshot) = &p.snapshot {
        if snapshot.native_transport {
            lines
                .push("部分启动的网关采集不完整，沿用会话日志统计；未测得的 TPS 保持未知。".into());
        }
        lines.push(format!(
            "本次读 {} KiB · 检查 {} 文件 · 未归属 {} · 跳过 {} 行{}",
            snapshot.read_bytes / 1024,
            snapshot.files_checked,
            snapshot.unmatched,
            snapshot.skipped_lines,
            if snapshot.partial {
                " · 索引未完成"
            } else {
                ""
            }
        ));
        lines.push(String::new());
        if let Some(model) = &p.detail {
            lines.push(format!(
                "{} · 请求明细 · 第 {} 页（每页 40 条）",
                super::overview::clean(model),
                p.page + 1
            ));
            if snapshot.recent.is_empty() {
                lines.push("没有更多请求".into());
            }
            for row in &snapshot.recent {
                let at = chrono::DateTime::from_timestamp_millis(row.at)
                    .map(|t| {
                        t.with_timezone(&chrono::Local)
                            .format("%m-%d %H:%M:%S")
                            .to_string()
                    })
                    .unwrap_or_default();
                let speed = rate(row.speed(), "");
                let floor = rate(row.floor(), "≥");
                lines.push(format!(
                    "{at} · 输出 {} · 首字 {}",
                    if row.output_unknown {
                        "未知".into()
                    } else {
                        row.output.to_string()
                    },
                    ms(row.ttft_ms, false)
                ));
                lines.push(format!("  生成速度 {speed} · 速度下限 {floor}"));
                lines.push(format!(
                    "  耗时 {} · {}",
                    ms(row.duration_ms, !row.native_timing && !row.gateway),
                    if row.gateway {
                        "网关 HTTP 观测"
                    } else if row.native_timing {
                        "原生 CLI 计时"
                    } else {
                        "日志时间戳估算"
                    }
                ));
            }
        } else {
            lines.push("模型 / 请求数 / 平均首字 / 生成速度 / 速度下限".into());
            if snapshot.models_truncated {
                lines.push("模型过多，仅展示前 500 个；可缩小时间范围".into());
            }
            if snapshot.models.is_empty() {
                lines.push(
                    if snapshot.partial {
                        "正在建立索引，尚无可展示样本"
                    } else {
                        "没有可归属样本；未启动、旧日志缺字段或提供方不匹配均不会伪造数据"
                    }
                    .into(),
                );
            }
            for (index, m) in snapshot.models.iter().enumerate() {
                if index == p.selected {
                    focus_line = lines.len();
                }
                lines.push(format!(
                    "{} {} · {} 次",
                    if index == p.selected { "›" } else { " " },
                    super::overview::clean(&m.model),
                    m.calls
                ));
                lines.push(format!(
                    "  平均首字 {}（{}） · 生成 {}（{}）",
                    mean_ms(m.mean_ttft_ms),
                    m.timed_samples,
                    m.speed()
                        .map_or_else(|| "—".into(), |v| format!("{v:.1} tok/s")),
                    m.speed_samples
                ));
                lines.push(format!(
                    "  速度下限 {}（{}）",
                    m.floor()
                        .map_or_else(|| "—".into(), |v| format!("≥{v:.1} tok/s")),
                    m.floor_samples
                ));
            }
        }
    } else {
        lines.push("正在读取性能索引…".into());
    }
    let footer = if p.detail.is_some() {
        "↑↓ 滚动  PgUp/PgDn 翻页  ←→ 时间范围  r 更新日志  Esc 返回"
    } else {
        "↑↓ 模型  Enter 请求明细  ←→ 时间范围  r 更新日志  Esc 关闭"
    };
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let before = super::overview::wrap(
        lines[..focus_line]
            .iter()
            .cloned()
            .map(ratatui::text::Line::from)
            .collect(),
        body.width,
    )
    .len();
    let lines = super::overview::wrap(
        lines.into_iter().map(ratatui::text::Line::from).collect(),
        body.width,
    );
    let scroll = if p.detail.is_some() {
        p.scroll
    } else {
        before
            .saturating_sub(body.height as usize / 2)
            .min(u16::MAX as usize) as u16
    };
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body);
    if inner.height > 0 {
        frame.render_widget(
            Paragraph::new(footer).dim(),
            Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
        );
    }
}
use ratatui::widgets::Widget;
