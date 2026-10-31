use super::*;

pub(super) struct Rex;

impl TerminalBackend for Rex {
    fn target(&self) -> TerminalTarget {
        TerminalTarget {
            id: "rex",
            label: "Rex 新标签页",
            placement: Placement::Tab,
        }
    }
    fn environment_keys(&self) -> &'static [&'static str] {
        &["REX_SESSION", "REX_BIN_DIR", "REX_SERVER"]
    }
    fn available(&self, context: &TerminalContext) -> bool {
        context.var("REX_SESSION").is_some()
    }
    fn command(
        &self,
        context: &TerminalContext,
        request: &TerminalRequest<'_>,
    ) -> Result<Command, String> {
        let session = context
            .var("REX_SESSION")
            .ok_or("请在 Rex session 中打开新标签页")?;
        // Match the server's CLI even when shell startup reordered PATH.
        let binary = context
            .var("REX_BIN_DIR")
            .map(|dir| Path::new(dir).join("rex"))
            .unwrap_or_else(|| "rex".into());
        let mut cli = Command::new(binary);
        cli.arg("run")
            .arg("--session")
            .arg(session)
            .arg("--cwd")
            .arg(request.cwd)
            .args(["--focus=false", "--keep-open", "--", "sh", "-lc"])
            .arg(request.shell_command);
        Ok(cli)
    }
}
