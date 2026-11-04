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
    ) -> Result<TerminalCommand, String> {
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "printf '%s|%s' \"$1\" \"$TEST_TERMINAL\" >&2; exit 7",
                "test-terminal",
            ])
            .arg(request.shell_command)
            .current_dir(request.cwd);
        Ok(command.into())
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
        plan.command.command.get_current_dir(),
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

#[test]
fn herdr_context_takes_priority_and_requires_complete_routing() {
    let registry = builtin_registry();
    let nested = context(&[
        ("HC_TERMINAL_ID", "t3"),
        ("HC_TERMINAL_SOCKET", "/tmp/herdr named.sock"),
        ("REX_SESSION", "outer"),
        ("TERM_PROGRAM", "ghostty"),
    ]);
    assert_eq!(
        registry.target(Selection::Auto, &nested).unwrap().id,
        "herdr"
    );
    assert_eq!(
        registry
            .target(Selection::Backend("rex"), &nested)
            .unwrap()
            .id,
        "rex"
    );
    for vars in [
        vec![("HC_TERMINAL_ID", "t3")],
        vec![("HC_TERMINAL_SOCKET", "/tmp/h")],
    ] {
        let ctx = context(&vars);
        assert!(registry.target(Selection::Auto, &ctx).unwrap().is_current());
        assert!(
            registry
                .prepare(
                    Selection::Backend("herdr"),
                    &ctx,
                    &TerminalRequest {
                        cwd: Path::new("/"),
                        shell_command: "usage"
                    }
                )
                .is_err()
        );
    }
    for platform in [Platform::Linux, Platform::Windows] {
        let ctx = TerminalContext::new(platform, nested.vars.clone());
        assert_eq!(registry.target(Selection::Auto, &ctx).unwrap().id, "herdr");
    }
    let mode: ResumeMode = serde_json::from_str("\"herdr\"").unwrap();
    assert_eq!(mode, ResumeMode::Herdr);
    assert_eq!(ResumeMode::Herdr.backend_id(), Some("herdr"));
    assert_eq!(ResumeMode::Rex.next(), ResumeMode::Herdr);
    assert_eq!(ResumeMode::Herdr.next(), ResumeMode::Current);
}

#[cfg(unix)]
#[test]
fn herdr_routes_both_stages_and_stops_on_failures_without_replay() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("usage-herdr-{}", uuid::Uuid::new_v4()));
    let cwd = root.join("project ' with spaces");
    std::fs::create_dir_all(&cwd).unwrap();
    let binary = root.join("hc");
    let capture = root.join("calls.jsonl");
    let shell = "usage codex run 'account with spaces' -- --model 'a;$(literal)'";
    for scenario in [
        "success",
        "lookup-failure",
        "invalid-json",
        "missing-space",
        "run-failure",
    ] {
        let script = format!(
            r#"#!/usr/bin/env python3
import json,os,sys
args=sys.argv[1:]
with open({capture:?},'a') as f:
    f.write(json.dumps({{'args':args,'socket':os.environ['HC_SOCKET'],'terminal':os.environ['HC_TERMINAL_ID']}})+'\n')
if args[0]=='get':
    if {scenario:?}=='lookup-failure': print('lookup rejected',file=sys.stderr);sys.exit(3)
    if {scenario:?}=='invalid-json': print('bad json');sys.exit(0)
    print(json.dumps({{'labels':{{'herdr.space':'sp-fixture'}}}}) if {scenario:?}!='missing-space' else '{{}}')
else:
    if {scenario:?}=='run-failure': print('run rejected',file=sys.stderr);sys.exit(4)
"#,
            capture = capture.to_str().unwrap()
        );
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&capture, "").unwrap();
        let ctx = context(&[
            ("HC_TERMINAL_ID", "t3"),
            ("HC_TERMINAL_SOCKET", "/tmp/herdr named.sock"),
            ("PATH", &format!("{}:/usr/bin:/bin", root.display())),
        ]);
        let result = builtin_registry()
            .prepare(
                Selection::Auto,
                &ctx,
                &TerminalRequest {
                    cwd: &cwd,
                    shell_command: shell,
                },
            )
            .unwrap()
            .execute();
        assert_eq!(
            result.is_ok(),
            scenario == "success",
            "{scenario}: {result:?}"
        );
        let calls: Vec<serde_json::Value> = std::fs::read_to_string(&capture)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let stages = if matches!(scenario, "success" | "run-failure") {
            2
        } else {
            1
        };
        assert_eq!(calls.len(), stages, "no retry after {scenario}");
        assert_eq!(calls[0]["args"], serde_json::json!(["get", "--json", "t3"]));
        for call in &calls {
            assert_eq!(call["socket"], "/tmp/herdr named.sock");
            assert_eq!(call["terminal"], "t3");
        }
        if stages == 2 {
            let args: Vec<_> = calls[1]["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(
                &args[..7],
                [
                    "run",
                    "--json",
                    "--bg",
                    "--no-parent",
                    "--no-notify",
                    "--keep",
                    "--cwd"
                ]
            );
            assert_eq!(Path::new(args[7]), cwd.canonicalize().unwrap());
            assert_eq!(
                &args[8..11],
                ["--label", "herdr.space=sp-fixture", "--label"]
            );
            assert!(args[11].starts_with("herdr.tab=tb"));
            assert_eq!(&args[12..], ["--", "sh", "-lc", shell]);
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
