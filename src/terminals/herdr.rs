use super::*;

pub(super) struct Herdr;

impl TerminalBackend for Herdr {
    fn target(&self) -> TerminalTarget {
        TerminalTarget {
            id: "herdr",
            label: "Herdr 新标签页",
            placement: Placement::Tab,
        }
    }

    fn environment_keys(&self) -> &'static [&'static str] {
        &[
            "HC_TERMINAL_ID",
            "HC_TERMINAL_SOCKET",
            "HC_SESSION",
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
        let binary = core_binary(context);
        let socket = context
            .var("HC_TERMINAL_SOCKET")
            .ok_or("缺少 Herdr Core 终端连接")?;
        let terminal = context
            .var("HC_TERMINAL_ID")
            .ok_or("请在 Herdr 终端中打开新标签页")?;
        let mut get = Command::new(&binary);
        // HC_TERMINAL_SOCKET identifies the actual owning core; HC_SOCKET routes its CLI.
        get.env("HC_SOCKET", socket)
            .args(["get", "--json"])
            .arg(terminal);
        let mut run = Command::new(binary);
        run.env("HC_SOCKET", socket)
            .args([
                "run",
                "--json",
                "--bg",
                "--no-parent",
                "--no-notify",
                "--keep",
                "--cwd",
            ])
            .arg(request.cwd);
        let shell_command = request.shell_command.to_owned();
        Ok(TerminalCommand::from(get).then(move |stdout| {
            let response: serde_json::Value =
                serde_json::from_slice(stdout).map_err(|_| "Herdr Core 返回了无效 JSON")?;
            let space = response
                .pointer("/labels/herdr.space")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or("当前 Herdr 终端没有所属 space；未启动 Agent")?;
            // Herdr App adopts labeled terminals without changing the viewer's focus.
            run.arg("--label")
                .arg(format!("herdr.space={space}"))
                .arg("--label")
                .arg(format!("herdr.tab=tb{}", uuid::Uuid::new_v4().simple()))
                .args(["--", "sh", "-lc"])
                .arg(shell_command);
            Ok(run)
        }))
    }
}

fn core_binary(context: &TerminalContext) -> std::path::PathBuf {
    let names: &[&str] = if context.platform() == Platform::Windows {
        &["hc.exe", "herdr-core.exe"]
    } else {
        &["hc", "herdr-core"]
    };
    if let Some(path) = context.var("PATH") {
        for dir in std::env::split_paths(path) {
            for name in names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    if let Some(home) = context.var("HOME").or_else(|| context.var("USERPROFILE")) {
        for name in names {
            let candidate = Path::new(home).join(".local/bin").join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    names[0].into()
}
