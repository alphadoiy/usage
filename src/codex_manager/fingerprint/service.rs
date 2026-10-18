//! Application service: account binding, cadence and independent test operations.
use super::*;
use std::sync::atomic::AtomicBool;

pub(crate) struct FingerprintService {
    pub(super) root: PathBuf,
    pub(super) store: store::Store,
}
impl FingerprintService {
    pub fn new(root: PathBuf) -> Self {
        Self {
            store: store::Store::new(root.clone()),
            root,
        }
    }
    pub fn from_environment() -> Self {
        Self::new(super::super::default_root())
    }
    pub fn run(
        &self,
        selector: &str,
        due: bool,
        fpd: Option<&Path>,
        cancel: &AtomicBool,
    ) -> Result<Option<Report>> {
        self.run_with_manager(
            &mut Manager::open(self.root.clone())?,
            selector,
            due,
            fpd,
            cancel,
        )
    }
    pub fn enable(&self, selector: &str, fpd: Option<&Path>) -> Result<String> {
        let manager = Manager::open(self.root.clone())?;
        let profile = account(&manager, selector)?;
        self.store
            .enable(&profile.id, codex::resolve_fpd(&manager, fpd)?)?;
        Ok(profile.name)
    }
    pub fn disable(&self, selector: &str) -> Result<()> {
        let manager = Manager::open(self.root.clone())?;
        let profile = account(&manager, selector)?;
        self.store.disable(&profile.id)
    }
    pub fn status(&self, selector: &str) -> Result<Snapshot> {
        let manager = Manager::open_read_only(self.root.clone())?
            .ok_or_else(|| Error::message("账号库不存在"))?;
        let profile = account(&manager, selector)?;
        Ok(self.store.snapshot(&profile).unwrap_or(Snapshot {
            id: profile.id,
            name: profile.name,
            ..Default::default()
        }))
    }
    pub fn snapshots(&self) -> Result<Vec<Snapshot>> {
        let Some(manager) = Manager::open_read_only(self.root.clone())? else {
            return Ok(Vec::new());
        };
        let profiles = manager.list()?;
        self.store
            .retain_accounts(&profiles.iter().map(|p| p.id.clone()).collect());
        Ok(profiles
            .iter()
            .filter(|p| p.auth_mode == AuthMode::Oauth)
            .filter_map(|p| self.store.snapshot(p))
            .collect())
    }
    pub(super) fn jobs(&self) -> Result<Vec<String>> {
        if !self.root.join("fingerprint/accounts").is_dir() {
            return Ok(Vec::new());
        }
        let Some(manager) = Manager::open_read_only(self.root.clone())? else {
            return Ok(Vec::new());
        };
        let mut jobs = Vec::new();
        for profile in manager
            .list()?
            .into_iter()
            .filter(|p| p.auth_mode == AuthMode::Oauth)
        {
            match self.store.settings(&profile.id) {
                Ok(Some(_)) => jobs.push(profile.id),
                Ok(None) => {}
                Err(_) => tracing::warn!(
                    event = "fingerprint_schedule_failed",
                    provider_id = profile.id,
                    reason = "invalid_account_settings"
                ),
            }
        }
        Ok(jobs)
    }
    pub(super) fn run_scheduled(&self, id: &str, cancel: &AtomicBool) -> Result<Option<Report>> {
        // Recheck settings when the bounded queue dispatches: disabled accounts
        // must not start a stale planned job.
        let Some(settings) = self.store.settings(id)? else {
            return Ok(None);
        };
        self.run(id, true, Some(&settings.fpd), cancel)
    }
    fn run_with_manager(
        &self,
        manager: &mut Manager,
        selector: &str,
        due: bool,
        fpd: Option<&Path>,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<Report>> {
        use std::sync::atomic::Ordering;
        if cancel.load(Ordering::Acquire) {
            return Ok(None);
        }
        let profile = account(manager, selector)?;
        let base = self.store.account_dir(&profile.id);
        private_dir(&base)?;
        let lock_path = base.join("run.lock");
        super::super::private_create(&lock_path, b"")?;
        let lock = fs::File::open(lock_path)?;
        if lock.try_lock_exclusive().is_err() {
            return Ok(None);
        }
        let latest = base.join("latest.json");
        let now = chrono::Utc::now().timestamp();
        if due
            && store::read_report(&latest)?
                .is_some_and(|r| now.saturating_sub(r.started_at) < INTERVAL)
        {
            return Ok(None);
        }
        let fpd = codex::resolve_fpd(manager, fpd)?;
        private_dir(&base.join("runs"))?;
        let path = base.join("runs").join(uuid::Uuid::new_v4().to_string());
        private_dir(&path)?;
        let mut report = Report {
            http_status: None,
            quota_resets_at: None,
            model: MODEL.into(),
            channel: "codex-subscription".into(),
            started_at: now,
            finished_at: None,
            status: ReportStatus::Running,
            directory: path.clone(),
            accepted: 0,
            reference_contains_model: false,
            closest_candidate: None,
            message: "正在采集；若进程意外退出，下次定时任务至少间隔一小时再尝试。".into(),
        };
        store::save(&latest, &report)?;
        tracing::info!(
            event = "fingerprint_started",
            provider_id = profile.id,
            model = MODEL,
            samples = 3
        );
        let result = codex::CodexSubscriptionDetector
            .detect(manager, &profile, &path, &fpd, cancel)
            .and_then(|value| codex::summarize(&mut report, &value));
        if let Err(error) = result {
            report.status = ReportStatus::Failed;
            report.message = error.to_string();
            codex::failure_details(&mut report);
        }
        if cancel.load(Ordering::Acquire) {
            report.status = ReportStatus::Interrupted;
            report.message = "Usage 已退出，本轮测试已停止；重新打开后按到期时间检查。".into();
        }
        report.finished_at = Some(chrono::Utc::now().timestamp());
        store::save(&path.join("report.json"), &report)?;
        store::save(&latest, &report)?;
        if store::prune(&base.join("runs"), &path).is_err() {
            tracing::warn!(event = "fingerprint_cleanup_failed");
        }
        tracing::info!(
            event = "fingerprint_finished",
            provider_id = profile.id,
            model = MODEL,
            status = %report.status,
            accepted = report.accepted,
            reference_contains_model = report.reference_contains_model,
            closest_candidate = report.closest_candidate,
            http_status = report.http_status,
            quota_resets_at = report.quota_resets_at
        );
        Ok(Some(report))
    }
}
