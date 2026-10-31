use super::*;

pub(super) struct Ghostty;

impl TerminalBackend for Ghostty {
    fn target(&self) -> TerminalTarget {
        TerminalTarget {
            id: "ghostty",
            label: "Ghostty 右侧分屏",
            placement: Placement::RightSplit,
        }
    }
    fn environment_keys(&self) -> &'static [&'static str] {
        &["GHOSTTY_RESOURCES_DIR", "TERM_PROGRAM"]
    }
    fn available(&self, context: &TerminalContext) -> bool {
        context.platform() == Platform::MacOS
            && (context.var("GHOSTTY_RESOURCES_DIR").is_some()
                || context.var("TERM_PROGRAM") == Some(OsStr::new("ghostty")))
    }
    fn command(
        &self,
        _context: &TerminalContext,
        request: &TerminalRequest<'_>,
    ) -> Result<Command, String> {
        let script = r#"on run argv
        tell application "Ghostty"
            set currentTerm to focused terminal of selected tab of front window
            if currentTerm is missing value then error "找不到当前 Ghostty 终端"
            set cfg to new surface configuration
            set initial working directory of cfg to item 1 of argv
            set newTerm to split currentTerm direction right with configuration cfg
            input text (item 2 of argv) to newTerm
            send key "enter" to newTerm
            focus newTerm
        end tell
    end run"#;
        let mut command = Command::new("osascript");
        command
            .args(["-e", script])
            .arg(request.cwd)
            .arg(request.shell_command);
        Ok(command)
    }
}
