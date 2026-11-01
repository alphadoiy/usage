//! Shared onboarding for native accounts and external Magpie model connections.
mod native;
mod picker;
mod quota;
use super::forms::TextInput;
use crate::codex_manager::{Manager, Profile};
pub(super) use native::run as run_native;
pub(super) use native::{AddMethod, edit, relogin};
pub(super) use quota::edit as edit_quota;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::{future::Future, io, sync::mpsc, time::Duration};
use tokio::runtime::Handle;

pub(super) struct Added {
    pub profile: Option<Profile>,
    pub notice: String,
}
impl From<Profile> for Added {
    fn from(profile: Profile) -> Self {
        Self {
            notice: format!("已添加 Provider：{}", profile.name),
            profile: Some(profile),
        }
    }
}
struct Setup<'a> {
    terminal: &'a mut DefaultTerminal,
    background: &'a dyn Fn(&mut Frame),
}
impl Setup<'_> {
    fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        self.terminal.draw(|frame| {
            (self.background)(frame);
            render(frame);
        })?;
        Ok(())
    }
}
// Simple reusable fields; provider-specific authorization forms live in Magpie.
struct Choice {
    label: String,
    value: String,
}
struct Input {
    message: String,
    options: Vec<Choice>,
}
struct Field {
    input: Input,
    value: TextInput,
    secret: bool,
}
impl Field {
    fn text(_key: &str, label: &str, value: &str) -> Self {
        Self {
            input: Input {
                message: label.into(),
                options: Vec::new(),
            },
            value: value.into(),
            secret: false,
        }
    }
    fn display(&self) -> String {
        if self.secret {
            "•".repeat(self.value.chars().count().min(32))
        } else {
            self.input
                .options
                .iter()
                .find(|o| o.value == self.value.as_str())
                .map(|o| o.label.clone())
                .unwrap_or_else(|| self.value.to_string())
        }
    }
}
fn visible(fields: &[Field]) -> Vec<usize> {
    (0..fields.len()).collect()
}
pub(super) fn show_notice(
    terminal: &mut DefaultTerminal,
    background: &dyn Fn(&mut Frame),
    text: &str,
) -> io::Result<()> {
    notice(
        &mut Setup {
            terminal,
            background,
        },
        text,
    )
}
pub(super) fn run(
    handle: Handle,
    terminal: &mut DefaultTerminal,
    background: &dyn Fn(&mut Frame),
) -> io::Result<Option<Added>> {
    let mut setup = Setup {
        terminal,
        background,
    };
    loop {
        let rows = [
            "ChatGPT OAuth · 原生官方登录",
            "导入 OAuth",
            "BYOK · API Key",
            "从 CC Switch 导入",
        ]
        .map(str::to_owned);
        let Some(index) = pick(&mut setup, "添加 Provider › 接入方式", &rows)? else {
            return Ok(None);
        };
        let method = match index {
            0 => AddMethod::Oauth,
            1 => AddMethod::ImportOauth,
            2 => AddMethod::Byok,
            _ => AddMethod::CcSwitch,
        };
        if let Some(added) = run_native(handle.clone(), setup.terminal, setup.background, method)? {
            return Ok(Some(added));
        }
    }
}
pub(super) fn configure_magpie(
    handle: Handle,
    terminal: &mut DefaultTerminal,
    background: &dyn Fn(&mut Frame),
) -> io::Result<Option<Added>> {
    magpie(
        &handle,
        &mut Setup {
            terminal,
            background,
        },
    )
}
fn magpie(handle: &Handle, terminal: &mut Setup<'_>) -> io::Result<Option<Added>> {
    let manager = Manager::open_default().map_err(io::Error::other)?;
    let existing = crate::magpie::builtin_connection(Some(&manager)).map_err(io::Error::other)?;
    let saved = crate::magpie::default_profile(&manager)
        .map_err(io::Error::other)?
        .map(|p| manager.revision(&p.id, p.revision))
        .transpose()
        .map_err(io::Error::other)?;
    let doc = saved
        .as_ref()
        .map(|s| crate::codex_manager::config::parse(&s.config))
        .transpose()
        .map_err(io::Error::other)?;
    let effort = doc
        .as_ref()
        .and_then(|d| d.get("model_reasoning_effort")?.as_str())
        .unwrap_or_default();
    let account = doc
        .as_ref()
        .and_then(|d| {
            d.get("model_providers")?
                .get(crate::magpie::PROVIDER)?
                .get("http_headers")?
                .get("X-Magpie-Account")?
                .as_str()
        })
        .unwrap_or_default();
    let mut fields = [
        Field::text("address", "Magpie 地址", existing.address()),
        Field::text("key", "网关密钥（留空保留；本机无需填写）", ""),
    ];
    fields[1].secret = true;
    if !form(
        terminal,
        "连接 Magpie · 供应商与路由方案在 Magpie 管理",
        &mut fields,
    )? {
        return Ok(None);
    }
    let connection = match crate::magpie::Connection::new(
        fields[0].value.trim(),
        if fields[1].value.trim().is_empty() {
            crate::magpie::LOCAL_KEY
        } else {
            fields[1].value.trim()
        },
    ) {
        Ok(c) => c,
        Err(e) => {
            notice(terminal, &e)?;
            return Ok(None);
        }
    };
    let connection =
        if fields[1].value.trim().is_empty() && connection.address() == existing.address() {
            existing
        } else {
            connection
        };
    let query = connection.clone();
    let Some(models) = wait(
        terminal,
        handle,
        "读取 Magpie 模型与路由方案…",
        true,
        async move {
            let http = crate::magpie::http();
            query.hello(http.as_ref()).await?;
            query.models(http.as_ref()).await
        },
    )?
    else {
        return Ok(None);
    };
    if models.is_empty() {
        notice(
            terminal,
            "Magpie 尚无可用模型，请在 Magpie 添加供应商或路由组后重试",
        )?;
        return Ok(None);
    }
    let labels = models.iter().map(|m| m.label()).collect::<Vec<_>>();
    let Some(index) = pick(terminal, "选择路由方案或模型 · 输入文字筛选", &labels)?
    else {
        return Ok(None);
    };
    let model = models[index].clone();
    let mut fields = [
        Field::text("effort", "推理强度（可留空，由路由方案决定）", effort),
        Field::text("account", "固定账号 ID（可留空，由 Magpie 调度）", account),
    ];
    if !form(terminal, "保存内置 Magpie 配置", &mut fields)? {
        return Ok(None);
    }
    let effort = fields[0].value.to_string();
    let account = fields[1].value.to_string();
    let saved = wait(
        terminal,
        handle,
        "保存 Magpie 启动配置…",
        false,
        async move {
            crate::background::blocking(move || {
                let mut manager = Manager::open_default().map_err(|e| e.to_string())?;
                connection.configure(
                    &mut manager,
                    &model.id,
                    (!effort.trim().is_empty()).then_some(effort.trim()),
                    (!account.trim().is_empty()).then_some(account.trim()),
                )
            })
            .await
            .map_err(str::to_owned)?
        },
    )?;
    Ok(saved.map(|mut profile| {
        profile.id = crate::magpie::BUILTIN_ID.into();
        Added {
            profile: Some(profile),
            notice: "已保存内置 Magpie 配置".into(),
        }
    }))
}
fn input_event() -> io::Result<Option<Event>> {
    if !event::poll(Duration::from_millis(80))? {
        return Ok(None);
    }
    Ok(match event::read()? {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            Some(Event::Key(key))
        }
        Event::Paste(text) => Some(Event::Paste(text)),
        _ => None,
    })
}
fn key() -> io::Result<Option<KeyEvent>> {
    Ok(match input_event()? {
        Some(Event::Key(key)) => Some(key),
        _ => None,
    })
}
fn cancelled(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
}
fn wait<T: Send + 'static>(
    terminal: &mut Setup<'_>,
    handle: &Handle,
    title: &str,
    cancellable: bool,
    future: impl Future<Output = Result<T, String>> + Send + 'static,
) -> io::Result<Option<T>> {
    let (tx, rx) = mpsc::sync_channel(1);
    let _pending = if cancellable {
        Some(crate::background::Task::query(
            handle,
            future,
            move |result| {
                let _ = tx.send(result);
            },
        ))
    } else {
        // An accepted save/install must finish even if terminal rendering fails.
        handle.spawn(async move {
            let _ = tx.send(future.await);
        });
        None
    };
    loop {
        match rx.try_recv() {
            Ok(Ok(value)) => return Ok(Some(value)),
            Ok(Err(error)) => {
                notice(terminal, &error)?;
                return Ok(None);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                notice(terminal, "任务异常结束")?;
                return Ok(None);
            }
            _ => {}
        }
        terminal.draw(|frame| {
            page(
                frame,
                title,
                Vec::new(),
                None,
                if cancellable {
                    "Esc 返回"
                } else {
                    "正在保存，请稍候…"
                },
            )
        })?;
        if let Some(key) = key()?
            && cancelled(key)
            && cancellable
        {
            return Ok(None);
        }
    }
}
fn notice(terminal: &mut Setup<'_>, text: &str) -> io::Result<()> {
    loop {
        terminal.draw(|frame| {
            page(
                frame,
                "添加 Provider",
                vec![text.into()],
                None,
                "Enter / Esc 返回",
            )
        })?;
        if let Some(key) = key()?
            && (cancelled(key) || key.code == KeyCode::Enter)
        {
            return Ok(());
        }
    }
}
#[derive(Default)]
struct Picker {
    query: TextInput,
    selected: usize,
}
impl Picker {
    fn indices(&self, rows: &[String]) -> Vec<usize> {
        let query = self.query.to_lowercase();
        rows.iter()
            .enumerate()
            .filter(|(_, r)| r.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect()
    }
    fn key(&mut self, key: KeyEvent, count: usize) {
        match key.code {
            KeyCode::Up | KeyCode::BackTab => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1))
            }
            _ => {
                if self.query.key(key) {
                    self.selected = 0;
                }
            }
        }
    }
}
#[derive(Clone)]
struct MenuItem {
    label: String,
    description: Option<String>,
    enabled: bool,
}
impl From<String> for MenuItem {
    fn from(label: String) -> Self {
        Self {
            label,
            description: None,
            enabled: true,
        }
    }
}
fn pick(terminal: &mut Setup<'_>, title: &str, rows: &[String]) -> io::Result<Option<usize>> {
    let options: Vec<_> = rows.iter().cloned().map(MenuItem::from).collect();
    pick_options(terminal, title, &options)
}
fn selected_option(rows: &[MenuItem], indices: &[usize], selected: usize) -> Option<usize> {
    indices.get(selected).copied().filter(|&i| rows[i].enabled)
}
fn pick_options(
    terminal: &mut Setup<'_>,
    title: &str,
    rows: &[MenuItem],
) -> io::Result<Option<usize>> {
    if rows.is_empty() {
        notice(terminal, "没有可选项；可返回重试或选择其他账号")?;
        return Ok(None);
    }
    let labels: Vec<_> = rows.iter().map(|r| r.label.clone()).collect();
    let mut picker = Picker {
        selected: rows.iter().position(|r| r.enabled).unwrap_or(0),
        ..Picker::default()
    };
    loop {
        let indices = picker.indices(&labels);
        let visible = indices.iter().map(|&i| rows[i].clone()).collect();
        terminal.draw(|frame| {
            menu_page(
                frame,
                title,
                visible,
                Some(picker.selected),
                "↑↓ 选择 · Enter 进入 · Esc 上一级",
                Some(&picker.query),
            )
        })?;
        let Some(input_event) = input_event()? else {
            continue;
        };
        if let Event::Paste(text) = &input_event {
            if picker.query.paste(text) {
                picker.selected = 0;
            }
            continue;
        }
        if let Event::Key(key) = input_event {
            if cancelled(key) {
                return Ok(None);
            }
            if key.code == KeyCode::Enter
                && let Some(index) = selected_option(rows, &indices, picker.selected)
            {
                return Ok(Some(index));
            }
            picker.key(key, indices.len());
        }
    }
}
fn form(terminal: &mut Setup<'_>, title: &str, fields: &mut [Field]) -> io::Result<bool> {
    let mut selected = 0;
    loop {
        let indices = visible(fields);
        selected = selected.min(indices.len().saturating_sub(1));
        terminal.draw(|frame| {
            let area = popup_area(
                frame.area(),
                (indices.len() as u16).saturating_mul(2).saturating_add(5),
            );
            frame.render_widget(Clear, area);
            let block = Block::bordered().title(title);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let capacity = usize::from(inner.height.saturating_sub(2) / 2).max(1);
            let first = selected.saturating_sub(capacity - 1);
            for (row, &index) in indices.iter().skip(first).take(capacity).enumerate() {
                let field = &fields[index];
                let y = inner.y + row as u16 * 2;
                if y >= inner.bottom() {
                    break;
                }
                let rect = Rect::new(inner.x, y, inner.width, 1);
                if field.input.options.is_empty() {
                    super::forms::input_row(
                        frame,
                        rect,
                        &field.input.message,
                        &field.value,
                        field.secret,
                        first + row == selected,
                    );
                } else {
                    frame.render_widget(
                        Paragraph::new(format!(
                            "{} {}：{}",
                            if first + row == selected { "›" } else { " " },
                            field.input.message,
                            field.display()
                        )),
                        rect,
                    );
                }
            }
            if inner.height > 1 {
                frame.render_widget(
                    Paragraph::new("Tab / ↑↓ 换字段 · ←→ 编辑/选项 · Enter 确认 · Esc 返回"),
                    Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
                );
            }
        })?;
        let Some(input_event) = input_event()? else {
            continue;
        };
        if let Event::Paste(text) = &input_event {
            if let Some(&index) = indices.get(selected)
                && fields[index].input.options.is_empty()
            {
                fields[index].value.paste(text);
            }
            continue;
        }
        if let Event::Key(key) = input_event {
            if cancelled(key) {
                return Ok(false);
            }
            if key.code == KeyCode::Enter {
                return Ok(true);
            }
            match key.code {
                KeyCode::Up | KeyCode::BackTab => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => {
                    selected = (selected + 1).min(indices.len().saturating_sub(1))
                }
                _ => {
                    if let Some(&index) = indices.get(selected) {
                        let field = &mut fields[index];
                        if field.input.options.is_empty() {
                            field.value.key(key);
                        } else if matches!(key.code, KeyCode::Left | KeyCode::Right) {
                            let n = field.input.options.len();
                            let at = field
                                .input
                                .options
                                .iter()
                                .position(|o| o.value == field.value.as_str())
                                .unwrap_or(0);
                            let next = if key.code == KeyCode::Left {
                                (at + n - 1) % n
                            } else {
                                (at + 1) % n
                            };
                            field.value = field.input.options[next].value.clone().into();
                        }
                    }
                }
            }
        }
    }
}
fn page(frame: &mut Frame, title: &str, rows: Vec<String>, selected: Option<usize>, hint: &str) {
    menu_page(
        frame,
        title,
        rows.into_iter().map(MenuItem::from).collect(),
        selected,
        hint,
        None,
    );
}
fn popup_area(area: Rect, height: u16) -> Rect {
    let width = area.width.saturating_sub(4).min(80);
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}
fn menu_page(
    frame: &mut Frame,
    title: &str,
    rows: Vec<MenuItem>,
    selected: Option<usize>,
    hint: &str,
    editor: Option<&TextInput>,
) {
    let area = popup_area(frame.area(), u16::MAX);
    let width = area.width.saturating_sub(4).max(1);
    let items: Vec<_> = rows
        .into_iter()
        .map(|row| {
            let style = if row.enabled {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let mut lines =
                super::overview::wrap(vec![Line::from(super::overview::clean(&row.label))], width);
            if let Some(description) = row.description {
                lines.extend(super::overview::wrap(
                    vec![
                        Line::from(super::overview::clean(&description))
                            .style(Style::default().fg(Color::Gray)),
                    ],
                    width,
                ));
            }
            ListItem::new(lines).style(style)
        })
        .collect();
    let content_height: usize = items.iter().map(ListItem::height).sum();
    let hints = super::overview::wrap(
        vec![Line::from(super::overview::clean(hint))],
        area.width.saturating_sub(2).max(1),
    );
    let hint_height = hints.len().min(3) as u16 + u16::from(editor.is_some());
    // Bound long model lists while sizing short submenus to their actual content.
    let area = popup_area(
        frame.area(),
        (content_height.clamp(1, 12) as u16) + hint_height + 3,
    );
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(format!(" {} ", super::overview::clean(title)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [body, _, footer] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(hint_height),
    ])
    .areas(inner);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::new().fg(Color::LightBlue)),
        body,
        &mut ListState::default().with_selected(selected),
    );
    let footer = if let Some(editor) = editor {
        if !footer.is_empty() {
            super::forms::input_row(
                frame,
                Rect::new(footer.x, footer.y, footer.width, 1),
                "筛选",
                editor,
                false,
                true,
            );
        }
        Rect::new(
            footer.x,
            footer.y.saturating_add(1),
            footer.width,
            footer.height.saturating_sub(1),
        )
    } else {
        footer
    };
    frame.render_widget(
        Paragraph::new(hints)
            .style(Style::new().fg(Color::Gray))
            .wrap(Wrap { trim: false }),
        footer,
    );
}
