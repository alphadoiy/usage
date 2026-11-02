use super::*;
use crate::provider_catalog::{ProviderKind, ProviderUsage, UsageSource};
use crate::resources::{ResourceBinding, ResourceScope, ResourceSnapshot};

fn fixture() -> Provider {
    let binding = ResourceBinding {
        account: "fixture".into(),
        revision: "1".into(),
    };
    let metrics = (0..20)
        .map(|n| ResourceMetric::Account {
            account: ResourceAccount {
                id: format!("id-{n}"),
                provider: format!("vendor-{n}"),
                name: format!("服务商 {n:02}"),
                user: Some("long-account-identifier-that-must-not-wrap-in-list".into()),
                status: AccountStatus::Ready,
                metrics: vec![ResourceMetric::AllowanceWindow {
                    name: "Credits".into(),
                    used: "20".into(),
                    limit: "100".into(),
                    remaining: "80".into(),
                    unit: "credits".into(),
                    resets_at: None,
                }],
            },
        })
        .collect();
    Provider {
        id: "magpie".into(),
        name: "Magpie".into(),
        agent: Some("Codex".into()),
        kind: ProviderKind::Magpie,
        configured_model: Some("auto/deepseek-v4.1-flash".into()),
        models: Models::Ready((0..136).map(|n| format!("model-{n:03}")).collect()),
        usage: ProviderUsage {
            sources: vec![UsageSource {
                target: "fixture".into(),
                label: "Magpie".into(),
                service: "magpie_quotas".into(),
                binding: binding.clone(),
                state: QueryState::Result(ResourceSnapshot {
                    binding,
                    service: "magpie_quotas".into(),
                    scope: ResourceScope::Unspecified,
                    state: ResourceState::Ready,
                    metrics,
                    queried_at: None,
                    message: None,
                }),
            }],
            ..Default::default()
        },
        operations: vec![],
        performance: None,
    }
}
fn rendered(
    provider: &Provider,
    state: &State,
    width: u16,
    height: u16,
    models: bool,
) -> Vec<String> {
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
    render(buf.area, &mut buf, provider, state, models, 0);
    (0..height)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0;
            while x < width {
                let symbol = buf[(x, y)].symbol();
                row.push_str(symbol);
                x += symbol.width().max(1) as u16;
            }
            row
        })
        .collect()
}
#[test]
fn account_viewport_tracks_selection_and_keeps_summary_and_model_footer_fixed() {
    let p = fixture();
    let mut state = State::default();
    for _ in 0..19 {
        state.key(KeyCode::Down, &p, (76, 20));
    }
    for width in [32, 48, 76, 100] {
        let rows = rendered(&p, &state, width, 20, false);
        assert!(rows[0].contains("Magpie"));
        assert!(rows[1].contains("今日用量"));
        assert!(rows[18].contains("模型"));
        assert!(
            rows.iter().any(|r| r.contains("服务商 19")),
            "{}",
            rows.join("\n")
        );
        assert!(!rows[4..10].join("\n").contains("long-account"));
    }
    state.key(KeyCode::Enter, &p, (76, 20));
    let rows = rendered(&p, &state, 76, 20, false);
    assert!(!rows.join("\n").contains("long-account"));
    assert!(rows[18].contains("136 个"));
    for h in 0..10 {
        rendered(&p, &state, 20, h, false);
    }
}
#[test]
fn selection_survives_refresh_reordering_and_recovers_from_account_removal() {
    let mut p = fixture();
    let mut state = State::default();
    state.key(KeyCode::Down, &p, (76, 20));
    state.key(KeyCode::Down, &p, (76, 20));
    if let QueryState::Result(s) = &mut p.usage.sources[0].state {
        s.metrics.reverse();
    }
    state.reconcile(&p);
    assert_eq!(accounts(&p)[state.index(&accounts(&p))].id, "id-2");
    if let QueryState::Result(s) = &mut p.usage.sources[0].state {
        s.metrics
            .retain(|m| !matches!(m,ResourceMetric::Account {account} if account.id=="id-2"));
    }
    state.reconcile(&p);
    assert_eq!(accounts(&p)[state.index(&accounts(&p))].id, "id-19");
}
#[test]
fn statuses_and_vendor_totals_are_not_invented_or_added_together() {
    let p = fixture();
    let mut account = accounts(&p)[0].clone();
    account.status = AccountStatus::Failed;
    assert_eq!(amount(&account).0, "查询失败");
    account.status = AccountStatus::Pending;
    assert_eq!(amount(&account).0, "待查询");
    account.status = AccountStatus::Ready;
    account.metrics = vec![
        ResourceMetric::AllowanceWindow {
            name: "总积分".into(),
            used: "0".into(),
            limit: "4500".into(),
            remaining: "4500".into(),
            unit: "credits".into(),
            resets_at: None,
        },
        ResourceMetric::Allowance {
            name: Some("基础积分包".into()),
            remaining: Some("500".into()),
            used: None,
            unit: "credits".into(),
            unlimited: false,
        },
        ResourceMetric::Allowance {
            name: Some("赠送积分包".into()),
            remaining: Some("4000".into()),
            used: None,
            unit: "credits".into(),
            unlimited: false,
        },
    ];
    assert_eq!(amount(&account).0, "4,500 credits · +2项");
    let detail = details(&account, 80)
        .iter()
        .map(ToString::to_string)
        .collect::<String>();
    assert!(detail.contains("500 credits") && detail.contains("4,000 credits"));
    assert!(!detail.contains("4,500") && !detail.contains("9,000"));
    assert!(
        rendered(&p, &State::default(), 76, 20, true)
            .join("\n")
            .contains("model-000")
    );
}

fn window(name: &str, remaining: &str, limit: &str, used: &str, unit: &str) -> ResourceMetric {
    ResourceMetric::AllowanceWindow {
        name: name.into(),
        remaining: remaining.into(),
        limit: limit.into(),
        used: used.into(),
        unit: unit.into(),
        resets_at: None,
    }
}

fn representative() -> Provider {
    let mut p = fixture();
    let rows = [
        (
            "WorkBuddy",
            "三年",
            vec![
                window("Credits", "500", "500", "0", "credits"),
                window("Extra credits", "5990.6", "7600", "1609.4", "credits"),
            ],
        ),
        (
            "Trae CN",
            "用户 4544292619",
            vec![window("Credits", "1296.39", "1300", "3.61", "credits")],
        ),
        (
            "CodeArts",
            "iam::55f01cbfca074bb38ddafbf42d301f:user:fixture_87frzn29bqbktkz",
            vec![
                window("总积分", "4500", "4500", "0", "credits"),
                window("基础积分包", "500", "500", "0", "credits"),
                window("赠送积分包", "4000", "4000", "0", "credits"),
            ],
        ),
        (
            "AutoClaw",
            "Eason Young",
            vec![
                ResourceMetric::Allowance {
                    name: Some("积分余额".into()),
                    remaining: Some("14000".into()),
                    used: None,
                    unit: "积分".into(),
                    unlimited: false,
                },
                ResourceMetric::ReportedBalance {
                    name: "余额".into(),
                    display: "14000 积分".into(),
                },
            ],
        ),
        (
            "Qoder CN",
            "01a0facc-5cd0-74d5-9e3f-fixture",
            vec![ResourceMetric::QuotaWindow {
                name: "Add-on credits".into(),
                used_percent: 0.0,
                resets_at: None,
            }],
        ),
        (
            "商汤小浣熊",
            "Raccoon-pu-01a19d21-1312-7530-fixture",
            vec![window("Credits", "6030", "6030", "0", "credits")],
        ),
        (
            "妙手",
            "CatPaw",
            vec![ResourceMetric::ReportedBalance {
                name: "余额".into(),
                display: "0.00 credits".into(),
            }],
        ),
    ];
    if let QueryState::Result(s) = &mut p.usage.sources[0].state {
        s.metrics = rows
            .into_iter()
            .enumerate()
            .map(|(n, (name, user, metrics))| ResourceMetric::Account {
                account: ResourceAccount {
                    id: format!("id-{n}"),
                    provider: name.into(),
                    name: name.into(),
                    user: Some(user.into()),
                    status: AccountStatus::Ready,
                    metrics,
                },
            })
            .collect();
    }
    p
}

#[test]
fn realistic_accounts_show_balances_once_and_keep_details_reachable() {
    let p = representative();
    let mut state = State::default();
    let wide = rendered(&p, &state, 76, 22, false).join("\n");
    assert!(wide.contains("5990.6") || wide.contains("5,990.6"));
    assert!(wide.contains("4,500 credits") && wide.contains("14,000 积分"));
    assert!(!wide.contains("iam::"));
    let mut paged = p.clone();
    if let QueryState::Result(s) = &mut paged.usage.sources[0].state
        && let ResourceMetric::Account { account } = &mut s.metrics[2]
    {
        account.user = Some("long-account-identifier-".repeat(8));
    }
    state.key(KeyCode::Down, &paged, (32, 18));
    state.key(KeyCode::Down, &paged, (32, 18));
    for _ in 0..50 {
        state.key(KeyCode::PageDown, &paged, (32, 18));
    }
    let bottom = state.detail_scroll;
    assert!(
        rendered(&paged, &state, 32, 18, false)
            .join("\n")
            .contains("4,000 credits")
    );
    state.key(KeyCode::PageUp, &paged, (32, 18));
    assert!(state.detail_scroll < bottom);
    // Optional exports use the actual Ratatui cells, not a separately drawn mockup.
    if let Some(path) = std::env::var_os("USAGE_MAGPIE_LAYOUT_DIR") {
        let path = std::path::PathBuf::from(path);
        std::fs::create_dir_all(&path).unwrap();
        for (name, width, height, state) in [
            ("wide", 76, 22, State::default()),
            ("narrow", 44, 22, State::default()),
            ("details", 44, 22, state),
        ] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
            render(buffer.area, &mut buffer, &p, &state, false, 0);
            let mut cells = Vec::new();
            for y in 0..height {
                let mut x = 0;
                while x < width {
                    let c = &buffer[(x, y)];
                    cells.push(serde_json::json!({"x":x,"y":y,"text":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg),"bold":c.modifier.contains(ratatui::style::Modifier::BOLD),"dim":c.modifier.contains(ratatui::style::Modifier::DIM)}));
                    x += c.symbol().width().max(1) as u16;
                }
            }
            std::fs::write(
                path.join(format!("{name}.json")),
                serde_json::to_vec(
                    &serde_json::json!({"width":width,"height":height,"cells":cells}),
                )
                .unwrap(),
            )
            .unwrap();
        }
    }
}

#[test]
fn distinct_units_and_account_suffixes_survive_compaction() {
    let mut p = representative();
    let mut a = accounts(&p)[3].clone();
    a.metrics.push(ResourceMetric::ReportedBalance {
        name: "余额".into(),
        display: "12.30 USD".into(),
    });
    let text = details(&a, 80)
        .iter()
        .map(ToString::to_string)
        .collect::<String>();
    assert!(text.contains("12.30 USD"));
    assert_eq!(text.matches("14,000").count(), 1);
    assert!(!text.contains("14000 积分"));
    a.user = Some("same-prefix-account-AAAA".into());
    let mut b = a.clone();
    b.id = "other-id".into();
    b.user = Some("same-prefix-account-BBBB".into());
    let all = [&a, &b];
    assert_ne!(label(&a, &all, 28), label(&b, &all, 28));
    assert!(label(&a, &all, 28).contains("AAAA"));
    assert!(label(&b, &all, 28).contains("BBBB"));
    // A real zero remains visible; failure is a distinct state.
    if let QueryState::Result(s) = &mut p.usage.sources[0].state
        && let ResourceMetric::Account { account } = &mut s.metrics[6]
    {
        account.status = AccountStatus::Failed;
    }
    assert!(
        rendered(&p, &State::default(), 76, 22, false)
            .join("\n")
            .contains("查询失败")
    );
}
