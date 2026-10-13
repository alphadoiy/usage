//! Human-readable excerpts; IDs are diagnostics, not notification content.
use std::path::Path;

use serde_json::Value;

use super::Event;

pub(super) fn project(data: &Value) -> &str {
    data["cwd"]
        .as_str()
        .and_then(|cwd| Path::new(cwd).file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Codex")
}

pub(super) fn reply(data: &Value) -> Option<&str> {
    data["last-assistant-message"]
        .as_str()
        .or_else(|| data["last_assistant_message"].as_str())
}

/// Collapse whitespace and truncate by Unicode characters, including ellipsis.
/// Stream the input so a large message does not require a second full copy.
pub(super) fn excerpt(text: &str, limit: usize) -> String {
    let mut result = String::new();
    let mut count = 0;
    for word in text.split_whitespace() {
        let separator = (count > 0).then_some(' ');
        for ch in separator
            .into_iter()
            .chain(word.chars().filter(|ch| !ch.is_control()))
        {
            if count == limit {
                result.pop();
                if limit > 0 {
                    result.push('…');
                }
                return result;
            }
            result.push(ch);
            count += 1;
        }
    }
    result
}

pub(super) fn notification(event: Event, data: &Value, account: &str) -> (String, String) {
    let status = match event {
        Event::Complete => "本轮完成",
        Event::Approval => "需要审批",
    };
    let title = format!("Codex · {} · {status}", excerpt(project(data), 60));
    let summary = match event {
        Event::Complete => {
            let text = excerpt(reply(data).unwrap_or(""), 400);
            if text.is_empty() {
                "本轮回答已结束，未提供回复摘要。请返回电脑查看。".into()
            } else {
                text
            }
        }
        Event::Approval => {
            let tool = data["tool_name"].as_str().unwrap_or("工具操作");
            let reason = data["tool_input"]["justification"]
                .as_str()
                .map(|text| excerpt(text, 300))
                .filter(|text| !text.is_empty());
            let mut text = format!("{} 请求权限审批", excerpt(tool, 60));
            if let Some(reason) = reason {
                text.push_str(&format!("：{reason}"));
            }
            text.push_str("\n请返回电脑处理。");
            text
        }
    };
    (title, format!("{summary}\n账号：{}", excerpt(account, 60)))
}
