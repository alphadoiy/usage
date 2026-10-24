//! Dashboard rendering reads state without launching queries or changing it.
use super::{Dashboard, ResetStage};
use crate::{
    sessions,
    ui::{history, session_browser, widgets::accent},
};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::Line,
    widgets::{Block, Clear, Paragraph, Widget, Wrap},
};

pub(super) fn draw(frame: &mut Frame, app: &Dashboard) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let status = if app.history_state.panel.is_some() {
        app.history_state
            .panel
            .as_ref()
            .and_then(|p| p.history.synced_at)
            .map_or_else(
                || "本地 Token · 本地时区".into(),
                |t| {
                    format!(
                        "本地 Token · 更新 {}{}",
                        t.with_timezone(&chrono::Local).format("%H:%M:%S"),
                        if app.history_state.loading {
                            " · 同步中"
                        } else {
                            ""
                        }
                    )
                },
            )
    } else if app.browser_state.loading {
        "正在读取本机会话…".to_string()
    } else {
        app.notice.clone().unwrap_or_else(|| {
            app.browser_state.panel.as_ref().map_or_else(
                || {
                    app.overview_state.error.clone().unwrap_or_else(|| {
                        if app.overview_state.loading {
                            "正在更新 Provider 状态…".into()
                        } else {
                            "Provider · 厂商状态 · 项目与会话".into()
                        }
                    })
                },
                |browser| {
                    format!(
                        "{} 个项目 · 显示 {}/{} 个会话",
                        browser.project_count(),
                        browser.visible_count(),
                        browser.sessions.len()
                    )
                },
            )
        })
    };
    let header_title = if app.history_state.panel.is_some() {
        " F10 菜单  F1 首页  [F2 历史]  F3 会话  F4 Provider "
    } else if app.browser_state.panel.is_some() {
        " F10 菜单  F1 首页  F2 历史  [F3 会话]  F4 Provider "
    } else {
        " F10 菜单  [F1 首页]  F2 历史  F3 会话  F4 Provider "
    };
    Paragraph::new(Line::from(vec![header_title.bold(), status.dim()]))
        .style(
            Style::new()
                .bg(Color::Rgb(22, 22, 24))
                .fg(Color::Rgb(242, 242, 247)),
        )
        .render(header, frame.buffer_mut());
    if let Some(panel) = &app.history_state.panel {
        history::render(body, frame.buffer_mut(), panel);
    } else if let Some(browser) = &app.browser_state.panel {
        if app.browser_state.loading && browser.sessions.is_empty() {
            Paragraph::new(" 正在扫描 Claude Code、Codex 和 Grok 的本机会话…")
                .style(Style::new().fg(Color::Rgb(142, 142, 147)))
                .render(body, frame.buffer_mut());
        } else {
            session_browser::render(body, frame.buffer_mut(), browser);
        }
    } else {
        render_home(body, frame.buffer_mut(), app);
    }
    let footer_text = if app.history_state.panel.is_some() {
        if frame.area().width >= 105 {
            " ↑新 ↓旧  ←/→ 周  Enter 会话  v 范围  f 项目  M 模型  i 实例  m 日/月  Tab 来源  c 清筛选  r 更新  Esc 返回"
        } else {
            " ↑新 ↓旧  Enter 会话  v 范围  f 项目  M 模型  i 实例  m 日/月  r 更新  Esc 返回"
        }
    } else if let Some(browser) = &app.browser_state.panel {
        if app.browser_state.loading {
            " 正在读取本机会话… Esc 返回"
        } else if browser.launching {
            " 正在 Ghostty 右侧打开会话… 请等待"
        } else {
            " ↑/↓ 选择  Tab 列表  Enter 打开  / 搜索  o 排序  R 恢复方式  y 复制  c 清除  r 更新  Esc 返回"
        }
    } else {
        match app.reset.as_ref().map(|flow| &flow.stage) {
            Some(ResetStage::Loading) => " 正在查询重置次数…   n/Esc 取消",
            Some(ResetStage::Confirm(_)) => " y 确认消耗 1 次   n/Esc 取消",
            Some(ResetStage::Executing) => " 正在提交主动重置，请等待结果…",
            Some(ResetStage::Result(_)) => " Enter / Esc 返回",
            None if app.home_sessions_state.browser.launching => " 正在恢复会话，请等待…",
            None => match app.home_focus {
                crate::ui::home::Focus::Providers => {
                    if crate::ui::overview::selected(
                        app.overview_state.catalog.as_ref(),
                        app.home_provider.as_deref(),
                    )
                    .is_some_and(|(_, provider)| crate::ui::subscription_tests::supports(provider))
                    {
                        " Tab 看板  ←/→ Provider  ↑/↓ 滚动  a 添加  Enter 模型  t 订阅测试  v 性能  x 重置  c 签到  r 刷新  F4 管理  q 退出"
                    } else {
                        " Tab 看板  ←/→ Provider  ↑/↓ 滚动  a 添加  Enter 模型  v 性能  x 重置  c 签到  r 刷新  F4 管理  q 退出"
                    }
                }
                crate::ui::home::Focus::Connections => {
                    if app.home_connections.is_details() {
                        " ↑/↓ 滚动详情  Esc 返回状态列表  Tab 看板  r 刷新  q 退出"
                    } else {
                        " Tab 看板  ↑/↓ 选择厂商  Enter 事件与历史  r 刷新  q 退出"
                    }
                }
                crate::ui::home::Focus::Projects => {
                    " Tab 看板  ↑/↓ 选择  ←项目 →会话  Enter 恢复  / 搜索  R 恢复方式  r 刷新  q 退出"
                }
            },
        }
    };
    Paragraph::new(footer_text)
        .style(
            Style::new()
                .fg(Color::Rgb(142, 142, 147))
                .bg(Color::Rgb(22, 22, 24)),
        )
        .render(footer, frame.buffer_mut());
    draw_reset_modal(frame, app);
    if let Some(panel) = &app.provider_action {
        panel.render(frame);
    }
    if let Some(panel) = &app.performance.panel {
        crate::ui::performance::draw(frame, panel);
    }
    if let Some(menu) = &app.menu {
        menu.render(frame);
    }
}

fn draw_reset_modal(frame: &mut Frame, app: &Dashboard) {
    let Some(flow) = &app.reset else {
        return;
    };
    let area = frame.area();
    let width = area.width.min(62);
    let height = area
        .height
        .min(if matches!(flow.stage, ResetStage::Confirm(_)) {
            10
        } else {
            5
        });
    let modal = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let mut lines = Vec::new();
    match &flow.stage {
        ResetStage::Loading => lines.push("正在查询可用的主动重置次数…".to_string()),
        ResetStage::Confirm(preview) => {
            lines.push(format!("可用次数：{}", preview.available));
            if let Some(account) = &preview.account_label {
                lines.push(format!("账号：{account}"));
            }
            if let Some(expiry) = preview.next_expires_at {
                lines.push(format!(
                    "最近到期：{}",
                    expiry.with_timezone(&chrono::Local).format("%m-%d %H:%M")
                ));
            }
            lines.push("将消耗 1 次，重置当前 5 小时额度。".into());
            lines.push("按 y 确认，按 n 或 Esc 取消".into());
        }
        ResetStage::Executing => lines.push("正在提交，请勿重复操作…".into()),
        ResetStage::Result(message) => lines.push(message.clone()),
    }
    Clear.render(modal, frame.buffer_mut());
    let block = Block::bordered()
        .title(format!(" {} 主动重置 ", flow.provider_name))
        .style(
            Style::new()
                .bg(Color::Rgb(22, 22, 24))
                .fg(Color::Rgb(242, 242, 247)),
        )
        .border_style(accent(&flow.provider_id));
    let inner = block.inner(modal);
    block.render(modal, frame.buffer_mut());
    Paragraph::new(lines.join("\n"))
        .wrap(Wrap { trim: true })
        .render(inner, frame.buffer_mut());
}

pub(super) fn home_lines(app: &Dashboard, width: u16) -> Vec<Line<'static>> {
    if app.home_subscription_details
        && let Some((_, provider)) = crate::ui::overview::selected(
            app.overview_state.catalog.as_ref(),
            app.home_provider.as_deref(),
        )
        && crate::ui::subscription_tests::supports(provider)
    {
        return app.subscription_tests.details(provider, width);
    }
    let summary = crate::ui::overview::selected(
        app.overview_state.catalog.as_ref(),
        app.home_provider.as_deref(),
    )
    .and_then(|(_, p)| p.performance.as_ref())
    .and_then(|b| app.home_performance.cached.get(&b.cache_key()));
    crate::ui::overview::wrap(
        crate::ui::overview::lines(
            app.overview_state.catalog.as_ref(),
            app.overview_state.loading,
            app.overview_state.error.as_deref(),
            app.home_models_expanded,
            app.home_provider.as_deref(),
            summary,
            crate::ui::overview::Extras {
                subscription_tests: Some(&app.subscription_tests),
                operation_statuses: Some(&app.operation_statuses),
            },
        ),
        width,
    )
}

pub(super) fn render_home(area: Rect, buf: &mut Buffer, app: &Dashboard) {
    use crate::ui::home::{self, Focus as HomeFocus};
    let [providers, connections, projects] = home::areas(area);
    let provider_title = crate::ui::overview::selected(
        app.overview_state.catalog.as_ref(),
        app.home_provider.as_deref(),
    )
    .map(|(index, _)| {
        format!(
            "Provider · {} / {}",
            index + 1,
            app.overview_state.catalog.as_ref().unwrap().providers.len(),
        )
    })
    .unwrap_or_else(|| "Provider".into());
    let inner = home::board(
        providers,
        buf,
        &provider_title,
        app.home_focus == HomeFocus::Providers,
    );
    render_provider_contents(inner, buf, app);
    let inner = home::board(
        connections,
        buf,
        "官方 API · 厂商状态",
        app.home_focus == HomeFocus::Connections,
    );
    app.home_connections.render(
        inner,
        buf,
        &app.service_state.statuses,
        &app.service_state.history,
        app.service_state.pending > 0,
        app.home_focus == HomeFocus::Connections,
    );
    let mode = if sessions::uses_ghostty(app.home_sessions_state.browser.resume_mode) {
        "分屏"
    } else {
        "当前终端"
    };
    let title = if app.home_sessions_state.browser.query.is_empty() {
        format!(
            "项目与会话 · {} 项目 · {mode}{}",
            app.home_sessions_state.browser.project_count(),
            if app.home_sessions_state.loading {
                " · 更新中"
            } else {
                ""
            }
        )
    } else {
        format!(
            "项目与会话 · 搜索 {}",
            crate::ui::overview::clean(&app.home_sessions_state.browser.query)
        )
    };
    let mut inner = home::board(projects, buf, &title, app.home_focus == HomeFocus::Projects);
    if let Some(warning) = app
        .home_sessions_state
        .warning
        .as_ref()
        .or(app.home_activity.error.as_ref())
    {
        Paragraph::new(format!(
            "读取警告 · {}",
            crate::ui::overview::clean(warning)
        ))
        .fg(Color::Yellow)
        .render(
            Rect {
                height: inner.height.min(1),
                ..inner
            },
            buf,
        );
        inner.y = inner.y.saturating_add(1);
        inner.height = inner.height.saturating_sub(1);
    }
    if app.home_sessions_state.browser.sessions.is_empty() {
        let message = if app.home_sessions_state.loading {
            "正在读取本地项目与会话…"
        } else {
            "尚无可恢复会话 · F4 启动 Agent · F3 完整会话页"
        };
        Paragraph::new(message)
            .wrap(Wrap { trim: false })
            .dim()
            .render(inner, buf);
    } else {
        session_browser::render_compact(inner, buf, &app.home_sessions_state.browser, &|session| {
            let account = if let Some(binding) = session.instance.strip_prefix("managed-") {
                if let Some((id, revision)) = binding.rsplit_once("-r")
                    && let Some(account) =
                        app.overview_state.catalog.as_ref().and_then(|snapshot| {
                            snapshot.providers.iter().find(|account| account.id == id)
                        })
                {
                    format!("{} · r{revision}", account.name)
                } else {
                    "托管账号（名称未加载）".into()
                }
            } else if session.instance == session.provider.id() {
                session.provider.name().into()
            } else {
                session.instance.clone()
            };
            match app.home_activity.sessions.get(&session.key()) {
                Some(activity) if !activity.runs.is_empty() => {
                    format!("{account} · {}", activity.label())
                }
                _ => account,
            }
        });
    }
}

pub(super) fn render_provider_contents(area: Rect, buf: &mut Buffer, app: &Dashboard) {
    if area.is_empty() {
        return;
    }
    let lines = home_lines(app, area.width);
    let max = lines
        .len()
        .saturating_sub(usize::from(area.height))
        .min(u16::MAX as usize) as u16;
    Paragraph::new(lines)
        .scroll((app.home_scroll.min(max), 0))
        .render(area, buf);
}
