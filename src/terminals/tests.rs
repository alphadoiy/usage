use super::*;

fn context(vars: &[(&str, &str)]) -> TerminalContext {
    TerminalContext::new(
        Platform::MacOS,
        vars.iter().map(|(k, v)| (k.to_string(), OsString::from(v))),
    )
}

#[test]
fn nested_context_and_explicit_selection_have_deterministic_priority() {
    let registry = builtin_registry();
    let nested = context(&[
        ("REX_SESSION", "session:fixed"),
        ("TERM_PROGRAM", "ghostty"),
    ]);
    assert_eq!(registry.target(Selection::Auto, &nested).unwrap().id, "rex");
    assert!(
        registry
            .target(Selection::Current, &nested)
            .unwrap()
            .is_current()
    );
    assert_eq!(
        registry
            .target(Selection::Backend("ghostty"), &nested)
            .unwrap()
            .id,
        "ghostty"
    );
    let linux = TerminalContext::new(Platform::Linux, [("TERM_PROGRAM".into(), "ghostty".into())]);
    assert!(
        registry
            .target(Selection::Auto, &linux)
            .unwrap()
            .is_current()
    );
    let empty = context(&[]);
    assert!(
        registry
            .target(Selection::Auto, &empty)
            .unwrap()
            .is_current()
    );
    let request = TerminalRequest {
        cwd: Path::new("/"),
        shell_command: "usage codex run fixture",
    };
    // An explicit target cannot silently run in a different terminal.
    assert!(
        registry
            .prepare(Selection::Backend("rex"), &empty, &request)
            .is_err()
    );
    assert!(
        registry
            .prepare(Selection::Backend("missing"), &nested, &request)
            .is_err()
    );
}

struct TestBackend;
impl TerminalBackend for TestBackend {
    fn target(&self) -> TerminalTarget {
        TerminalTarget {
            id: "test",
            label: "测试终端",
            placement: Placement::Tab,
        }
    }
    fn environment_keys(&self) -> &'static [&'static str] {
        &["TEST_TERMINAL"]
    }
    fn available(&self, context: &TerminalContext) -> bool {
        context.var("TEST_TERMINAL").is_some()
    }
    fn command(
        &self,
        _context: &TerminalContext,
        request: &TerminalRequest<'_>,
    ) -> Result<Command, String> {
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "printf '%s|%s' \"$1\" \"$TEST_TERMINAL\" >&2; exit 7",
                "test-terminal",
            ])
            .arg(request.shell_command)
            .current_dir(request.cwd);
        Ok(command)
    }
}

#[cfg(unix)]
#[test]
fn an_injected_backend_launches_through_the_common_path_and_surfaces_failure() {
    let registry = TerminalRegistry::new(vec![Box::new(TestBackend)]).unwrap();
    let ctx = context(&[("TEST_TERMINAL", "active")]);
    let root = std::env::temp_dir().join(format!("usage-terminal-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("project ' with spaces")).unwrap();
    let cwd = root.join("project ' with spaces");
    let shell = "opaque command 'with spaces' $()";
    let plan = registry
        .prepare(
            Selection::Auto,
            &ctx,
            &TerminalRequest {
                cwd: &cwd,
                shell_command: shell,
            },
        )
        .unwrap();
    assert_eq!(plan.target.id, "test");
    assert_eq!(
        plan.command.get_current_dir(),
        Some(cwd.canonicalize().unwrap().as_path())
    );
    let error = plan.execute().unwrap_err();
    assert!(error.contains(shell), "{error}");
    assert!(error.contains("测试终端"), "{error}");
    assert!(
        error.ends_with("|active"),
        "prepared context must reach the child: {error}"
    );
    assert!(
        registry
            .prepare(
                Selection::Auto,
                &ctx,
                &TerminalRequest {
                    cwd: &root.join("missing"),
                    shell_command: shell
                }
            )
            .is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn duplicate_or_reserved_registration_cannot_replace_an_adapter() {
    assert!(TerminalRegistry::new(vec![Box::new(TestBackend), Box::new(TestBackend)]).is_err());
}
