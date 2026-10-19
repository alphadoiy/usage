//! Provider management. Accounts and Agent configuration retain their existing stores.
use super::forms::{edit_input, expand_path, modal};
use crate::codex_manager::{self, AuthMode, Manager, Profile};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Widget, Wrap};
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

enum Prompt {
    Directory(String),
    Remove(String),
}
fn subscription_account(profile: &Profile) -> bool {
    crate::plugins::endpoint_account(&profile.base_url).is_some()
}
fn identity(profile: &Profile) -> String {
    if subscription_account(profile) {
        "订阅服务 · 共享插件账号".into()
    } else if let Some(oauth) = &profile.oauth {
        format!("ChatGPT OAuth · {}", oauth.label())
    } else {
        format!(
            "{} · BYOK · {}",
            profile.base_url,
            if profile.follow {
                "跟随 CC Switch"
            } else {
                "独立配置"
            }
        )
    }
}
fn actions(profile: Option<&Profile>) -> String {
    let mut text = "a / F5 添加 · i CC 导入 · o 项目 · Esc 返回".to_string();
    if let Some(profile) = profile {
        text.push_str("\ne Agent 配置 · E TOML · c 复制 · d 移除 · Enter 启动 Codex");
        if profile.auth_mode == AuthMode::Oauth {
            text.push_str(" · l 重新登录");
        }
        if profile.source.is_some() {
            text.push_str(" · f 跟随 · s 同步");
        }
    }
    text
}
fn draw_accounts(
    frame: &mut ratatui::Frame,
    profiles: &[Profile],
    selected: usize,
    cwd: &std::path::Path,
    notice: &str,
) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(3),
    ])
    .areas(frame.area());
    Paragraph::new(Line::from(vec![
        " Provider 管理 ".bold(),
        format!("项目：{}", cwd.display()).dim(),
    ]))
    .render(header, frame.buffer_mut());
    let items: Vec<ListItem> = profiles
        .iter()
        .map(|p| {
            ListItem::new(vec![
                Line::from(format!("{}  ·  {}  ·  v{}", p.name, p.model, p.revision)),
                Line::from(identity(p)).dim(),
                Line::from(format!("{} {}", p.id, p.source_status)).dim(),
            ])
        })
        .collect();
    if items.is_empty() {
        Paragraph::new("还没有配置 Provider。\n\na / F5 添加 Provider（BYOK / OAuth / WorkBuddy）\ni 从 CC Switch 导入（先预览再保存）\no 选择项目目录")
                        .block(Block::bordered()).render(body,frame.buffer_mut());
    } else {
        let mut state = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(
            List::new(items)
                .block(Block::bordered())
                .highlight_style(Style::new().bg(Color::Rgb(35, 45, 65)))
                .highlight_symbol("› "),
            body,
            &mut state,
        );
    }
    Paragraph::new(format!("{}\n{notice}", actions(profiles.get(selected))))
        .style(Style::new().fg(Color::Rgb(142, 142, 147)))
        .wrap(Wrap { trim: false })
        .render(footer, frame.buffer_mut());
}

pub fn run_with_config(handle: Handle, usage_config: Option<PathBuf>) -> io::Result<()> {
    let mut manager = Manager::open_default().map_err(io::Error::other)?;
    let mut profiles = manager.list().map_err(io::Error::other)?;
    let mut selected = 0usize;
    let mut cwd = std::env::current_dir()?;
    let mut prompt: Option<Prompt> = None;
    let mut notice = "选择 Provider · e 配置 Agent · Enter 启动 Codex".to_string();
    let mut refreshed = Instant::now();
    let mut terminal = ratatui::try_init().map_err(io::Error::other)?;
    let result = (|| -> io::Result<()> {
        loop {
            terminal.draw(|frame| {
                draw_accounts(frame, &profiles, selected, &cwd, &notice);
                if let Some(prompt) = &prompt {
                    draw_prompt(frame, prompt);
                }
            })?;
            if !event::poll(Duration::from_millis(100))? {
                if refreshed.elapsed() >= Duration::from_secs(15) && prompt.is_none() {
                    match manager.sync(false) {
                        Ok(report)
                            if !report.updated.is_empty()
                                || !report.skipped.is_empty()
                                || !report.missing.is_empty() =>
                        {
                            notice = report.to_string()
                        }
                        Err(error) => notice = error.to_string(),
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
            if let Some(current) = prompt.take() {
                match current {
                    Prompt::Directory(mut text) => match key.code {
                        KeyCode::Esc => {}
                        KeyCode::Enter => match expand_path(&text).canonicalize() {
                            Ok(path) if path.is_dir() => cwd = path,
                            _ => notice = "项目目录不存在".into(),
                        },
                        code => {
                            if code == KeyCode::Char('u')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                text.clear();
                            } else if !key.modifiers.contains(KeyModifiers::CONTROL) {
                                edit_input(&mut text, code);
                            }
                            prompt = Some(Prompt::Directory(text));
                        }
                    },
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
                KeyCode::Char('a') | KeyCode::F(5) => {
                    let result =
                        super::provider_setup::run(handle.clone(), &mut terminal, &|frame| {
                            draw_accounts(frame, &profiles, selected, &cwd, &notice)
                        });
                    apply_setup(result, &manager, &mut profiles, &mut selected, &mut notice);
                }
                KeyCode::Char('l') => {
                    if let Some(profile) = profile {
                        if profile.auth_mode == AuthMode::Oauth {
                            let result = super::provider_setup::relogin(
                                handle.clone(),
                                &mut terminal,
                                &|frame| draw_accounts(frame, &profiles, selected, &cwd, &notice),
                                &profile,
                            );
                            apply_setup(
                                result,
                                &manager,
                                &mut profiles,
                                &mut selected,
                                &mut notice,
                            );
                        } else {
                            notice = if subscription_account(&profile) {
                                "该插件暂不支持原地替换登录；按 a 登录并绑定新账号".into()
                            } else {
                                "BYOK 使用 API Key；按 e 修改配置".into()
                            };
                        }
                    }
                }
                KeyCode::Char('e') => {
                    if let Some(profile) = profile {
                        let result = super::provider_setup::edit(
                            handle.clone(),
                            &mut terminal,
                            &|frame| draw_accounts(frame, &profiles, selected, &cwd, &notice),
                            &profile,
                        );
                        apply_setup(result, &manager, &mut profiles, &mut selected, &mut notice);
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
                    let result = super::provider_setup::run_native(
                        handle.clone(),
                        &mut terminal,
                        &|frame| draw_accounts(frame, &profiles, selected, &cwd, &notice),
                        super::provider_setup::AddMethod::CcSwitch,
                    );
                    apply_setup(result, &manager, &mut profiles, &mut selected, &mut notice);
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
                                    codex_manager::with_usage_config(
                                        command,
                                        usage_config.as_deref(),
                                    )
                                })
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
                                        usage_config: usage_config.clone(),
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
fn apply_setup(
    result: io::Result<Option<super::provider_setup::Added>>,
    manager: &Manager,
    profiles: &mut Vec<Profile>,
    selected: &mut usize,
    notice: &mut String,
) {
    match result {
        Ok(Some(added)) => {
            *notice = added.notice;
            reload(manager, profiles, selected, notice);
            if let Some(profile) = added.profile {
                *selected = profiles
                    .iter()
                    .position(|p| p.id == profile.id)
                    .unwrap_or(*selected);
            }
        }
        Ok(None) => {}
        Err(error) => *notice = error.to_string(),
    }
}

fn draw_prompt(frame: &mut ratatui::Frame, prompt: &Prompt) {
    let (title, text) = match prompt {
        Prompt::Directory(path) => ("项目目录", format!("{path}\n\nEnter 确认 · Esc 取消")),
        Prompt::Remove(_) => (
            "移出列表",
            "配置版本及历史仍会保留。\n\ny 确认 · n / Esc 取消".into(),
        ),
    };
    let area = modal(frame.area(), 9);
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
    fn profile() -> Profile {
        Profile {
            id: "fixture".into(),
            name: "WorkBuddy".into(),
            revision: 1,
            base_url: crate::plugins::account_endpoint(&uuid::Uuid::new_v4().to_string()),
            model: "model-a".into(),
            source: None,
            follow: false,
            source_status: String::new(),
            auth_mode: AuthMode::Byok,
            oauth: None,
        }
    }
    #[test]
    fn management_labels_and_actions_follow_the_selected_provider() {
        let mut p = profile();
        assert!(identity(&p).contains("订阅服务"));
        assert!(!identity(&p).contains("BYOK"));
        assert!(!identity(&p).contains("usage-plugin.invalid"));
        assert!(!actions(Some(&p)).contains("重新登录"));
        assert!(!actions(Some(&p)).contains("跟随"));
        p.base_url = "https://api.example.test/v1".into();
        p.source = Some("cc-switch".into());
        p.follow = true;
        assert!(identity(&p).contains("BYOK"));
        assert!(actions(Some(&p)).contains("跟随"));
        p.auth_mode = AuthMode::Oauth;
        assert!(actions(Some(&p)).contains("重新登录"));
        assert!(!actions(None).contains("移除"));
    }
}
