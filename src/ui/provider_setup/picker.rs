//! Option picker for Agent fields: known values first, type to filter or enter a custom value.
use crate::provider_catalog::Models;
use ratatui::style::{Color, Stylize};
use ratatui::text::Line;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Choice {
    pub value: String,
    pub note: String,
}
impl Choice {
    fn new(value: &str, note: &str) -> Self {
        Self {
            value: value.into(),
            note: note.into(),
        }
    }
}

pub(super) fn efforts(optional: bool) -> Vec<Choice> {
    let mut items: Vec<Choice> = crate::codex_manager::config::REASONING_EFFORTS
        .iter()
        .map(|value| {
            let note = match *value {
                "low" => "低",
                "medium" => "中",
                "high" => "高",
                "xhigh" => "极高（需模型支持）",
                "minimal" => "最少（需模型支持）",
                _ => "",
            };
            Choice::new(value, note)
        })
        .collect();
    if optional {
        items.insert(0, Choice::new("", "使用默认"));
    }
    items
}

pub(super) fn models(models: Option<&Models>, optional: bool) -> Vec<Choice> {
    let mut items = Vec::new();
    if optional {
        items.push(Choice::new("", "使用默认"));
    }
    if let Some(Models::Ready(list)) = models {
        items.extend(list.iter().map(|m| Choice::new(m, "")));
    }
    items
}

/// The current value leads, deduplicated, so reopening a field keeps its value one Enter away.
pub(super) fn with_current(items: Vec<Choice>, current: &str) -> Vec<Choice> {
    let mut out: Vec<Choice> = Vec::new();
    if let Some(found) = items.iter().find(|c| c.value == current) {
        let note = if found.note.is_empty() {
            "当前".into()
        } else {
            format!("{} · 当前", found.note)
        };
        out.push(Choice::new(current, &note));
    } else if !current.is_empty() {
        out.push(Choice::new(current, "当前"));
    }
    for item in items {
        if !out.iter().any(|c| c.value == item.value) {
            out.push(item);
        }
    }
    out
}

pub(super) struct Picker {
    pub field: usize,
    pub label: &'static str,
    pub query: String,
    pub cursor: usize,
    pub items: Vec<Choice>,
    /// Model fields show fetch progress and can be fetched again.
    pub models: bool,
}
impl Picker {
    /// Prefix, then substring, then subsequence of the value, then a note match.
    pub fn matches(&self) -> Vec<&Choice> {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            return self.items.iter().collect();
        }
        let mut hits: Vec<(usize, &Choice)> = self
            .items
            .iter()
            .filter_map(|item| {
                score(&item.value.to_lowercase(), &item.note.to_lowercase(), &q).map(|s| (s, item))
            })
            .collect();
        hits.sort_by_key(|(s, _)| *s);
        hits.into_iter().map(|(_, item)| item).collect()
    }
    pub fn choice(&self) -> Option<String> {
        match self.matches().get(self.cursor) {
            Some(item) => Some(item.value.clone()),
            None => Some(self.query.trim().to_string()).filter(|v| !v.is_empty()),
        }
    }
    pub fn move_by(&mut self, delta: isize) {
        let n = self.matches().len();
        if n > 0 {
            self.cursor = (self.cursor as isize + delta).rem_euclid(n as isize) as usize;
        }
    }
    pub fn lines(&self, status: Option<&Models>, rows: usize) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(format!("❯ {}▏", self.query)).fg(Color::LightBlue),
            Line::from(""),
        ];
        let matches = self.matches();
        if matches.is_empty() {
            lines.push(match self.choice() {
                Some(v) => Line::from(format!("↵ 使用自定义值 {v}")),
                None if self.items.is_empty() => Line::from("暂无可选项，直接输入").dim(),
                None => Line::from("没有匹配项").dim(),
            });
        }
        let first = self.cursor.saturating_sub(rows.saturating_sub(1));
        for (k, item) in matches.iter().enumerate().skip(first).take(rows) {
            let value = if item.value.is_empty() {
                "（默认）"
            } else {
                item.value.as_str()
            };
            let text = if item.note.is_empty() {
                value.to_string()
            } else {
                format!("{value}  {}", item.note)
            };
            lines.push(if k == self.cursor {
                Line::from(format!("› {text}")).fg(Color::LightBlue)
            } else {
                Line::from(format!("  {text}"))
            });
        }
        if matches.len() > rows {
            let end = (first + rows).min(matches.len());
            lines.push(Line::from(format!("{}–{} / {}", first + 1, end, matches.len())).dim());
        }
        if self.models {
            match status {
                None | Some(Models::Loading) => lines.push(Line::from("正在获取模型列表…").dim()),
                Some(Models::Unavailable(error)) => {
                    lines.push(Line::from(format!("模型列表不可用：{error}；可直接输入")).dim())
                }
                Some(Models::Ready(_)) => {}
            }
        }
        lines.push(Line::from(""));
        lines.push(
            Line::from(if self.models {
                "↑↓ 选择 · 输入筛选或自定义 · Enter 确定 · Ctrl+R 重新获取 · Esc 返回"
            } else {
                "↑↓ 选择 · 输入筛选或自定义 · Enter 确定 · Esc 返回"
            })
            .dim(),
        );
        lines
    }
}

fn score(value: &str, note: &str, q: &str) -> Option<usize> {
    if value.starts_with(q) {
        Some(0)
    } else if let Some(i) = value.find(q) {
        Some(1 + i)
    } else if subsequence(value, q) {
        Some(1000)
    } else if note.contains(q) {
        Some(2000)
    } else {
        None
    }
}
fn subsequence(s: &str, q: &str) -> bool {
    let mut chars = s.chars();
    q.chars().all(|c| chars.any(|x| x == c))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn picker(items: Vec<Choice>, query: &str) -> Picker {
        Picker {
            field: 2,
            label: "模型",
            query: query.into(),
            cursor: 0,
            items,
            models: true,
        }
    }

    #[test]
    fn current_value_leads_and_filtering_ranks_prefix_before_subsequence() {
        let list = Models::Ready(vec![
            "gpt-5".into(),
            "deepseek-v4".into(),
            "gpt-5-mini".into(),
        ]);
        let items = with_current(models(Some(&list), false), "deepseek-v4");
        assert_eq!(items[0], Choice::new("deepseek-v4", "当前"));
        assert_eq!(items.len(), 3);
        let p = picker(items.clone(), "g5m");
        assert_eq!(p.choice().as_deref(), Some("gpt-5-mini"));
        let p = picker(items, "GPT");
        assert_eq!(
            p.matches()
                .iter()
                .map(|c| c.value.as_str())
                .collect::<Vec<_>>(),
            ["gpt-5", "gpt-5-mini"]
        );
    }

    #[test]
    fn unmatched_text_is_a_custom_value_and_blank_is_offered_only_when_optional() {
        let p = picker(with_current(models(None, false), ""), "my-model ");
        assert!(p.matches().is_empty());
        assert_eq!(p.choice().as_deref(), Some("my-model"));
        assert!(picker(Vec::new(), "  ").choice().is_none());
        assert_eq!(efforts(true)[0].value, "");
        assert!(efforts(false).iter().all(|c| !c.value.is_empty()));
        // An unknown saved value is still offered as current.
        assert_eq!(
            with_current(efforts(false), "max")[0],
            Choice::new("max", "当前")
        );
    }
}
