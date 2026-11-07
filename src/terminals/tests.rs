use super::*;

fn context(vars: &[(&str, &str)]) -> TerminalContext {
    TerminalContext::new(
        Platform::MacOS,
        vars.iter().map(|(k, v)| (k.to_string(), OsString::from(v))),
    )
}

#[cfg(unix)]
#[test]
fn herdr_resumes_focus_existing_panes_and_recreate_closed_ones() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("usage-focus-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let binary = root.join("herdr-app");
    let state = root.join("state.json");
    let calls = root.join("calls.jsonl");
    let scope = uuid::Uuid::new_v4().to_string();
    std::fs::write(
        &binary,
        format!(
            r#"#!/usr/bin/env python3
import json,sys,pathlib
state=pathlib.Path({state:?})
s=json.loads(state.read_text()) if state.exists() else {{'n':0,'panes':[]}}
a=sys.argv[1:]
with open({calls:?},'a') as f: f.write(json.dumps(a)+'\n')
if a[0]=='ls':
    tabs=[{{'id':'home','terminals':[{{'terminal':'usage','pane':'anchor'}}]}}]
    tabs += [{{'id':'tab-'+p,'terminals':[{{'terminal':'t-'+p,'pane':p}}]}} for p in s['panes']]
    print(json.dumps({{'spaces':[{{'id':{scope:?},'tabs':tabs}}]}}))
elif a[1]=='tab.new':
    s['n']+=1
    pane='p'+str(s['n']);s['panes'].append(pane);state.write_text(json.dumps(s))
    print(json.dumps({{'pane':pane,'tab':'tab-'+pane}}))
else: print('{{"ok":true}}')
"#,
            state = state.to_str().unwrap(),
            calls = calls.to_str().unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let ctx = context(&[
        ("HC_TERMINAL_ID", "usage"),
        ("HC_TERMINAL_SOCKET", "fixture"),
        ("PATH", &format!("{}:/usr/bin:/bin", root.display())),
    ]);
    let launch = |key| {
        builtin_registry()
            .prepare(
                Selection::Auto,
                &ctx,
                &TerminalRequest {
                    cwd: &root,
                    shell_command: "opaque resume command",
                    reuse_key: key,
                    options: LaunchOptions {
                        layout: LayoutMode::Tab,
                        ..Default::default()
                    },
                },
            )
            .unwrap()
            .execute()
            .unwrap()
    };
    launch(Some("same-session-and-target"));
    launch(Some("same-session-and-target"));
    let recorded: Vec<serde_json::Value> = std::fs::read_to_string(&calls)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(recorded.iter().filter(|c| c[1] == "tab.new").count(), 1);
    assert_eq!(recorded.iter().filter(|c| c[1] == "pane.focus").count(), 2);
    for call in recorded.iter().filter(|c| c[1] == "tab.select") {
        let args: serde_json::Value = serde_json::from_str(call[2].as_str().unwrap()).unwrap();
        assert_eq!(
            args["id"], "tab-p1",
            "must switch to the target tab before focus"
        );
    }
    launch(Some("different-target"));
    launch(None);
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert_eq!(saved["n"], 3);
    saved["panes"] = serde_json::json!([]);
    std::fs::write(&state, serde_json::to_vec(&saved).unwrap()).unwrap();
    launch(Some("same-session-and-target"));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert_eq!(saved["n"], 4, "closed panes must be recreated");
    std::fs::remove_dir_all(root).unwrap();
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
        reuse_key: None,
        options: LaunchOptions {
            focus: FocusMode::Keep,
            ..Default::default()
        },
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
            capabilities: Capabilities {
                split: false,
                tab: true,
                adaptive: false,
            },
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
                reuse_key: None,
                options: LaunchOptions {
                    focus: FocusMode::Keep,
                    ..Default::default()
                },
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
                    shell_command: shell,
                    reuse_key: None,
                    options: LaunchOptions {
                        focus: FocusMode::Keep,
                        ..Default::default()
                    },
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
                        shell_command: "usage",
                        reuse_key: None,
                        options: LaunchOptions {
                            focus: FocusMode::Keep,
                            ..Default::default()
                        },
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
fn herdr_routes_all_stages_and_stops_on_failures_without_replay() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("usage-herdr-app-{}", uuid::Uuid::new_v4()));
    let cwd = root.join("project ' with spaces");
    std::fs::create_dir_all(&cwd).unwrap();
    let app = root.join("herdr-app");
    let capture = root.join("calls.jsonl");
    let shell = "usage codex run 'account with spaces' -- --model 'a;$(literal)'";
    for scenario in [
        "success",
        "ls-failure",
        "invalid-json",
        "missing-terminal",
        "launch-failure",
        "bad-created",
        "focus-failure",
    ] {
        std::fs::write(
            &app,
            format!(
                r#"#!/usr/bin/env python3
import json,os,sys
args=sys.argv[1:]
with open({capture:?},'a') as f: f.write(json.dumps({{'args':args}})+'\n')
if args[0]=='ls':
    if {scenario:?}=='ls-failure': print('ls failed',file=sys.stderr);sys.exit(3)
    if {scenario:?}=='invalid-json': print('bad json');sys.exit(0)
    print('{{"spaces":[{{"id":"sp1","tabs":[{{"id":"tb1","terminals":[{{"terminal":"t3","pane":"p1"}},{{"terminal":"t9","pane":"p2"}}]}}]}}]}}' if {scenario:?}!='missing-terminal' else '{{"spaces":[{{"id":"sp1","tabs":[{{"id":"tb1","terminals":[{{"terminal":"t9","pane":"p2"}}]}}]}}]}}')
elif args[0]=='do':
    if args[1]=='pane.split':
        if {scenario:?}=='launch-failure': print(json.dumps({{"error":"split failed"}}));sys.exit(4)
        if {scenario:?}=='bad-created': print(json.dumps({{"pane":"p-new"}}),flush=True);print('unexpected trailing output',file=sys.stderr);sys.exit(0)
        print(json.dumps({{"pane":"p-new","terminal":"t-new"}}))
    elif args[1]=='pane.focus' or args[1]=='tab.select':
        if {scenario:?}=='focus-failure': print('focus failed',file=sys.stderr);sys.exit(5)
        print(json.dumps({{"ok":True}}))
"#,
                capture = capture.to_str().unwrap()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&capture, "").unwrap();
        let ctx = context(&[
            ("HC_TERMINAL_ID", "t3"),
            ("HC_TERMINAL_SOCKET", "/tmp/herdr named.sock"),
            ("PATH", &format!("{}:/usr/bin:/bin", root.display())),
        ])
        .with_size(200, 50);
        let result = builtin_registry()
            .prepare(
                Selection::Auto,
                &ctx,
                &TerminalRequest {
                    cwd: &cwd,
                    shell_command: shell,
                    reuse_key: None,
                    options: LaunchOptions {
                        focus: FocusMode::Keep,
                        ..Default::default()
                    },
                },
            )
            .unwrap()
            .execute();
        assert_eq!(
            result.is_ok(),
            scenario == "success" || scenario == "bad-created",
            "{scenario}: {result:?}"
        );
        let calls: Vec<serde_json::Value> = std::fs::read_to_string(&capture)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        if scenario == "focus-failure" {
            assert!(result.unwrap_err().contains("实例已创建"));
        }
        let stages = match scenario {
            "success" | "bad-created" => 4,
            "ls-failure" | "invalid-json" | "missing-terminal" => 1,
            "launch-failure" => 2,
            "focus-failure" => 3,
            _ => unreachable!("unknown Herdr scenario"),
        };
        assert_eq!(calls.len(), stages, "no retry after {scenario}");
        if stages == 4 {
            assert_eq!(calls[0]["args"][0], "ls");
            assert_eq!(calls[1]["args"][0], "do");
            assert_eq!(calls[1]["args"][1], "pane.split");
            assert_eq!(calls[2]["args"][1], "tab.select");
            assert_eq!(calls[3]["args"][1], "pane.focus");
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

fn options(layout: LayoutMode, focus: FocusMode) -> LaunchOptions {
    LaunchOptions { layout, focus }
}

#[test]
fn layout_intent_resolves_the_same_way_for_every_terminal_capability() {
    use LayoutMode::{Auto, Split, Tab};
    let both = Capabilities {
        split: true,
        tab: true,
        adaptive: false,
    };
    let tab_only = Capabilities {
        split: false,
        tab: true,
        adaptive: false,
    };
    let split_only = Capabilities {
        split: true,
        tab: false,
        adaptive: false,
    };
    let none = Capabilities {
        split: false,
        tab: false,
        adaptive: false,
    };
    for (caps, layout, expected) in [
        (both, Auto, Placement::Split),
        (both, Split, Placement::Split),
        (both, Tab, Placement::Tab),
        (tab_only, Auto, Placement::Tab),
        (tab_only, Split, Placement::Tab),
        (tab_only, Tab, Placement::Tab),
        (split_only, Auto, Placement::Split),
        (split_only, Split, Placement::Split),
        (split_only, Tab, Placement::Split),
        (none, Auto, Placement::Current),
        (none, Tab, Placement::Current),
    ] {
        assert_eq!(resolve(caps, layout), expected, "{caps:?} {layout:?}");
    }
}

#[test]
fn descriptions_never_promise_a_layout_the_terminal_cannot_make() {
    let registry = builtin_registry();
    let ctx = context(&[
        ("REX_SESSION", "s"),
        ("HC_TERMINAL_ID", "t"),
        ("HC_TERMINAL_SOCKET", "/tmp/s"),
    ]);
    let herdr = registry.target(Selection::Backend("herdr"), &ctx).unwrap();
    let rex = registry.target(Selection::Backend("rex"), &ctx).unwrap();
    let any = |layout| options(layout, FocusMode::Keep);
    assert_eq!(herdr.describe(any(LayoutMode::Auto)), "Herdr 自动排布");
    assert_eq!(herdr.describe(any(LayoutMode::Split)), "Herdr 分屏");
    assert_eq!(rex.describe(any(LayoutMode::Auto)), "Rex 自动排布");
    assert_eq!(rex.describe(any(LayoutMode::Tab)), "Rex 新标签页");
    assert_eq!(
        registry
            .target(Selection::Current, &ctx)
            .unwrap()
            .describe(any(LayoutMode::Split)),
        "当前终端"
    );
}

#[cfg(unix)]
#[test]
fn rex_routes_inspect_then_adaptive_launch_and_keeps_failure_surfaces() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("usage-rex-adaptive-{}", uuid::Uuid::new_v4()));
    let cwd = root.join("project ' with spaces");
    std::fs::create_dir_all(&cwd).unwrap();
    let rex = root.join("rex");
    let capture = root.join("calls.jsonl");
    for scenario in [
        "success",
        "inspect-failure",
        "invalid-json",
        "launch-failure",
    ] {
        std::fs::write(
            &rex,
            format!(
                r#"#!/usr/bin/env python3
import json,os,sys
args=sys.argv[1:]
with open({capture:?},'a') as f: f.write(json.dumps(args)+'\n')
if args[0]=='session':
    if {scenario:?}=='inspect-failure': print('inspect failed',file=sys.stderr);sys.exit(3)
    if {scenario:?}=='invalid-json': print('bad json');sys.exit(0)
    print(json.dumps({{"windows":[{{"window_id":"w0","layers":[{{"blocks":[{{"block_id":"block:usage"}},{{"block_id":"block:agent"}}]}}]}}]}}))
elif args[0]=='run':
    if {scenario:?}=='launch-failure': print('run failed',file=sys.stderr);sys.exit(4)
    print(json.dumps({{"block_ids":["block:new"],"window_id":"w1"}}))
"#,
                capture = capture.to_str().unwrap()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&rex, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&capture, "").unwrap();
        let ctx = context(&[
            ("REX_SESSION", "session:fixed"),
            ("REX_BIN_DIR", root.to_str().unwrap()),
            ("REX_BLOCK", "block:usage"),
        ])
        .with_size(200, 50);
        let result = builtin_registry()
            .prepare(
                Selection::Auto,
                &ctx,
                &TerminalRequest {
                    cwd: &cwd,
                    shell_command: "usage codex run $(literal)",
                    reuse_key: None,
                    options: LaunchOptions {
                        focus: FocusMode::Keep,
                        ..Default::default()
                    },
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
        assert_eq!(
            calls.len(),
            if scenario == "success" || scenario == "launch-failure" {
                2
            } else {
                1
            },
            "{scenario}: {calls:?} {result:?}"
        );
        if scenario == "success" {
            let launch: Vec<String> = calls
                .iter()
                .filter_map(|call| call.as_array())
                .find(|args| args.first().and_then(serde_json::Value::as_str) == Some("run"))
                .map(|args| {
                    args.iter()
                        .map(|arg| arg.as_str().unwrap_or_default().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            assert!(
                launch.windows(2).any(|p| {
                    let pair: Vec<&str> = p.iter().map(String::as_str).collect();
                    pair == ["--block", "block:usage"] || pair == ["--block", "block:agent"]
                }),
                "{launch:?}"
            );
            assert!(
                launch
                    .windows(2)
                    .any(|p| p.iter().map(String::as_str).collect::<Vec<_>>()
                        == ["--split", "right"]),
                "{launch:?}"
            );
            assert!(launch.iter().any(|a| a == "--focus=false"), "{launch:?}");
            assert_eq!(
                launch[launch.len() - 3..],
                ["sh", "-lc", "usage codex run $(literal)"]
            );
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn ghostty_observes_the_anchor_tty_then_splits_the_chosen_pane() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("usage-ghostty-fake-{}", uuid::Uuid::new_v4()));
    let cwd = root.join("project ' with spaces");
    std::fs::create_dir_all(&cwd).unwrap();
    let capture = root.join("calls");
    let osascript = root.join("osascript");
    std::fs::write(
        &osascript,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$@" >> {capture:?}
if printf '%s' "$2" | grep -q 'ps -o tty='; then
  printf 'w1|t1|term-usage|true\nw1|t1|term-editor|false\n'
  exit 0
else
  printf 'term-new|t-new\n'
fi
"#,
            capture = capture.to_str().unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&osascript, std::fs::Permissions::from_mode(0o700)).unwrap();
    let ctx = context(&[
        ("TERM_PROGRAM", "ghostty"),
        ("PATH", &format!("{}:/usr/bin:/bin", root.display())),
    ])
    .with_size(200, 50);
    let result = builtin_registry()
        .prepare(
            Selection::Auto,
            &ctx,
            &TerminalRequest {
                cwd: &cwd,
                shell_command: "usage codex run $(literal)",
                reuse_key: None,
                options: LaunchOptions {
                    focus: FocusMode::Keep,
                    ..Default::default()
                },
            },
        )
        .unwrap()
        .execute();
    assert!(result.is_ok(), "{result:?}");
    let calls = std::fs::read_to_string(&capture).unwrap();
    assert!(calls.contains("ps -o tty="), "{calls}");
    assert!(
        calls.contains("split targetTerm direction right"),
        "{calls}"
    );
    assert!(calls.contains(&cwd.canonicalize().unwrap().display().to_string()));
    assert!(calls.contains("usage codex run $(literal)"));
    assert!(calls.contains("keep"));
    std::fs::remove_dir_all(root).unwrap();
}
