//! Codex profile management; the same Manager powers CLI, TUI, and session restoration.
use crate::codex_manager::{self, AuthMode, Manager, Profile, config, oauth};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Widget, Wrap};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

struct Form {
    fields: Vec<String>,
    selected: usize,
    editing: Option<(String, i64, String)>,
    auth_mode: AuthMode,
}
enum Prompt {
    AddType,
    OAuthName(String),
    LoginMethod(String, Option<String>),
    AuthSource(String),
    AuthImport(PathBuf, oauth::Identity),
    Source(String),
    Directory(String),
    Import(PathBuf, String),
    Remove(String),
}

pub fn run(handle: Handle) -> io::Result<()> {
    let mut manager = Manager::open_default().map_err(io::Error::other)?;
    let mut profiles = manager.list().map_err(io::Error::other)?;
    let mut selected = 0usize;
    let mut cwd = std::env::current_dir()?;
    let mut form: Option<Form> = None;
    let mut prompt: Option<Prompt> = None;
    let mut notice = "账号配置独立保存 · 修改后生成新版本 · Enter 打开 Codex".to_string();
    let mut refreshed = Instant::now();
    let mut terminal = ratatui::try_init().map_err(io::Error::other)?;
    let result = (|| -> io::Result<()> {
        loop {
            terminal.draw(|frame| {
                let [header, body, footer] = Layout::vertical([Constraint::Length(1),Constraint::Fill(1),Constraint::Length(3)]).areas(frame.area());
                Paragraph::new(Line::from(vec![" Codex 账号配置 ".bold(), format!("项目：{}",cwd.display()).dim()])).render(header,frame.buffer_mut());
                let items: Vec<ListItem> = profiles.iter().map(|p| ListItem::new(vec![
                    Line::from(format!("{}  ·  {}  ·  v{}",p.name,p.model,p.revision)),
                    Line::from(format!("{}  ·  {}",p.base_url,p.oauth.as_ref().map(|o| o.label()).unwrap_or_else(|| if p.follow {"BYOK · 跟随 CC Switch".into()} else {"BYOK · 独立配置".into()}))).dim(),
                    Line::from(format!("{} {}",p.id,p.source_status)).dim(),
                ])).collect();
                if items.is_empty() {
                    Paragraph::new("还没有保存账号配置。\n\na 添加账号（BYOK / OAuth）\nl 重新登录选中的 OAuth 账号\ni 从 CC Switch 导入（先预览再保存）\no 选择项目目录")
                        .block(Block::bordered()).render(body,frame.buffer_mut());
                } else {
                    let mut state = ListState::default().with_selected(Some(selected));
                    frame.render_stateful_widget(List::new(items).block(Block::bordered()).highlight_style(Style::new().bg(Color::Rgb(35,45,65)))
                        .highlight_symbol("› "),body,&mut state);
                }
                Paragraph::new(format!("a 添加  e 编辑  E TOML  l 登录  i CC导入  Enter 打开  Esc 返回\nc 复制  d 移除  s 同步  f 跟随  o 项目\n{notice}"))
                    .style(Style::new().fg(Color::Rgb(142,142,147))).render(footer,frame.buffer_mut());
                if let Some(form) = &form { draw_form(frame,form); }
                if let Some(prompt) = &prompt { draw_prompt(frame,prompt); }
            })?;
            if !event::poll(Duration::from_millis(100))? {
                if refreshed.elapsed() >= Duration::from_secs(15)
                    && form.is_none()
                    && prompt.is_none()
                {
                    // Background import is unnecessary: committed source reads have a bounded busy timeout.
                    match manager.sync(false) {
                        Ok(report)
                            if !report.updated.is_empty()
                                || !report.skipped.is_empty()
                                || !report.missing.is_empty() =>
                        {
                            notice = report.to_string()
                        }
                        Err(e) => notice = e.to_string(),
                        _ => {}
                    }
                    reload(&manager, &mut profiles, &mut selected, &mut notice);
                    refreshed = Instant::now();
                }
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                return Ok(());
            }
            if let Some(edit) = &mut form {
                match key.code {
                    KeyCode::Esc => form = None,
                    KeyCode::Tab | KeyCode::Down => {
                        edit.selected = (edit.selected + 1) % edit.fields.len()
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        edit.selected = (edit.selected + edit.fields.len() - 1) % edit.fields.len()
                    }
                    KeyCode::Backspace => {
                        edit.fields[edit.selected].pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        edit.fields[edit.selected].clear()
                    }
                    KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        edit.fields[edit.selected].push(ch)
                    }
                    KeyCode::Enter => match save_form(&mut manager, edit) {
                        Ok(p) => {
                            notice = format!("已保存 {} v{}", p.name, p.revision);
                            form = None;
                            reload(&manager, &mut profiles, &mut selected, &mut notice);
                            selected = profiles.iter().position(|x| x.id == p.id).unwrap_or(0);
                        }
                        Err(e) => notice = e.to_string(),
                    },
                    _ => {}
                }
                continue;
            }
            if let Some(current) = prompt.take() {
                let directory = matches!(&current, Prompt::Directory(_));
                match current {
                    Prompt::AddType => match key.code {
                        KeyCode::Esc => {}
                        KeyCode::Char('b') | KeyCode::Char('1') => form = Some(byok_form()),
                        KeyCode::Char('o') | KeyCode::Char('2') => {
                            prompt = Some(Prompt::OAuthName(String::new()))
                        }
                        KeyCode::Char('i') | KeyCode::Char('3') => {
                            prompt = Some(Prompt::AuthSource(
                                crate::config::home_dir()
                                    .join(".codex/auth.json")
                                    .to_string_lossy()
                                    .into(),
                            ))
                        }
                        _ => prompt = Some(Prompt::AddType),
                    },
                    Prompt::OAuthName(mut name) => match key.code {
                        KeyCode::Esc => {}
                        KeyCode::Enter if !name.trim().is_empty() => {
                            prompt = Some(Prompt::LoginMethod(name, None))
                        }
                        code => {
                            if code == KeyCode::Char('u')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                name.clear();
                            } else if !key.modifiers.contains(KeyModifiers::CONTROL) {
                                edit_input(&mut name, code);
                            }
                            prompt = Some(Prompt::OAuthName(name));
                        }
                    },
                    Prompt::LoginMethod(name, account) => {
                        if [
                            KeyCode::Enter,
                            KeyCode::Char('1'),
                            KeyCode::Char('b'),
                            KeyCode::Char('2'),
                            KeyCode::Char('d'),
                        ]
                        .contains(&key.code)
                        {
                            let device =
                                [KeyCode::Char('2'), KeyCode::Char('d')].contains(&key.code);
                            ratatui::restore();
                            let result = {
                                let _runtime = handle.enter();
                                #[cfg(unix)]
                                let _interrupts = tokio::signal::unix::signal(
                                    tokio::signal::unix::SignalKind::interrupt(),
                                )?;
                                manager.login_oauth(
                                    Some(&name),
                                    None,
                                    device,
                                    std::path::Path::new("codex"),
                                    account.as_deref(),
                                )
                            };
                            terminal = ratatui::try_init().map_err(io::Error::other)?;
                            notice = result
                                .map(|p| format!("已保存 OAuth：{}", p.name))
                                .unwrap_or_else(|e| e.to_string());
                            reload(&manager, &mut profiles, &mut selected, &mut notice);
                        } else if key.code != KeyCode::Esc {
                            prompt = Some(Prompt::LoginMethod(name, account));
                        }
                    }
                    Prompt::AuthSource(mut text) => match key.code {
                        KeyCode::Esc => {}
                        KeyCode::Enter => {
                            let path = expand_path(&text);
                            match oauth::read_auth(&path) {
                                Ok(auth) => prompt = Some(Prompt::AuthImport(path, auth.identity)),
                                Err(e) => {
                                    notice = e.to_string();
                                    prompt = Some(Prompt::AuthSource(text));
                                }
                            }
                        }
                        code => {
                            if code == KeyCode::Char('u')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                text.clear();
                            } else if !key.modifiers.contains(KeyModifiers::CONTROL) {
                                edit_input(&mut text, code);
                            }
                            prompt = Some(Prompt::AuthSource(text));
                        }
                    },
                    Prompt::AuthImport(path, identity) => {
                        if key.code == KeyCode::Enter {
                            notice = manager
                                .import_oauth(None, None, &path, None)
                                .map(|p| format!("已导入 OAuth：{}", p.name))
                                .unwrap_or_else(|e| e.to_string());
                            reload(&manager, &mut profiles, &mut selected, &mut notice);
                        } else if key.code != KeyCode::Esc {
                            prompt = Some(Prompt::AuthImport(path, identity));
                        }
                    }
                    Prompt::Source(mut text) | Prompt::Directory(mut text) => match key.code {
                        KeyCode::Esc => {}
                        KeyCode::Enter if directory => {
                            let path = expand_path(&text);
                            match path.canonicalize() {
                                Ok(p) if p.is_dir() => cwd = p,
                                _ => notice = "项目目录不存在".into(),
                            }
                        }
                        KeyCode::Enter => {
                            let path = expand_path(&text);
                            match manager.import_ccswitch(&path, false, &[], true) {
                                Ok(report) => {
                                    prompt = Some(Prompt::Import(path, report.to_string()))
                                }
                                Err(e) => notice = e.to_string(),
                            }
                        }
                        code => {
                            if code == KeyCode::Char('u')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                text.clear();
                            } else if !key.modifiers.contains(KeyModifiers::CONTROL) {
                                edit_input(&mut text, code);
                            }
                            prompt = Some(if directory {
                                Prompt::Directory(text)
                            } else {
                                Prompt::Source(text)
                            });
                        }
                    },
                    Prompt::Import(path, preview) => {
                        if key.code == KeyCode::Enter || key.code == KeyCode::Char('f') {
                            match manager.import_ccswitch(
                                &path,
                                key.code == KeyCode::Char('f'),
                                &[],
                                false,
                            ) {
                                Ok(report) => notice = report.to_string(),
                                Err(e) => notice = e.to_string(),
                            }
                            reload(&manager, &mut profiles, &mut selected, &mut notice);
                        } else if key.code != KeyCode::Esc {
                            prompt = Some(Prompt::Import(path, preview));
                        }
                    }
                    Prompt::Remove(id) => {
                        if key.code == KeyCode::Char('y') {
                            notice = manager
                                .remove(&id)
                                .map(|_| "已移出列表，历史和版本已保留".into())
                                .unwrap_or_else(|e| e.to_string());
                            reload(&manager, &mut profiles, &mut selected, &mut notice);
                        } else if key.code != KeyCode::Esc && key.code != KeyCode::Char('n') {
                            prompt = Some(Prompt::Remove(id));
                        }
                    }
                }
                continue;
            }
            let profile = profiles.get(selected).cloned();
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(profiles.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Char('a') => prompt = Some(Prompt::AddType),
                KeyCode::Char('l') => {
                    if let Some(p) = profile {
                        if p.auth_mode == AuthMode::Oauth {
                            prompt = Some(Prompt::LoginMethod(p.name, Some(p.id)));
                        } else {
                            notice = "BYOK 账号无需 OAuth 登录；按 a 添加 OAuth 账号".into();
                        }
                    }
                }
                KeyCode::Char('e') => {
                    if let Some(p) = profile {
                        match manager.revision(&p.id, p.revision) {
                            Ok(rev) => {
                                let effort = config::parse(&rev.config)
                                    .ok()
                                    .and_then(|d| {
                                        d.get("model_reasoning_effort")
                                            .and_then(toml_edit::Item::as_str)
                                            .map(str::to_string)
                                    })
                                    .unwrap_or_default();
                                form = Some(Form {
                                    fields: if p.auth_mode == AuthMode::Oauth {
                                        let model = config::parse(&rev.config)
                                            .ok()
                                            .and_then(|d| {
                                                d.get("model")
                                                    .and_then(toml_edit::Item::as_str)
                                                    .map(str::to_owned)
                                            })
                                            .unwrap_or_default();
                                        vec![p.name, model, effort]
                                    } else {
                                        vec![p.name, p.base_url, p.model, effort, String::new()]
                                    },
                                    auth_mode: p.auth_mode,
                                    selected: 0,
                                    editing: Some((p.id, p.revision, rev.config)),
                                });
                            }
                            Err(e) => notice = e.to_string(),
                        }
                    }
                }
                KeyCode::Char('E') => {
                    if let Some(p) = profile {
                        match manager.revision(&p.id, p.revision) {
                            Ok(rev) => {
                                ratatui::restore();
                                let edited = codex_manager::cli::edit_text(&rev.config);
                                terminal = ratatui::try_init().map_err(io::Error::other)?;
                                notice = edited
                                    .and_then(|text| {
                                        manager.update(&p.id, p.revision, &text, None, None)
                                    })
                                    .map(|p| format!("已保存 {} v{}", p.name, p.revision))
                                    .unwrap_or_else(|e| e.to_string());
                                reload(&manager, &mut profiles, &mut selected, &mut notice);
                            }
                            Err(e) => notice = e.to_string(),
                        }
                    }
                }
                KeyCode::Char('c') => {
                    if let Some(p) = profile {
                        notice = manager
                            .copy(&p.id, &format!("{} 副本", p.name))
                            .map(|p| format!("已复制：{}", p.name))
                            .unwrap_or_else(|e| e.to_string());
                        reload(&manager, &mut profiles, &mut selected, &mut notice);
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(p) = profile {
                        prompt = Some(Prompt::Remove(p.id));
                    }
                }
                KeyCode::Char('i') => {
                    prompt = Some(Prompt::Source(
                        codex_manager::ccswitch::default_path()
                            .to_string_lossy()
                            .into(),
                    ))
                }
                KeyCode::Char('o') => {
                    prompt = Some(Prompt::Directory(cwd.to_string_lossy().into()))
                }
                KeyCode::Char('f') => {
                    if let Some(p) = profile {
                        notice = manager
                            .follow(&p.id, !p.follow)
                            .map(|_| {
                                if p.follow {
                                    "已暂停跟随".into()
                                } else {
                                    "已开启跟随".into()
                                }
                            })
                            .unwrap_or_else(|e| e.to_string());
                        reload(&manager, &mut profiles, &mut selected, &mut notice);
                    }
                }
                KeyCode::Char('s') => {
                    notice = manager
                        .sync(false)
                        .map(|r| r.to_string())
                        .unwrap_or_else(|e| e.to_string());
                    reload(&manager, &mut profiles, &mut selected, &mut notice);
                    refreshed = Instant::now();
                }
                KeyCode::Char('r') => reload(&manager, &mut profiles, &mut selected, &mut notice),
                KeyCode::Enter => {
                    if let Some(p) = profile {
                        if crate::sessions::uses_ghostty(crate::config::ResumeMode::Auto) {
                            notice = codex_manager::launch_shell_command(&p.id, &cwd, None, None)
                                .and_then(|command| {
                                    crate::sessions::command_in_ghostty(&cwd, &command)
                                        .map_err(codex_manager::Error::message)
                                })
                                .map(|_| "已在右侧打开 Codex；可继续选择其他账号".into())
                                .unwrap_or_else(|e| e.to_string());
                        } else {
                            ratatui::restore();
                            let status = {
                                let _runtime = handle.enter();
                                #[cfg(unix)]
                                let _interrupts = tokio::signal::unix::signal(
                                    tokio::signal::unix::SignalKind::interrupt(),
                                )?;
                                crate::hub::spawn_managed_codex(
                                    manager.root(),
                                    &p.id,
                                    None,
                                    &crate::agents::SpawnRequest {
                                        cwd: cwd.clone(),
                                        program: "codex".into(),
                                        args: Vec::new(),
                                        resume: None,
                                    },
                                )
                                .map(|outcome| outcome.status)
                            };
                            terminal = ratatui::try_init().map_err(io::Error::other)?;
                            notice = status
                                .map(|s| format!("Codex 已退出：{s}"))
                                .unwrap_or_else(|e| e.to_string());
                        }
                        reload(&manager, &mut profiles, &mut selected, &mut notice);
                    }
                }
                _ => {}
            }
        }
    })();
    ratatui::restore();
    result
}

fn reload(
    manager: &Manager,
    profiles: &mut Vec<Profile>,
    selected: &mut usize,
    notice: &mut String,
) {
    let id = profiles.get(*selected).map(|p| p.id.clone());
    match manager.list() {
        Ok(new) => {
            *profiles = new;
            *selected = id
                .and_then(|id| profiles.iter().position(|p| p.id == id))
                .unwrap_or((*selected).min(profiles.len().saturating_sub(1)));
        }
        Err(e) => *notice = e.to_string(),
    }
}
fn save_form(manager: &mut Manager, form: &Form) -> codex_manager::Result<Profile> {
    let fields = &form.fields;
    if form.auth_mode == AuthMode::Oauth {
        let (id, revision, original) = form
            .editing
            .as_ref()
            .ok_or_else(|| codex_manager::Error::message("OAuth 请通过登录添加"))?;
        let text = oauth::patch_config(original, Some(&fields[1]), Some(&fields[2]))?;
        return manager.update(id, *revision, &text, None, Some(&fields[0]));
    }
    let effort = (!fields[3].is_empty()).then_some(fields[3].as_str());
    if let Some((id, revision, original)) = &form.editing {
        let text = config::patch(original, Some(&fields[1]), Some(&fields[2]), effort)?;
        manager.update(
            id,
            *revision,
            &text,
            (!fields[4].is_empty()).then_some(fields[4].as_str()),
            Some(&fields[0]),
        )
    } else {
        manager.add(
            &fields[0],
            &config::new_config(&fields[1], &fields[2], effort)?,
            &fields[4],
        )
    }
}
fn byok_form() -> Form {
    Form {
        fields: vec![
            String::new(),
            "https://api.openai.com/v1".into(),
            String::new(),
            "high".into(),
            String::new(),
        ],
        selected: 0,
        editing: None,
        auth_mode: AuthMode::Byok,
    }
}
fn expand_path(text: &str) -> PathBuf {
    if text == "~" {
        crate::config::home_dir()
    } else if let Some(rest) = text.strip_prefix("~/") {
        crate::config::home_dir().join(rest)
    } else {
        text.into()
    }
}
fn edit_input(text: &mut String, key: KeyCode) {
    match key {
        KeyCode::Char(c) => text.push(c),
        KeyCode::Backspace => {
            text.pop();
        }
        _ => {}
    }
}
fn modal(area: Rect, height: u16) -> Rect {
    let w = area.width.min(86);
    let h = area.height.min(height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}
fn draw_form(frame: &mut ratatui::Frame, form: &Form) {
    let area = modal(frame.area(), 15);
    frame.render_widget(Clear, area);
    let mut lines = vec![
        Line::from(if form.auth_mode == AuthMode::Oauth {
            "OAuth：模型留空使用默认；保存配置不会复制登录凭据"
        } else if form.editing.is_some() {
            "保存后生成新版本；旧会话保留原版本"
        } else {
            "添加 Codex BYOK 账号"
        }),
        Line::from(""),
    ];
    let labels: &[&str] = if form.auth_mode == AuthMode::Oauth {
        &["名称", "模型（可留空）", "推理强度（可留空）"]
    } else {
        &["名称", "Base URL", "模型", "推理强度", "API Key"]
    };
    for (index, label) in labels.iter().enumerate() {
        let text = if index == 4 {
            if form.fields[index].is_empty() && form.editing.is_some() {
                "（留空保留原密钥）".into()
            } else {
                "•".repeat(form.fields[index].chars().count().min(32))
            }
        } else {
            form.fields[index].clone()
        };
        let line = Line::from(format!(
            "{} {label}: {text}",
            if index == form.selected { "›" } else { " " }
        ));
        lines.push(if index == form.selected {
            line.fg(Color::LightBlue)
        } else {
            line
        });
        lines.push(Line::from(""));
    }
    lines.push(Line::from(
        "Tab / ↑↓ 换字段 · Ctrl+U 清空 · Enter 保存 · Esc 取消",
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered())
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_prompt(frame: &mut ratatui::Frame, prompt: &Prompt) {
    let (title, text) = match prompt {
        Prompt::AddType => ("添加 Codex 账号", "1 / b  BYOK · API Key 和自定义 API\n\n2 / o  OAuth · 登录 ChatGPT\n\n3 / i  导入 OAuth · auth.json / Cockpit 单账号文件\n\nEsc 返回".into()),
        Prompt::OAuthName(name) => ("OAuth 账号名称",format!("{name}\n\nEnter 选择登录方式 · Ctrl+U 清空 · Esc 取消")),
        Prompt::LoginMethod(name,_) => ("ChatGPT 登录方式",format!("账号：{name}\n\n1 / b / Enter  浏览器登录\n2 / d  设备码登录（需账号启用设备授权）\n\n凭据保存在独立账号目录，Codex 自动刷新。\n同账号可多开，共用登录；运行中不能替换登录。\nEsc 取消")),
        Prompt::AuthSource(path) => ("OAuth 登录文件",format!("{path}\n\n支持官方 auth.json 或 Cockpit 单账号 JSON。\n只读来源，导入后独立保存。\nEnter 预览 · Ctrl+U 清空 · Esc 取消")),
        Prompt::AuthImport(_,identity) => ("OAuth 导入预览",format!("{}\n账号：{}\n\n包含可刷新登录凭据；本地文件权限 600。\n重复身份更新原账号，运行中不能更新。\nEnter 导入 · Esc 取消",identity.label(),identity.account_id)),
        Prompt::Source(path) => (
            "CC Switch 数据库",
            format!("{path}\n\nEnter 预览 · Esc 取消"),
        ),
        Prompt::Directory(path) => ("项目目录", format!("{path}\n\nEnter 确认 · Esc 取消")),
        Prompt::Import(_, preview) => (
            "导入预览",
            format!("{preview}\nEnter 一次性导入 · f 导入并跟随 · Esc 取消"),
        ),
        Prompt::Remove(_) => (
            "移出列表",
            "配置版本及历史仍会保留。\n\ny 确认 · n / Esc 取消".into(),
        ),
    };
    let area = modal(frame.area(), 18);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn oauth_options_and_identity_preview_are_visible_without_tokens() {
        let identity = oauth::Identity {
            email: "fixture@example.test".into(),
            account_id: "fixture-workspace".into(),
            user_id: "fixture-user".into(),
            organization_id: None,
            plan: Some("plus".into()),
        };
        for prompt in [
            Prompt::AddType,
            Prompt::LoginMethod("Work".into(), None),
            Prompt::AuthImport("fixture.json".into(), identity),
        ] {
            for (width, height) in [(100, 30), (40, 20)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| draw_prompt(frame, &prompt)).unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                match &prompt {
                    Prompt::AddType => assert!(text.contains("OAuth")),
                    Prompt::LoginMethod(_, _) => assert!(
                        text.chars()
                            .filter(|c| !c.is_whitespace())
                            .collect::<String>()
                            .contains("浏览器登录")
                    ),
                    Prompt::AuthImport(_, _) => assert!(text.contains("fixture@example.test")),
                    _ => unreachable!(),
                }
                assert!(!text.contains("refresh_token"));
            }
        }
    }

    #[test]
    fn key_field_is_masked_on_wide_and_narrow_terminals() {
        let form = Form {
            fields: vec![
                "A".into(),
                "https://example.com/v1".into(),
                "m".into(),
                "high".into(),
                "fixture-ui-secret".into(),
            ],
            selected: 4,
            editing: None,
            auth_mode: AuthMode::Byok,
        };
        for (width, height) in [(100, 30), (40, 18), (20, 10)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw_form(frame, &form)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(!text.contains("fixture-ui-secret"));
            if width == 100 {
                assert!(text.contains("••••"));
            }
        }
    }
}
