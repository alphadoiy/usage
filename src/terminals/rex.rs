use super::adaptive::{self, Created, Live, LiveTab, Store};
use super::layout::{Action, Direction, group_key};
use super::*;
use serde_json::Value;
use std::path::PathBuf;

pub(super) struct Rex;
impl TerminalBackend for Rex {
    fn target(&self) -> TerminalTarget {
        TerminalTarget {
            id: "rex",
            label: "Rex",
            capabilities: Capabilities {
                split: true,
                tab: true,
                adaptive: true,
            },
        }
    }
    fn environment_keys(&self) -> &'static [&'static str] {
        &[
            "REX_SESSION",
            "REX_BLOCK",
            "REX_BIN_DIR",
            "REX_SERVER",
            "PATH",
        ]
    }
    fn available(&self, context: &TerminalContext) -> bool {
        context.var("REX_SESSION").is_some()
    }
    fn command(
        &self,
        context: &TerminalContext,
        request: &TerminalRequest<'_>,
    ) -> Result<TerminalCommand, String> {
        let session = context
            .var("REX_SESSION")
            .ok_or("请在 Rex session 中打开新标签页")?
            .to_string_lossy()
            .into_owned();
        let anchor = context
            .var("REX_BLOCK")
            .ok_or("无法确认 Usage 所在的 Rex block；未启动 Agent")?
            .to_string_lossy()
            .into_owned();
        let binary = context
            .var("REX_BIN_DIR")
            .filter(|dir| Path::new(dir).join("rex").is_file())
            .map(|dir| Path::new(dir).join("rex"))
            .or_else(|| locate(context, "rex"))
            .unwrap_or_else(|| "rex".into());
        let mut inspect = Command::new(&binary);
        inspect.args(["session", "inspect", "--json", &session]);
        let size = context.size();
        let cwd = request.cwd.to_owned();
        let shell = request.shell_command.to_owned();
        let options = request.options;
        Ok(TerminalCommand::from(inspect).then(move |stdout| {
            let live = parse_inspect(&String::from_utf8_lossy(stdout), &anchor)?;
            let scope = format!("rex-{session}");
            let store = Store::default_store();
            let group = group_key(&cwd);
            let plan = adaptive::plan(&live, size, &store.load(&scope), options.layout, &group);
            let mut cli = Command::new(&binary);
            cli.arg("run")
                .arg("--session")
                .arg(&session)
                .arg("--cwd")
                .arg(&cwd)
                .arg(match options.focus {
                    FocusMode::Keep => "--focus=false",
                    FocusMode::New => "--focus=true",
                })
                .arg("--keep-open")
                .arg("--json");
            match &plan.action {
                Action::Split {
                    pane, direction, ..
                } => {
                    cli.args(["--block", pane])
                        .args([
                            "--split",
                            match direction {
                                Direction::Right => "right",
                                Direction::Below => "below",
                            },
                        ])
                        .args(["--ratio", "50"]);
                }
                Action::NewTab => {
                    cli.arg("--label").arg(
                        cwd.file_name()
                            .map_or_else(|| "agent".into(), |n| n.to_string_lossy().into_owned()),
                    );
                }
            }
            cli.args(["--", "sh", "-lc"]).arg(&shell);
            Ok(TerminalCommand::from(cli).after(move |stdout| {
                plan.commit(
                    &store,
                    &scope,
                    parse_created(&String::from_utf8_lossy(stdout)),
                    options.layout,
                    &group,
                );
            }))
        }))
    }
}
fn locate(context: &TerminalContext, name: &str) -> Option<PathBuf> {
    context
        .var("PATH")
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}
/// Caller identity is supplied by REX_BLOCK, never inferred from focus or ordering.
fn parse_inspect(stdout: &str, anchor: &str) -> Result<Live, String> {
    let value: Value = serde_json::from_str(stdout).map_err(|_| "Rex 返回了无效 JSON")?;
    fn walk(node: &Value, panes: &mut Vec<String>) {
        if let Some(nodes) = node.as_array() {
            for child in nodes {
                walk(child, panes);
            }
            return;
        }
        if node.get("children").is_none()
            && let Some(id) = node["block_id"].as_str().or_else(|| node["id"].as_str())
        {
            panes.push(id.to_owned());
        }
        for key in ["children", "blocks", "layers", "layout"] {
            if let Some(nodes) = node.get(key) {
                walk(nodes, panes);
            }
        }
    }
    let windows = value["windows"]
        .as_array()
        .ok_or("Rex 返回了无效窗口列表")?;
    let mut tabs = Vec::new();
    for window in windows {
        let mut panes = Vec::new();
        for key in ["layout", "block", "blocks", "layers"] {
            if let Some(node) = window.get(key) {
                walk(node, &mut panes);
            }
        }
        let mut seen = std::collections::HashSet::new();
        panes.retain(|pane| seen.insert(pane.clone()));
        tabs.push(LiveTab {
            id: window["id"]
                .as_str()
                .or_else(|| window["window_id"].as_str())
                .ok_or("Rex 返回了无效窗口 ID")?
                .to_owned(),
            panes,
        });
    }
    let current = tabs
        .iter()
        .position(|tab| tab.panes.iter().any(|pane| pane == anchor))
        .ok_or("Rex 没有返回 Usage 所在窗口；未启动 Agent")?;
    Ok(Live {
        tabs,
        current,
        anchor: anchor.to_owned(),
    })
}
fn parse_created(stdout: &str) -> Option<Created> {
    let value: Value = serde_json::from_str(stdout).ok()?;
    Some(Created {
        pane: value["block"]
            .as_str()
            .or_else(|| value["block_id"].as_str())
            .or_else(|| value["block_ids"].as_array()?.first()?.as_str())?
            .to_owned(),
        tab: value["window"]
            .as_str()
            .or_else(|| value["window_id"].as_str())
            .map(str::to_owned),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_real_rex_window_layers_and_block_ids() {
        let json = r#"{"windows":[{"window_id":"window:1","layers":[{"kind":"tiled","layout":{"block_id":"block:usage"},"blocks":[{"block_id":"block:usage"},{"block_id":"block:agent"}]}]}]}"#;
        let live = parse_inspect(json, "block:usage").unwrap();
        assert_eq!(live.tabs[0].id, "window:1");
        assert_eq!(live.tabs[0].panes, ["block:usage", "block:agent"]);
        let created =
            parse_created(r#"{"window_id":"window:1","block_ids":["block:new"],"revision":3}"#)
                .unwrap();
        assert_eq!(created.pane, "block:new");
        assert_eq!(created.tab.as_deref(), Some("window:1"));
    }
    #[test]
    fn caller_identity_wins_over_order_and_focus_and_arrays_are_walked() {
        let json = r#"{"windows":[{"id":"other","focused":true,"blocks":[{"id":"editor"}]},{"id":"home","layout":{"id":"container","children":[{"id":"agent"},{"id":"usage"}]},"blocks":[{"id":"agent"},{"id":"usage"}]}]}"#;
        let live = parse_inspect(json, "usage").unwrap();
        assert_eq!(live.current, 1);
        assert_eq!(live.anchor, "usage");
        assert_eq!(live.tabs[1].panes, ["agent", "usage"]);
        assert!(parse_inspect(json, "missing").is_err());
    }
}
