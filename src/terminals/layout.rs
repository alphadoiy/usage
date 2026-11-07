//! Terminal-independent placement of a new instance among existing panes and tabs.
//! Pure: adapters supply a `Topology` read from their terminal, and execute the `Action`.
use crate::config::LayoutMode;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A split must leave both halves at least this big, so Agent TUIs stay readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Limits {
    pub min_cols: u32,
    pub min_rows: u32,
    /// Panes per tab in `auto`; more instances overflow into a tab of their own.
    pub max_panes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            min_cols: 70,
            min_rows: 16,
            max_panes: 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Pane {
    pub id: String,
    pub cols: u32,
    pub rows: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Tab {
    pub id: String,
    /// Project key stamped on tabs that Usage created for overflow.
    pub group: Option<String>,
    pub panes: Vec<Pane>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Topology {
    pub tabs: Vec<Tab>,
    /// The tab that contains Usage.
    pub current: usize,
    /// The pane running Usage; ties prefer other panes so the dashboard stays whole.
    pub anchor: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    /// Side by side.
    Right,
    /// Stacked.
    Below,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    Split {
        tab: usize,
        pane: String,
        direction: Direction,
    },
    NewTab,
}

/// Terminal cells are about twice as tall as they are wide.
fn direction(pane: &Pane) -> Direction {
    if pane.cols >= pane.rows * 2 {
        Direction::Right
    } else {
        Direction::Below
    }
}

fn fits(pane: &Pane, direction: Direction, limits: Limits) -> bool {
    match direction {
        Direction::Right => {
            (pane.cols.saturating_sub(1)) / 2 >= limits.min_cols && pane.rows >= limits.min_rows
        }
        Direction::Below => {
            (pane.rows.saturating_sub(1)) / 2 >= limits.min_rows && pane.cols >= limits.min_cols
        }
    }
}

/// The largest pane that can be halved; the preferred axis first, then the other.
fn best_split(topology: &Topology, tab: usize, limits: Limits) -> Option<(String, Direction)> {
    topology.tabs[tab]
        .panes
        .iter()
        .enumerate()
        .filter_map(|(index, pane)| {
            let preferred = direction(pane);
            let other = match preferred {
                Direction::Right => Direction::Below,
                Direction::Below => Direction::Right,
            };
            [preferred, other]
                .into_iter()
                .find(|d| fits(pane, *d, limits))
                .map(|d| {
                    let key = (
                        u64::from(pane.cols) * u64::from(pane.rows),
                        pane.id != topology.anchor,
                        index,
                    );
                    (key, pane.id.clone(), d)
                })
        })
        .max_by_key(|(key, ..)| *key)
        .map(|(_, id, d)| (id, d))
}

/// `Tab` always opens a tab. `Split` always splits (shrinking below the limits if it must).
/// `Auto` tiles the project's overflow tab first, then Usage's tab, and only then opens a tab.
pub(super) fn decide(
    topology: &Topology,
    layout: LayoutMode,
    group: &str,
    limits: Limits,
) -> Action {
    if layout == LayoutMode::Tab {
        return Action::NewTab;
    }
    let force = layout == LayoutMode::Split;
    // An explicit split stays beside Usage; only `auto` fills a project's overflow tab.
    let own_group = (0..topology.tabs.len())
        .filter(|&i| !force && topology.tabs[i].group.as_deref() == Some(group))
        .chain(std::iter::once(topology.current));
    let mut seen = Vec::new();
    for tab in own_group {
        if seen.contains(&tab) {
            continue;
        }
        seen.push(tab);
        if !force && topology.tabs[tab].panes.len() >= limits.max_panes {
            continue;
        }
        if let Some((pane, direction)) = best_split(topology, tab, limits) {
            return Action::Split {
                tab,
                pane,
                direction,
            };
        }
    }
    if !force {
        return Action::NewTab;
    }
    let current = &topology.tabs[topology.current];
    let target = current
        .panes
        .iter()
        .max_by_key(|p| u64::from(p.cols) * u64::from(p.rows))
        .expect("a topology contains the Usage pane");
    Action::Split {
        tab: topology.current,
        pane: target.id.clone(),
        direction: direction(target),
    }
}

/// Stable, shell- and format-safe key for "this project": FNV-1a over the canonical path.
pub(super) fn group_key(cwd: &Path) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in cwd.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("p{hash:016x}")
}

/// Record what a terminal does with an action: halve the pane, the new pane takes the second
/// half; a new tab starts with one pane of `tab_size`.
pub(super) fn apply(
    topology: &mut Topology,
    action: &Action,
    new_pane: &str,
    new_tab: &str,
    group: Option<&str>,
    tab_size: (u32, u32),
) {
    match action {
        Action::Split {
            tab,
            pane: target,
            direction,
        } => {
            let panes = &mut topology.tabs[*tab].panes;
            let Some(source) = panes.iter_mut().find(|p| &p.id == target) else {
                return;
            };
            let created = match direction {
                Direction::Right => {
                    let cols = source.cols.saturating_sub(1) / 2;
                    source.cols = source.cols.saturating_sub(1 + cols);
                    Pane {
                        id: new_pane.into(),
                        cols,
                        rows: source.rows,
                    }
                }
                Direction::Below => {
                    let rows = source.rows.saturating_sub(1) / 2;
                    source.rows = source.rows.saturating_sub(1 + rows);
                    Pane {
                        id: new_pane.into(),
                        cols: source.cols,
                        rows,
                    }
                }
            };
            panes.push(created);
        }
        Action::NewTab => topology.tabs.push(Tab {
            id: new_tab.into(),
            group: group.map(str::to_owned),
            panes: vec![Pane {
                id: new_pane.into(),
                cols: tab_size.0,
                rows: tab_size.1,
            }],
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, cols: u32, rows: u32) -> Pane {
        Pane {
            id: id.into(),
            cols,
            rows,
        }
    }

    fn single(cols: u32, rows: u32) -> Topology {
        Topology {
            tabs: vec![Tab {
                id: "w0".into(),
                group: None,
                panes: vec![pane("usage", cols, rows)],
            }],
            current: 0,
            anchor: "usage".into(),
        }
    }

    fn run(topology: &mut Topology, layout: LayoutMode, group: &str, count: usize) -> Vec<Action> {
        (0..count)
            .map(|i| {
                let action = decide(topology, layout, group, Limits::default());
                let tab_id = format!("w{}", topology.tabs.len());
                let group = (layout != LayoutMode::Tab).then_some(group);
                apply(
                    topology,
                    &action,
                    &format!("a{i}"),
                    &tab_id,
                    group,
                    (200, 50),
                );
                action
            })
            .collect()
    }

    #[test]
    fn first_instance_goes_beside_usage_along_the_longer_axis() {
        let wide = decide(&single(200, 50), LayoutMode::Auto, "p", Limits::default());
        assert_eq!(
            wide,
            Action::Split {
                tab: 0,
                pane: "usage".into(),
                direction: Direction::Right
            }
        );
        // 100x60 is taller than wide in pixels (cells are twice as tall as wide).
        let tall = decide(&single(100, 60), LayoutMode::Auto, "p", Limits::default());
        assert!(matches!(
            tall,
            Action::Split {
                direction: Direction::Below,
                ..
            }
        ));
    }

    #[test]
    fn four_instances_tile_into_a_grid_then_overflow_to_one_shared_tab() {
        let mut topology = single(200, 50);
        let actions = run(&mut topology, LayoutMode::Auto, "proj", 7);
        let splits = actions
            .iter()
            .filter(|a| matches!(a, Action::Split { .. }))
            .count();
        // Usage's tab fills up to the cap; the rest share one overflow tab for the project.
        assert_eq!(topology.tabs[0].panes.len(), Limits::default().max_panes);
        assert_eq!(topology.tabs.len(), 2, "{actions:?}");
        assert_eq!(topology.tabs[1].group.as_deref(), Some("proj"));
        assert_eq!(splits, 3 + (topology.tabs[1].panes.len() - 1));
        assert_eq!(
            topology.tabs[0].panes.len() + topology.tabs[1].panes.len(),
            8
        );
        for tab in &topology.tabs {
            for pane in &tab.panes {
                assert!(pane.cols >= 70 && pane.rows >= 16, "{tab:?}");
            }
        }
        // The dashboard keeps a usable share of the screen.
        assert!(topology.tabs[0].panes[0].cols >= 70);
    }

    #[test]
    fn other_projects_do_not_fill_a_project_tab_but_use_free_space_in_usage_tab() {
        let mut topology = single(200, 50);
        run(&mut topology, LayoutMode::Auto, "a", 4);
        assert_eq!(topology.tabs.len(), 2);
        // Project b has no tab: Usage's is full, so it gets its own.
        let action = decide(&topology, LayoutMode::Auto, "b", Limits::default());
        assert_eq!(action, Action::NewTab);
        // Project a continues in its tab.
        let action = decide(&topology, LayoutMode::Auto, "a", Limits::default());
        assert!(matches!(action, Action::Split { tab: 1, .. }), "{action:?}");
    }

    #[test]
    fn small_windows_open_a_tab_instead_of_cramming_panes() {
        for (cols, rows) in [(120, 30), (80, 24), (60, 20), (0, 0)] {
            let action = decide(
                &single(cols, rows),
                LayoutMode::Auto,
                "p",
                Limits::default(),
            );
            assert_eq!(action, Action::NewTab, "{cols}x{rows}");
        }
    }

    #[test]
    fn explicit_tab_always_opens_a_tab_and_explicit_split_always_splits() {
        let big = single(300, 80);
        assert_eq!(
            decide(&big, LayoutMode::Tab, "p", Limits::default()),
            Action::NewTab
        );
        let small = single(60, 20);
        assert!(matches!(
            decide(&small, LayoutMode::Split, "p", Limits::default()),
            Action::Split { .. }
        ));
        // Even with an overflow tab for the project, a split stays in Usage's tab.
        let mut with_overflow = single(200, 50);
        run(&mut with_overflow, LayoutMode::Auto, "p", 4);
        let action = decide(&with_overflow, LayoutMode::Split, "p", Limits::default());
        assert!(matches!(action, Action::Split { tab: 0, .. }), "{action:?}");
        let mut crowded = single(400, 100);
        run(&mut crowded, LayoutMode::Split, "p", 6);
        assert_eq!(crowded.tabs.len(), 1, "explicit split ignores the pane cap");
        assert_eq!(crowded.tabs[0].panes.len(), 7);
    }

    #[test]
    fn ties_leave_the_dashboard_whole_and_other_panes_are_respected() {
        let topology = Topology {
            tabs: vec![Tab {
                id: "w".into(),
                group: None,
                panes: vec![pane("usage", 100, 50), pane("editor", 100, 50)],
            }],
            current: 0,
            anchor: "usage".into(),
        };
        let action = decide(&topology, LayoutMode::Auto, "p", Limits::default());
        assert!(
            matches!(&action, Action::Split { pane, .. } if pane == "editor"),
            "{action:?}"
        );
    }
}
