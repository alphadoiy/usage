use super::adaptive::{self, Created, Live, LiveTab, Store};
use super::layout::{Action, Direction, group_key};
use super::*;
use std::path::PathBuf;

pub(super) struct Ghostty;

/// argv: Usage's PID. Prints `window|tab|terminal|owns-this-tty` per terminal.
/// The terminal whose TTY belongs to Usage is the anchor, so the split lands beside Usage
/// even when another tab, split or window has focus; no focus-based fallback is used.
const OBSERVE: &str = r#"on run argv
    set mine to "/dev/" & (do shell script "ps -o tty= -p " & (item 1 of argv) & " | tr -d ' '")
    set out to ""
    tell application "Ghostty"
        repeat with w in windows
            set wid to id of w
            repeat with t in tabs of w
                set tid to id of t
                repeat with term in terminals of t
                    set termId to id of term
                    set owns to false
                    try
                        set owns to ((tty of term) is equal to mine)
                    end try
                    set out to out & wid & "|" & tid & "|" & termId & "|" & (owns as text) & linefeed
                end repeat
            end repeat
        end repeat
    end tell
    return out
end run"#;

/// argv: window, anchor terminal, cwd, command, focus (keep|new), Usage's tab, mode
/// (right|down|tab), target terminal. Prints `terminal|tab` of what it created (tab is empty for splits).
/// The command is pasted into the new shell, so it stays an opaque string.
const ACT: &str = r#"on run argv
    set wid to item 1 of argv
    set anchorId to item 2 of argv
    set cwdPath to item 3 of argv
    set cmd to item 4 of argv
    set focusMode to item 5 of argv
    set homeTabId to item 6 of argv
    set mode to item 7 of argv
    set targetId to item 8 of argv
    tell application "Ghostty"
        set w to first window whose id is wid
        set anchorTerm to first terminal of w whose id is anchorId
        set targetTerm to first terminal of w whose id is targetId
        set cfg to new surface configuration
        set initial working directory of cfg to cwdPath
        set createdTab to ""
        if mode is "tab" then
            set newTab to new tab in w with configuration cfg
            set newTerm to focused terminal of newTab
            set createdTab to id of newTab
        else if mode is "right" then
            set newTerm to split targetTerm direction right with configuration cfg
        else
            set newTerm to split targetTerm direction down with configuration cfg
        end if
        input text cmd to newTerm
        send key "enter" to newTerm
        if focusMode is "new" then
            focus newTerm
        else
            select tab (first tab of w whose id is homeTabId)
            focus anchorTerm
        end if
        return (id of newTerm) & "|" & createdTab
    end tell
end run"#;

impl TerminalBackend for Ghostty {
    fn target(&self) -> TerminalTarget {
        TerminalTarget {
            id: "ghostty",
            label: "Ghostty",
            capabilities: Capabilities {
                split: true,
                tab: true,
                adaptive: true,
            },
        }
    }
    fn environment_keys(&self) -> &'static [&'static str] {
        &["GHOSTTY_RESOURCES_DIR", "TERM_PROGRAM", "PATH"]
    }
    fn available(&self, context: &TerminalContext) -> bool {
        context.platform() == Platform::MacOS
            && (context.var("GHOSTTY_RESOURCES_DIR").is_some()
                || context.var("TERM_PROGRAM") == Some(OsStr::new("ghostty")))
    }
    fn command(
        &self,
        context: &TerminalContext,
        request: &TerminalRequest<'_>,
    ) -> Result<TerminalCommand, String> {
        let osascript = locate(context, "osascript");
        let size = context.size();
        let cwd = request.cwd.to_owned();
        let shell = request.shell_command.to_owned();
        let options = request.options;
        let mut observe = Command::new(&osascript);
        observe
            .args(["-e", OBSERVE])
            .arg(std::process::id().to_string());
        Ok(TerminalCommand::from(observe).then(move |stdout| {
            let (live, window) = parse_observed(&String::from_utf8_lossy(stdout))?;
            let scope = format!("ghostty-{window}");
            let store = Store::default_store();
            let group = group_key(&cwd);
            let plan = adaptive::plan(&live, size, &store.load(&scope), options.layout, &group);
            let act = action_command(
                &osascript,
                &window,
                &live,
                &plan.action,
                &TerminalRequest {
                    cwd: &cwd,
                    shell_command: &shell,
                    options,
                },
            );
            let layout = options.layout;
            Ok(TerminalCommand::from(act).after(move |stdout| {
                let created = parse_created(&String::from_utf8_lossy(stdout));
                plan.commit(&store, &scope, created, layout, &group);
            }))
        }))
    }
}

fn action_command(
    osascript: &Path,
    window: &str,
    live: &Live,
    action: &Action,
    request: &TerminalRequest<'_>,
) -> Command {
    let mode = match action {
        Action::Split {
            direction: Direction::Right,
            ..
        } => "right",
        Action::Split {
            direction: Direction::Below,
            ..
        } => "down",
        Action::NewTab => "tab",
    };
    let mut act = Command::new(osascript);
    act.args(["-e", ACT])
        .arg(window)
        .arg(&live.anchor)
        .arg(request.cwd)
        .arg(request.shell_command)
        .arg(match request.options.focus {
            FocusMode::Keep => "keep",
            FocusMode::New => "new",
        })
        .arg(&live.tabs[live.current].id)
        .arg(mode)
        .arg(match action {
            Action::Split { pane, .. } => pane,
            Action::NewTab => &live.anchor,
        });
    act
}

fn locate(context: &TerminalContext, name: &str) -> PathBuf {
    context
        .var("PATH")
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| name.into())
}

/// Returns the window that holds Usage, with its tabs. Requires a TTY match.
fn parse_observed(output: &str) -> Result<(Live, String), String> {
    struct Row<'a> {
        window: &'a str,
        tab: &'a str,
        terminal: &'a str,
        owns: bool,
    }
    let rows: Vec<Row> = output
        .lines()
        .filter_map(|line| {
            let f: Vec<_> = line.trim().split('|').collect();
            let [window, tab, terminal, owns] = f[..] else {
                return None;
            };
            Some(Row {
                window,
                tab,
                terminal,
                owns: owns == "true",
            })
        })
        .collect();
    let anchor = rows
        .iter()
        .find(|r| r.owns)
        .ok_or("找不到 Usage 所在的 Ghostty 终端；未启动 Agent")?;
    let mut tabs: Vec<LiveTab> = Vec::new();
    for row in rows.iter().filter(|r| r.window == anchor.window) {
        match tabs.iter_mut().find(|t| t.id == row.tab) {
            Some(tab) => tab.panes.push(row.terminal.to_owned()),
            None => tabs.push(LiveTab {
                id: row.tab.to_owned(),
                panes: vec![row.terminal.to_owned()],
            }),
        }
    }
    let current = tabs
        .iter()
        .position(|t| t.id == anchor.tab)
        .ok_or("找不到 Usage 所在的 Ghostty 标签页")?;
    Ok((
        Live {
            tabs,
            current,
            anchor: anchor.terminal.to_owned(),
        },
        anchor.window.to_owned(),
    ))
}

fn parse_created(output: &str) -> Option<Created> {
    let (pane, tab) = output.trim().split_once('|')?;
    (!pane.is_empty()).then(|| Created {
        pane: pane.to_owned(),
        tab: (!tab.is_empty()).then(|| tab.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_tty_does_not_redirect_launch_to_the_focused_window() {
        assert!(parse_observed("w|t|other|false\n").is_err());
    }
    #[test]
    fn overflow_split_targets_agent_and_restores_usage_focus() {
        let live = Live {
            tabs: vec![
                LiveTab {
                    id: "home".into(),
                    panes: vec!["usage".into()],
                },
                LiveTab {
                    id: "overflow".into(),
                    panes: vec!["agent".into()],
                },
            ],
            current: 0,
            anchor: "usage".into(),
        };
        let action = Action::Split {
            tab: 1,
            pane: "agent".into(),
            direction: Direction::Below,
        };
        let command = action_command(
            Path::new("osascript"),
            "window",
            &live,
            &action,
            &TerminalRequest {
                cwd: Path::new("/tmp"),
                shell_command: "opaque",
                options: LaunchOptions::default(),
            },
        );
        let args: Vec<_> = command.get_args().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(
            &args[2..],
            [
                "window", "usage", "/tmp", "opaque", "keep", "home", "down", "agent"
            ]
        );
        assert!(ACT.contains("split targetTerm direction down"));
        assert!(ACT.contains("select tab (first tab of w whose id is homeTabId)"));
        assert!(ACT.contains("focus anchorTerm"));
    }
}
