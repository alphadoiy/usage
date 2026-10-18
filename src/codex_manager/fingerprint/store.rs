//! Account-scoped settings, immutable evidence and cached history reads.
use super::*;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;
use std::time::SystemTime;
pub(super) fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    private_write(
        path,
        &serde_json::to_vec_pretty(value).map_err(|_| Error::message("无法编码指纹报告"))?,
    )
}
pub(super) fn read_report(path: &Path) -> Result<Option<Report>> {
    match fs::File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 64 * 1024 {
                return Err(Error::message("指纹状态文件过大"));
            }
            let mut report: Report =
                serde_json::from_slice(&bytes).map_err(|_| Error::message("指纹状态文件无效"))?;
            if report.status == ReportStatus::Failed && report.http_status.is_none() {
                codex::failure_details(&mut report);
            }
            Ok(Some(report))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub(super) fn prune(path: &Path, keep: &Path) -> Result<()> {
    let mut runs = fs::read_dir(path)?
        .filter_map(std::result::Result::ok)
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_dir())
                && e.path() != keep
                && e.file_name()
                    .to_str()
                    .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
        })
        .collect::<Vec<_>>();
    runs.sort_by_key(|e| std::cmp::Reverse(e.metadata().and_then(|m| m.modified()).ok()));
    for entry in runs.into_iter().skip(RETAIN - 1) {
        fs::remove_dir_all(entry.path())?;
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Settings {
    pub fpd: PathBuf,
}
type Stamp = Option<(SystemTime, u64)>;
fn stamp(path: &Path) -> Stamp {
    fs::metadata(path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())))
}
struct CachedHistory {
    version: (Stamp, Stamp),
    reports: Vec<Report>,
    warnings: Vec<String>,
}
pub(super) struct Store {
    root: PathBuf,
    history: Mutex<HashMap<String, CachedHistory>>,
    #[cfg(test)]
    pub history_loads: std::sync::atomic::AtomicUsize,
}
impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            history: Mutex::new(HashMap::new()),
            #[cfg(test)]
            history_loads: Default::default(),
        }
    }
    pub fn account_dir(&self, id: &str) -> PathBuf {
        self.root.join("fingerprint/accounts").join(id)
    }
    pub fn settings(&self, id: &str) -> Result<Option<Settings>> {
        match fs::read(self.account_dir(id).join("schedule.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| Error::message("应用内测试配置无效")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn enable(&self, id: &str, fpd: PathBuf) -> Result<()> {
        let base = self.account_dir(id);
        private_dir(&base)?;
        save(&base.join("schedule.json"), &Settings { fpd })
    }
    pub fn disable(&self, id: &str) -> Result<()> {
        match fs::remove_file(self.account_dir(id).join("schedule.json")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn latest(&self, id: &str) -> Result<Option<Report>> {
        read_report(&self.account_dir(id).join("latest.json"))
    }
    pub fn snapshot(&self, profile: &super::super::Profile) -> Option<Snapshot> {
        let base = self.account_dir(&profile.id);
        if !base.is_dir() {
            return None;
        }
        let mut warnings = Vec::new();
        let enabled = match self.settings(&profile.id) {
            Ok(value) => value.is_some(),
            Err(_) => {
                warnings.push("定时测试配置损坏，请重新启用".into());
                false
            }
        };
        let latest = match self.latest(&profile.id) {
            Ok(value) => value,
            Err(_) => {
                warnings.push("最近测试记录读取失败".into());
                None
            }
        };
        // Run reports are immutable. A run directory or latest report change
        // invalidates the cache; idle UI refreshes don't decode all history again.
        let version = (stamp(&base.join("latest.json")), stamp(&base.join("runs")));
        let mut cache = self.history.lock().expect("fingerprint history cache");
        let cached = cache
            .entry(profile.id.clone())
            .or_insert_with(|| CachedHistory {
                version: (None, None),
                reports: Vec::new(),
                warnings: Vec::new(),
            });
        if cached.version != version {
            #[cfg(test)]
            self.history_loads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut reports = Vec::new();
            let mut damaged = false;
            if let Ok(entries) = fs::read_dir(base.join("runs")) {
                for entry in entries.flatten().take(1000) {
                    if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                        continue;
                    }
                    match read_report(&entry.path().join("report.json")) {
                        Ok(Some(report)) => reports.push(report),
                        Ok(None) => {}
                        Err(_) => damaged = true,
                    }
                }
            }
            reports.sort_by_key(|r| std::cmp::Reverse(r.started_at));
            reports.truncate(24);
            reports.reverse();
            *cached = CachedHistory {
                version,
                reports,
                warnings: if damaged {
                    vec!["部分历史测试记录损坏，已跳过".into()]
                } else {
                    Vec::new()
                },
            };
        }
        warnings.extend(cached.warnings.clone());
        Some(Snapshot {
            id: profile.id.clone(),
            name: profile.name.clone(),
            enabled,
            latest,
            history: cached.reports.clone(),
            warnings,
        })
    }
    pub fn retain_accounts(&self, ids: &std::collections::HashSet<String>) {
        self.history
            .lock()
            .expect("fingerprint history cache")
            .retain(|id, _| ids.contains(id));
    }
}
