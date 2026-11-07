//! Merge live membership and the caller's measured size with estimated geometry.
//! Observable size or membership changes invalidate estimates. Manual resizing of
//! other panes that leaves the caller unchanged cannot be observed by these APIs.
use super::layout::{self, Action, Limits, Pane, Tab, Topology};
use crate::config::LayoutMode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(super) struct Live {
    pub tabs: Vec<LiveTab>,
    pub current: usize,
    pub anchor: String,
}
pub(super) struct LiveTab {
    pub id: String,
    pub panes: Vec<String>,
}
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct Model {
    tabs: Vec<Tab>,
    tab_size: Option<(u32, u32)>,
    #[serde(default)]
    sessions: std::collections::HashMap<String, String>,
}
impl Model {
    pub fn existing(&self, key: Option<&str>, live: &Live) -> Option<Created> {
        let pane = self.sessions.get(key?)?;
        let tab = live.tabs.iter().find(|tab| tab.panes.contains(pane))?;
        Some(Created {
            pane: pane.clone(),
            tab: Some(tab.id.clone()),
        })
    }
}
pub(super) struct Store {
    dir: PathBuf,
}
impl Store {
    pub fn default_store() -> Self {
        Self {
            dir: crate::paths::data_dir().join("layout"),
        }
    }
    fn path(&self, scope: &str) -> PathBuf {
        use sha2::{Digest, Sha256};
        let name = format!("{:x}", Sha256::digest(scope.as_bytes()));
        self.dir.join(format!("{name}.json"))
    }
    pub fn load(&self, scope: &str) -> Model {
        std::fs::read(self.path(scope))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
    pub fn save(&self, scope: &str, model: &Model) {
        let write = || -> std::io::Result<()> {
            crate::private_fs::write(
                &self.path(scope),
                &serde_json::to_vec(model).map_err(std::io::Error::other)?,
            )
        };
        if let Err(error) = write() {
            tracing::debug!(event = "layout_state_unwritable", kind = ?error.kind());
        }
    }
}
pub(super) struct Plan {
    pub topology: Topology,
    pub action: Action,
    tab_size: (u32, u32),
    sessions: std::collections::HashMap<String, String>,
}
pub(super) fn plan(
    live: &Live,
    usage: (u32, u32),
    model: &Model,
    layout: LayoutMode,
    group: &str,
) -> Plan {
    let remembered = |live_tab: &LiveTab| {
        model.tabs.iter().find(|tab| {
            tab.id == live_tab.id || tab.panes.iter().any(|p| live_tab.panes.contains(&p.id))
        })
    };
    let stable = model
        .tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .find(|pane| pane.id == live.anchor)
        .is_some_and(|pane| (pane.cols, pane.rows) == usage)
        && live.tabs.len() == model.tabs.len()
        && live.tabs.iter().all(|tab| {
            remembered(tab).is_some_and(|known| {
                tab.panes.len() == known.panes.len()
                    && tab
                        .panes
                        .iter()
                        .all(|id| known.panes.iter().any(|p| &p.id == id))
            })
        });
    let tab_size = if live.tabs[live.current].panes.len() == 1 {
        usage
    } else if stable {
        model.tab_size.unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    let tabs = live
        .tabs
        .iter()
        .map(|live_tab| {
            let known = remembered(live_tab);
            Tab {
                id: live_tab.id.clone(),
                group: known.and_then(|tab| tab.group.clone()),
                panes: live_tab
                    .panes
                    .iter()
                    .map(|id| {
                        let (cols, rows) = if *id == live.anchor {
                            usage
                        } else {
                            known
                                .filter(|_| stable)
                                .and_then(|tab| tab.panes.iter().find(|p| p.id == *id))
                                .map_or((0, 0), |p| (p.cols, p.rows))
                        };
                        Pane {
                            id: id.clone(),
                            cols,
                            rows,
                        }
                    })
                    .collect(),
            }
        })
        .collect();
    let topology = Topology {
        tabs,
        current: live.current,
        anchor: live.anchor.clone(),
    };
    let action = layout::decide(&topology, layout, group, Limits::default());
    tracing::debug!(
        event = "terminal_layout",
        placement = match action {
            Action::Split { .. } => "split",
            Action::NewTab => "tab",
        }
    );
    Plan {
        topology,
        action,
        tab_size,
        sessions: model
            .sessions
            .iter()
            .filter(|(_, pane)| live.tabs.iter().any(|tab| tab.panes.contains(pane)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}
pub(super) struct Created {
    pub pane: String,
    pub tab: Option<String>,
}
impl Plan {
    pub fn commit(
        mut self,
        store: &Store,
        scope: &str,
        created: Option<Created>,
        layout: LayoutMode,
        group: &str,
        reuse_key: Option<&str>,
    ) {
        let mut topology = self.topology;
        if let Some(created) = created {
            if let Some(key) = reuse_key {
                self.sessions.insert(key.into(), created.pane.clone());
            }
            let tab = created
                .tab
                .unwrap_or_else(|| format!("tab-{}", topology.tabs.len()));
            layout::apply(
                &mut topology,
                &self.action,
                &created.pane,
                &tab,
                (layout != LayoutMode::Tab).then_some(group),
                self.tab_size,
            );
        }
        store.save(
            scope,
            &Model {
                tabs: topology.tabs,
                tab_size: Some(self.tab_size),
                sessions: self.sessions,
            },
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn server_scopes_do_not_share_bindings_after_path_sanitization() {
        let store = Store {
            dir: "/tmp/unused-layout-test".into(),
        };
        assert_ne!(
            store.path("rex-server/a-session"),
            store.path("rex-server_a-session")
        );
    }
    fn live(panes: &[&[&str]], current: usize, anchor: &str) -> Live {
        Live {
            tabs: panes
                .iter()
                .enumerate()
                .map(|(i, p)| LiveTab {
                    id: format!("tab{i}"),
                    panes: p.iter().map(|s| (*s).into()).collect(),
                })
                .collect(),
            current,
            anchor: anchor.into(),
        }
    }
    #[test]
    fn remembered_sizes_make_the_next_launch_tile_instead_of_stacking() {
        let store = Store {
            dir: std::env::temp_dir().join(format!("usage-adaptive-{}", uuid::Uuid::new_v4())),
        };
        let mut tabs = vec![vec!["usage".to_owned()]];
        for i in 0..4 {
            let snapshot = Live {
                tabs: tabs
                    .iter()
                    .enumerate()
                    .map(|(n, p)| LiveTab {
                        id: format!("tab{n}"),
                        panes: p.clone(),
                    })
                    .collect(),
                current: 0,
                anchor: "usage".into(),
            };
            let model = store.load("scope");
            let size = model
                .tabs
                .iter()
                .flat_map(|t| &t.panes)
                .find(|p| p.id == "usage")
                .map_or((200, 50), |p| (p.cols, p.rows));
            let plan = plan(&snapshot, size, &model, LayoutMode::Auto, "g");
            let new_pane = format!("p{i}");
            match &plan.action {
                Action::Split { tab, .. } => tabs[*tab].push(new_pane.clone()),
                Action::NewTab => tabs.push(vec![new_pane.clone()]),
            }
            plan.commit(
                &store,
                "scope",
                Some(Created {
                    pane: new_pane,
                    tab: Some(format!("tab{}", tabs.len() - 1)),
                }),
                LayoutMode::Auto,
                "g",
                None,
            );
        }
        assert_eq!(tabs[0].len(), 4);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[1], ["p3"]);
        assert_eq!(store.load("scope").tabs[1].group.as_deref(), Some("g"));
        std::fs::remove_dir_all(store.dir).unwrap();
    }
    fn model() -> Model {
        Model {
            sessions: Default::default(),
            tabs: vec![Tab {
                id: "tab0".into(),
                group: None,
                panes: vec![
                    Pane {
                        id: "usage".into(),
                        cols: 100,
                        rows: 50,
                    },
                    Pane {
                        id: "agent".into(),
                        cols: 99,
                        rows: 50,
                    },
                ],
            }],
            tab_size: Some((200, 50)),
        }
    }
    #[test]
    fn resize_invalidates_other_panes_and_overflow_tab_size() {
        let plan = plan(
            &live(&[&["usage", "agent"]], 0, "usage"),
            (60, 20),
            &model(),
            LayoutMode::Auto,
            "g",
        );
        assert_eq!(plan.action, Action::NewTab);
        assert_eq!(plan.tab_size, (0, 0));
        assert_eq!(plan.topology.tabs[0].panes[1].cols, 0);
    }
    #[test]
    fn closed_panes_are_forgotten_and_foreign_panes_count_but_are_not_split() {
        let plan = plan(
            &live(&[&["usage", "foreign"]], 0, "usage"),
            (100, 50),
            &model(),
            LayoutMode::Auto,
            "g",
        );
        assert_eq!(plan.topology.tabs[0].panes.len(), 2);
        assert_eq!(plan.topology.tabs[0].panes[1].cols, 0);
        assert!(matches!(plan.action, Action::Split {pane,..} if pane == "usage"));
    }
    #[test]
    fn unreadable_or_missing_state_never_blocks_a_launch() {
        let dir = std::env::temp_dir().join(format!("usage-adaptive-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store { dir: dir.clone() };
        assert!(store.load("nothing").tabs.is_empty());
        std::fs::write(store.path("broken"), b"{ not json").unwrap();
        assert!(store.load("broken").tabs.is_empty());
        assert!(store.path("../../etc/passwd").starts_with(&dir));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
