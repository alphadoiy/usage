//! Projects are directories Agents run in. Manual entries persist in their own store; others
//! derive from local sessions when read, and are stored only once the user pins, hides or
//! launches in them. Session discovery stays independent: a directory never proves Run ownership.
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub path: PathBuf,
    pub manual: bool,
    pub pinned: bool,
    pub hidden: bool,
    pub added_at_ms: i64,
    pub launched_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Project {
    pub path: PathBuf,
    pub manual: bool,
    pub pinned: bool,
    /// Latest of session activity, launches from usage, and when a manual project was added.
    pub recency_ms: i64,
    /// A stored project whose directory is gone stays listed until the user removes it.
    pub missing: bool,
}

/// Session directories are compared as recorded; Agents record their physical working directory
/// and manual additions are canonicalized, so the two agree without resolving every session.
pub fn catalog<'a>(
    records: &[Record],
    sessions: impl IntoIterator<Item = (&'a Path, i64)>,
) -> Vec<Project> {
    let mut projects: HashMap<PathBuf, Project> = HashMap::new();
    for (cwd, at) in sessions {
        projects
            .entry(cwd.to_path_buf())
            .and_modify(|p| p.recency_ms = p.recency_ms.max(at))
            .or_insert_with(|| Project {
                path: cwd.to_path_buf(),
                manual: false,
                pinned: false,
                recency_ms: at,
                missing: false,
            });
    }
    for record in records {
        if record.hidden {
            projects.remove(&record.path);
            continue;
        }
        if !(record.manual || record.pinned || projects.contains_key(&record.path)) {
            continue;
        }
        let project = projects
            .entry(record.path.clone())
            .or_insert_with(|| Project {
                path: record.path.clone(),
                manual: false,
                pinned: false,
                recency_ms: record.added_at_ms,
                missing: !record.path.is_dir(),
            });
        project.manual = record.manual;
        project.pinned = record.pinned;
        project.recency_ms = project
            .recency_ms
            .max(record.launched_at_ms.unwrap_or(i64::MIN));
    }
    let mut projects: Vec<_> = projects.into_values().collect();
    projects.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| b.recency_ms.cmp(&a.recency_ms))
            .then_with(|| a.path.cmp(&b.path))
    });
    projects
}

#[derive(Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }
    pub fn from_environment() -> Self {
        Self::at(crate::paths::data_dir().join("projects.sqlite3"))
    }

    fn open(&self) -> Result<Connection, String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|_| "无法创建项目目录库")?;
        }
        let db = Connection::open(&self.path).map_err(|_| "无法打开项目库")?;
        db.busy_timeout(Duration::from_millis(250))
            .map_err(|_| "项目库暂不可用")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))
                .map_err(|_| "无法设置项目库权限")?;
        }
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS projects(
                path TEXT PRIMARY KEY,
                manual INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                hidden INTEGER NOT NULL DEFAULT 0,
                added_at INTEGER NOT NULL,
                launched_at INTEGER
            );",
        )
        .map_err(|_| "项目库初始化失败")?;
        Ok(db)
    }

    /// Reading never creates the store, so browsing leaves no file behind.
    pub fn list(&self) -> Result<Vec<Record>, String> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let db = self.open()?;
        let mut statement = db
            .prepare(
                "SELECT path,manual,pinned,hidden,added_at,launched_at FROM projects ORDER BY path",
            )
            .map_err(|_| "项目库读取失败")?;
        statement
            .query_map([], |row| {
                Ok(Record {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    manual: row.get(1)?,
                    pinned: row.get(2)?,
                    hidden: row.get(3)?,
                    added_at_ms: row.get(4)?,
                    launched_at_ms: row.get(5)?,
                })
            })
            .and_then(|rows| rows.collect())
            .map_err(|_| "项目库读取失败".into())
    }

    /// Adding an existing or hidden project makes it manual and visible again.
    pub fn add(&self, path: &Path) -> Result<PathBuf, String> {
        let path = path
            .canonicalize()
            .ok()
            .filter(|p| p.is_dir())
            .ok_or("项目目录不存在")?;
        self.upsert(&path, "manual=1,hidden=0")?;
        Ok(path)
    }
    /// Removing hides the directory from every project list, even while it still has sessions.
    pub fn remove(&self, path: &Path) -> Result<(), String> {
        self.upsert(path, "manual=0,pinned=0,hidden=1")
    }
    pub fn set_pinned(&self, path: &Path, pinned: bool) -> Result<(), String> {
        self.upsert(
            path,
            if pinned {
                "pinned=1,hidden=0"
            } else {
                "pinned=0"
            },
        )
    }
    pub fn record_launch(&self, path: &Path) -> Result<(), String> {
        self.upsert(path, &format!("launched_at={},hidden=0", now_ms()))
    }

    fn upsert(&self, path: &Path, assignments: &str) -> Result<(), String> {
        let key = path.to_str().ok_or("项目路径不是有效的 UTF-8")?;
        let mut db = self.open()?;
        let tx = db.transaction().map_err(|_| "项目库写入失败")?;
        tx.execute(
            "INSERT INTO projects(path,added_at) VALUES(?1,?2) ON CONFLICT(path) DO NOTHING",
            params![key, now_ms()],
        )
        .and_then(|_| {
            tx.execute(
                &format!("UPDATE projects SET {assignments} WHERE path=?1"),
                params![key],
            )
        })
        .map_err(|_| "项目库写入失败")?;
        tx.commit().map_err(|_| "项目库写入失败".into())
    }
}

#[derive(clap::Subcommand)]
pub enum ProjectCommand {
    /// 列出项目：手动添加的目录与本机会话所在目录
    List {
        #[arg(long)]
        json: bool,
    },
    /// 手动添加项目目录；也会恢复已移出的项目
    Add { path: PathBuf },
    /// 移出项目列表；会话与目录保留
    Remove { path: PathBuf },
    /// 置顶项目
    Pin { path: PathBuf },
    /// 取消置顶
    Unpin { path: PathBuf },
}

/// Relative paths resolve against the current directory; a missing directory is kept as written.
fn resolve(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub fn execute(command: ProjectCommand, config: Option<&Path>) -> Result<(), String> {
    let store = Store::from_environment();
    match command {
        ProjectCommand::List { json } => {
            let path = config
                .map(Path::to_path_buf)
                .unwrap_or_else(crate::config::default_config_path);
            let config = crate::config::load_config(&path, config.is_some())
                .map_err(|error| error.to_string())?;
            let sessions = crate::sessions::builtin_registry()
                .load_all(&crate::sessions::SessionHomes::from_config(&config))
                .sessions;
            let projects = catalog(
                &store.list()?,
                sessions.iter().map(|s| (s.cwd.as_path(), s.recency_at_ms)),
            );
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&projects).map_err(|_| "项目列表序列化失败")?
                );
                return Ok(());
            }
            for project in &projects {
                let flags: Vec<_> = [
                    (project.pinned, "置顶"),
                    (project.manual, "手动"),
                    (project.missing, "目录不存在"),
                ]
                .into_iter()
                .filter_map(|(set, flag)| set.then_some(flag))
                .collect();
                if flags.is_empty() {
                    println!("{}", project.path.display());
                } else {
                    println!("{}  [{}]", project.path.display(), flags.join("·"));
                }
            }
        }
        ProjectCommand::Add { path } => {
            println!("已添加：{}", store.add(&path)?.display());
        }
        ProjectCommand::Remove { path } => store.remove(&resolve(&path))?,
        ProjectCommand::Pin { path } => store.set_pinned(&resolve(&path), true)?,
        ProjectCommand::Unpin { path } => store.set_pinned(&resolve(&path), false)?,
    }
    Ok(())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("usage-projects-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }
        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }
        fn store(&self) -> Store {
            Store::at(self.0.join("data/projects.sqlite3"))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn manual_projects_list_without_sessions_and_derived_ones_need_no_row() {
        let f = Fixture::new();
        let store = f.store();
        assert!(store.list().unwrap().is_empty());
        assert!(
            !f.0.join("data").exists(),
            "reading must not create the store"
        );
        let fresh = store.add(&f.dir("fresh")).unwrap();
        let old = f.dir("old");
        let projects = catalog(&store.list().unwrap(), [(old.as_path(), 1)]);
        assert_eq!(
            projects.iter().map(|p| &p.path).collect::<Vec<_>>(),
            [&fresh, &old],
            "a just-added project is newer than old session activity"
        );
        assert!(projects[0].manual && !projects[1].manual);
        assert_eq!(
            store.list().unwrap().len(),
            1,
            "derived projects are not stored"
        );
        assert!(store.add(&f.0.join("missing")).is_err());
    }

    #[test]
    fn pinning_hiding_and_launching_reorder_or_remove_projects() {
        let f = Fixture::new();
        let store = f.store();
        let (a, b, c) = (f.dir("a"), f.dir("b"), f.dir("c"));
        let sessions = [(a.as_path(), 30), (b.as_path(), 20), (c.as_path(), 10)];
        store.set_pinned(&c, true).unwrap();
        store.remove(&a).unwrap();
        let projects = catalog(&store.list().unwrap(), sessions);
        assert_eq!(
            projects.iter().map(|p| &p.path).collect::<Vec<_>>(),
            [&c, &b]
        );
        assert!(projects[0].pinned);
        // Re-adding a hidden project restores it; launching counts as recent activity.
        store.add(&a).unwrap();
        store.set_pinned(&c, false).unwrap();
        store.record_launch(&b).unwrap();
        let projects = catalog(&store.list().unwrap(), sessions);
        assert_eq!(projects[0].path, b);
        assert!(projects.iter().any(|p| p.path == a && p.manual));
        // A stored project whose directory disappears is kept and marked.
        fs::remove_dir(&c).unwrap();
        store.set_pinned(&c, true).unwrap();
        let projects = catalog(&store.list().unwrap(), [(b.as_path(), 20)]);
        assert!(projects[0].missing && projects[0].path == c);
    }
}
