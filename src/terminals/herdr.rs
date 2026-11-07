use super::adaptive::{self, Created, Live, LiveTab, Store};
use super::layout::{Action, Direction, group_key};
use super::*;
use serde_json::{Value, json};
use std::path::PathBuf;

/// Herdr App's documented window commands (`herdr-app do …`), not the unlabeled-terminal
/// adoption convention: they split a chosen pane, name tabs and focus deliberately.
pub(super) struct Herdr;

impl TerminalBackend for Herdr {
    fn target(&self) -> TerminalTarget {
        TerminalTarget {
            id: "herdr",
            label: "Herdr",
            capabilities: Capabilities {
                split: true,
                tab: true,
                adaptive: true,
            },
        }
    }

    fn environment_keys(&self) -> &'static [&'static str] {
        &[
            "HC_TERMINAL_ID",
            "HC_TERMINAL_SOCKET",
            "HERDR_APP_HOME",
            "PATH",
            "HOME",
            "USERPROFILE",
        ]
    }

    fn available(&self, context: &TerminalContext) -> bool {
        context.var("HC_TERMINAL_ID").is_some() && context.var("HC_TERMINAL_SOCKET").is_some()
    }

    fn command(
        &self,
        context: &TerminalContext,
        request: &TerminalRequest<'_>,
    ) -> Result<TerminalCommand, String> {
        let app = app_binary(context);
        // `HC_TERMINAL_ID` is a terminal address; `ls` maps it to its tab and pane.
        let terminal = context
            .var("HC_TERMINAL_ID")
            .ok_or("请在 Herdr 终端中打开新实例")?
            .to_string_lossy()
            .into_owned();
        let size = context.size();
        let cwd = request.cwd.to_owned();
        let shell = request.shell_command.to_owned();
        let endpoint = context
            .var("HC_TERMINAL_SOCKET")
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let options = request.options;
        let reuse_key = request.reuse_key.map(str::to_owned);
        let mut ls = Command::new(&app);
        ls.arg("ls");
        Ok(TerminalCommand::from(ls).then(move |stdout| {
            let (live, space) = parse_ls(&String::from_utf8_lossy(stdout), &terminal)?;
            let scope = format!("herdr-{endpoint}-{space}");
            let store = Store::default_store();
            let group = group_key(&cwd);
            let model = store.load(&scope);
            if let Some(existing) = model.existing(reuse_key.as_deref(), &live) {
                let focus = do_command(&app, "pane.focus", json!({"pane": existing.pane}));
                let select = do_command(&app, "tab.select", json!({"id": existing.tab}));
                return Ok(TerminalCommand::from(select).then(move |_| Ok(focus)));
            }
            let plan = adaptive::plan(&live, size, &model, options.layout, &group);
            let launch = launch_command(
                &app,
                &space,
                &plan.action,
                &TerminalRequest {
                    cwd: &cwd,
                    shell_command: &shell,
                    reuse_key: None,
                    options,
                },
            );
            let home_tab = live
                .tabs
                .get(live.current)
                .map(|tab| tab.id.clone())
                .unwrap_or_default();
            let anchor = live.anchor.clone();
            let (hook_app, focus_new) = (app.clone(), options.focus == FocusMode::New);
            let layout = options.layout;
            let destination_tab = match &plan.action {
                Action::Split { tab, .. } => Some(live.tabs[*tab].id.clone()),
                Action::NewTab => None,
            };
            Ok(TerminalCommand::from(launch)
                .marks_created()
                .after(move |stdout| {
                    plan.commit(
                        &store,
                        &scope,
                        parse_created(stdout),
                        layout,
                        &group,
                        reuse_key.as_deref(),
                    );
                })
                .then(move |stdout| {
                    // Keep returns to Usage whatever the app did; New follows the new pane.
                    let created = parse_created(stdout);
                    let (target, tab) = if focus_new {
                        let created =
                            created.ok_or("实例已创建，但无法确认新 pane；请勿重复启动")?;
                        (
                            created.pane,
                            created
                                .tab
                                .or(destination_tab)
                                .ok_or("实例已创建，但无法确认新标签页；请勿重复启动")?,
                        )
                    } else {
                        (anchor, home_tab)
                    };
                    let focus = do_command(&hook_app, "pane.focus", json!({ "pane": target }));
                    if tab.is_empty() {
                        return Ok(TerminalCommand::from(focus));
                    }
                    let select = do_command(&hook_app, "tab.select", json!({ "id": tab }));
                    Ok(TerminalCommand::from(select).then(move |_| Ok(focus)))
                }))
        }))
    }
}

fn launch_command(
    app: &Path,
    space: &str,
    action: &Action,
    request: &TerminalRequest<'_>,
) -> Command {
    let cwd = request.cwd;
    let argv = json!([
        "sh",
        "-c",
        AGENT_WRAPPER,
        "usage-agent",
        cwd,
        request.shell_command
    ]);
    match action {
        Action::Split {
            pane, direction, ..
        } => do_command(
            app,
            "pane.split",
            json!({
                "direction": match direction { Direction::Right => "right", Direction::Below => "down" },
                "pane": pane, "argv": argv,
            }),
        ),
        Action::NewTab => do_command(
            app,
            "tab.new",
            json!({
                "space": space,
                "name": cwd.file_name().map_or_else(|| "agent".into(), |n| n.to_string_lossy().into_owned()),
                "folder": cwd, "argv": argv,
            }),
        ),
    }
}

fn do_command(app: &Path, name: &str, arguments: Value) -> Command {
    let mut command = Command::new(app);
    command.args(["do", name]).arg(arguments.to_string());
    command
}

/// `ls` lists spaces > tabs > terminals (with pane ids). Only the caller's space is relevant.
fn parse_ls(stdout: &str, terminal: &str) -> Result<(Live, String), String> {
    let value: Value = serde_json::from_str(stdout).map_err(|_| "Herdr 返回了无效 JSON")?;
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    for space in value["spaces"].as_array().into_iter().flatten() {
        let mut tabs = Vec::new();
        let mut current = None;
        let mut anchor = None;
        for tab in space["tabs"].as_array().into_iter().flatten() {
            let Some(id) = text(tab, "id") else { continue };
            let mut panes = Vec::new();
            for entry in tab["terminals"].as_array().into_iter().flatten() {
                let Some(pane) = text(entry, "pane") else {
                    continue;
                };
                if text(entry, "terminal").as_deref() == Some(terminal) {
                    current = Some(tabs.len());
                    anchor = Some(pane.clone());
                }
                panes.push(pane);
            }
            tabs.push(LiveTab { id, panes });
        }
        if let (Some(current), Some(anchor), Some(space)) = (current, anchor, text(space, "id")) {
            return Ok((
                Live {
                    tabs,
                    current,
                    anchor,
                },
                space,
            ));
        }
    }
    Err("当前 Herdr 终端不在任何标签页中；未启动 Agent".into())
}

fn parse_created(stdout: &[u8]) -> Option<Created> {
    let value: Value = serde_json::from_slice(stdout).ok()?;
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(Created {
        pane: text("pane")?,
        tab: text("tab").or_else(|| text("tab_id")),
    })
}

fn app_binary(context: &TerminalContext) -> PathBuf {
    let name = "herdr-app";
    if let Some(home) = context.var("HERDR_APP_HOME") {
        let candidate = Path::new(home).join("bin").join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    if let Some(path) = context.var("PATH") {
        for dir in std::env::split_paths(path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    if let Some(home) = context.var("HOME").or_else(|| context.var("USERPROFILE")) {
        let candidate = Path::new(home).join(".local/bin").join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    name.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tabs_are_created_in_the_callers_space_not_the_selected_space() {
        let (live, space) = parse_ls(r#"{"spaces":[{"id":"selected","tabs":[{"id":"other-tab","terminals":[{"terminal":"other","pane":"other-pane"}]}]},{"id":"home","tabs":[{"id":"usage-tab","terminals":[{"terminal":"usage","pane":"usage-pane"}]}]}]}"#, "usage").unwrap();
        assert_eq!(live.anchor, "usage-pane");
        let command = launch_command(
            Path::new("herdr-app"),
            &space,
            &Action::NewTab,
            &TerminalRequest {
                cwd: Path::new("/tmp"),
                shell_command: "opaque",
                reuse_key: None,
                options: LaunchOptions::default(),
            },
        );
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args[1], "tab.new");
        let payload: Value = serde_json::from_str(args[2].to_str().unwrap()).unwrap();
        assert_eq!(payload["space"], "home");
        assert_eq!(payload["folder"], "/tmp");
    }
}
