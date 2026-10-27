//! Conversion preferences contain no credentials and don't change the Agent revision.
use std::{io, path::Path, time::Duration};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::codex_manager::{AuthMode, Profile};
use crate::resources::QuotaConversion;

struct Target {
    id: Option<String>,
    label: String,
    ratio: String,
    currency: String,
}

fn targets(config: &crate::config::AppConfig, profile: &str) -> Vec<Target> {
    let associated: Vec<_> = config
        .resources
        .iter()
        .filter(|r| r.profile_ids.iter().any(|id| id == profile))
        .collect();
    if associated.is_empty() {
        let conversion = config
            .provider_usage
            .get(profile)
            .cloned()
            .unwrap_or_default();
        return vec![Target {
            id: None,
            label: "自动识别 / CC Switch".into(),
            ratio: conversion.quota_per_unit.unwrap_or_default(),
            currency: conversion.currency.unwrap_or_default(),
        }];
    }
    associated
        .into_iter()
        .filter(|r| matches!(r.service.as_str(), "new_api_token" | "new_api_account"))
        .map(|r| Target {
            id: Some(r.id.clone()),
            label: format!(
                "{}{}",
                r.label.as_deref().unwrap_or(&r.id),
                if r.profile_ids.len() > 1 {
                    "（共享，影响所有关联账号）"
                } else {
                    ""
                }
            ),
            ratio: r.options["quota_per_unit"]
                .as_str()
                .unwrap_or_default()
                .into(),
            currency: r.options["currency"].as_str().unwrap_or_default().into(),
        })
        .collect()
}

pub(in crate::ui) fn edit(
    terminal: &mut DefaultTerminal,
    background: &dyn Fn(&mut Frame),
    profile: &Profile,
    path: &Path,
) -> io::Result<bool> {
    if profile.auth_mode != AuthMode::Byok
        || crate::plugins::endpoint_account(&profile.base_url).is_some()
    {
        return Err(io::Error::other("此账号不使用 New API 原始额度换算"));
    }
    let original = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let config = crate::config::parse_config(&original).map_err(io::Error::other)?;
    let mut targets = targets(&config, &profile.id);
    if targets.is_empty() {
        return Err(io::Error::other(
            "当前配置的资源不使用 New API 原始额度换算",
        ));
    }
    let mut target = 0usize;
    let mut field = 0usize;
    let mut status = String::new();
    loop {
        terminal.draw(|frame| {
            background(frame);
            let area = crate::ui::forms::modal(frame.area(), 14);
            frame.render_widget(Clear, area);
            let current = &targets[target];
            let rows = [
                format!("作用范围：{}", current.label),
                format!("每一货币单位对应额度：{}", current.ratio),
                format!("币种：{}", current.currency),
            ];
            let mut lines = vec![
                Line::from(format!("{} · New API 额度换算", profile.name)),
                Line::from(""),
            ];
            for (index, row) in rows.iter().enumerate() {
                lines.push(Line::from(format!(
                    "{} {row}",
                    if field == index { "›" } else { " " }
                )));
            }
            lines.extend([
                Line::from(""),
                Line::from("金额 = 原始额度 ÷ 比例；例如 500000 和 USD"),
                Line::from("请按站点实际设置填写。两项留空清除换算覆盖。"),
                Line::from("Tab / ↑↓ 换字段 · ←→ 换资源 · Ctrl+S 保存 · Esc 取消"),
                Line::from(status.as_str()),
            ]);
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title(" 额度设置 ")),
                area,
            );
        })?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Ok(false),
            KeyCode::Char('c') if control => return Ok(false),
            KeyCode::Tab | KeyCode::Down | KeyCode::Enter => field = (field + 1) % 3,
            KeyCode::BackTab | KeyCode::Up => field = (field + 2) % 3,
            KeyCode::Left if field == 0 => target = (target + targets.len() - 1) % targets.len(),
            KeyCode::Right if field == 0 => target = (target + 1) % targets.len(),
            KeyCode::Char('s') if control => {
                let current = &targets[target];
                let nonempty = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_owned());
                let conversion = QuotaConversion {
                    quota_per_unit: nonempty(&current.ratio),
                    currency: nonempty(&current.currency).map(|v| v.to_ascii_uppercase()),
                };
                match crate::config::save_quota_conversion(
                    path,
                    &original,
                    &profile.id,
                    current.id.as_deref(),
                    &conversion,
                ) {
                    Ok(()) => return Ok(true),
                    Err(error) => status = error.to_string(),
                }
            }
            code if field > 0 => {
                let current = &mut targets[target];
                let text = if field == 1 {
                    &mut current.ratio
                } else {
                    &mut current.currency
                };
                if control && code == KeyCode::Char('u') {
                    text.clear();
                } else if !control {
                    crate::ui::forms::edit_input(text, code);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_new_api_resources_override_automatic_conversion_targets() {
        let id = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4();
        let config = crate::config::parse_config(&format!("[[resources]]\nid='wallet'\nservice='new_api_account'\nbase_url='https://relay.invalid'\ncredential_env='TOKEN'\nprofile_ids=['{id}','{other}']\noptions={{user_id=42,quota_per_unit='500000',currency='USD'}}\n")).unwrap();
        let available = targets(&config, &id);
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id.as_deref(), Some("wallet"));
        assert!(available[0].label.contains("共享"));
        assert_eq!(available[0].ratio, "500000");
        assert_eq!(available[0].currency, "USD");
        let config = crate::config::parse_config(&format!(
            "[provider_usage.'{id}']\nquota_per_unit='1000'\ncurrency='CNY'\n"
        ))
        .unwrap();
        let available = targets(&config, &id);
        assert_eq!(available[0].id, None);
        assert_eq!(available[0].ratio, "1000");
    }
}
