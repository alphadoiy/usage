//! Quota transitions are based on successful official snapshots, never token estimates.
use crate::config::AlertConfig;
use crate::model::{CredentialStatus, UsageSnapshot};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    pub message: String,
    pub recovered: bool,
}
#[derive(Default)]
struct WindowState {
    high: bool,
    pending_low: bool,
    last_low_alert: Option<DateTime<Utc>>,
    last_observed: Option<DateTime<Utc>>,
}
#[derive(Default)]
pub struct QuotaMonitor {
    windows: HashMap<(String, String, String, String), WindowState>,
}
impl QuotaMonitor {
    pub fn observe(
        &mut self,
        target: &str,
        snapshot: &UsageSnapshot,
        config: &AlertConfig,
        now: DateTime<Utc>,
    ) -> Vec<Alert> {
        if !config.enabled
            || !snapshot.success
            || snapshot.transient
            || snapshot.credential_status != CredentialStatus::Valid
        {
            return Vec::new();
        }
        let observed = snapshot.queried_at.unwrap_or(now);
        let mut alerts = Vec::new();
        for tier in &snapshot.tiers {
            if !tier.utilization.is_finite() {
                continue;
            }
            let key = (
                target.to_owned(),
                snapshot.tool.clone(),
                snapshot.account_email.clone().unwrap_or_default(),
                tier.name.clone(),
            );
            let state = self.windows.entry(key).or_default();
            if state.last_observed.is_some_and(|t| observed <= t) {
                continue;
            }
            state.last_observed = Some(observed);
            let remaining = (100.0 - tier.utilization).clamp(0.0, 100.0);
            let high = remaining <= f64::from(config.remaining_percent);
            let label = crate::labels::tier_label(&tier.name);
            if high && !state.high {
                state.pending_low = true;
            } else if !high {
                state.pending_low = false;
            }
            if high
                && state.pending_low
                && state.last_low_alert.is_none_or(|t| {
                    (now - t).num_seconds().max(0) as u64 >= config.cooldown_seconds
                })
            {
                state.last_low_alert = Some(now);
                state.pending_low = false;
                alerts.push(Alert {
                    message: format!(
                        "{} · {} 剩余额度 {:.0}%",
                        snapshot.display_name, label, remaining
                    ),
                    recovered: false,
                });
            } else if !high && state.high && config.recovery {
                alerts.push(Alert {
                    message: format!(
                        "{} · {} 额度已恢复，剩余 {:.0}%",
                        snapshot.display_name, label, remaining
                    ),
                    recovered: true,
                });
            }
            state.high = high;
        }
        alerts
    }
}

/// Optional local OS notification. Arguments stay separate from AppleScript/shell source.
pub fn notify_desktop(message: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = Command::new("osascript");
        c.args([
            "-e",
            "on run argv\ndisplay notification (item 1 of argv) with title \"usage\"\nend run",
        ])
        .arg(message);
        c
    };
    #[cfg(not(target_os = "macos"))]
    let mut command = {
        let mut c = Command::new("notify-send");
        c.arg("--").arg("usage").arg(message);
        c
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("系统通知不可用：{e}"))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err("系统通知发送失败".into())
                };
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("系统通知失败：{e}"));
            }
            Ok(None) if started.elapsed() >= std::time::Duration::from_secs(3) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("系统通知超时".into());
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{QuotaTier, snapshot_ok};
    use chrono::Duration;
    fn snapshot(used: f64, now: DateTime<Utc>, account: &str) -> UsageSnapshot {
        let mut s = snapshot_ok(
            "codex",
            "Codex",
            vec![QuotaTier {
                name: "primary".into(),
                utilization: used,
                resets_at: None,
            }],
            None,
            now,
        );
        s.account_email = Some(account.into());
        s
    }
    #[test]
    fn threshold_crossing_recovery_cooldown_and_accounts_are_independent() {
        let now = Utc::now();
        let config = AlertConfig {
            enabled: true,
            ..Default::default()
        };
        let mut monitor = QuotaMonitor::default();
        assert!(
            monitor
                .observe("local", &snapshot(80.0, now, "a"), &config, now)
                .is_empty()
        );
        let later = now + Duration::seconds(1);
        assert_eq!(
            monitor
                .observe("local", &snapshot(90.0, later, "a"), &config, later)
                .len(),
            1
        );
        assert!(
            monitor
                .observe(
                    "local",
                    &snapshot(95.0, later + Duration::seconds(1), "a"),
                    &config,
                    later
                )
                .is_empty()
        );
        let recovered = monitor.observe(
            "local",
            &snapshot(10.0, later + Duration::seconds(2), "a"),
            &config,
            later,
        );
        assert!(recovered[0].recovered);
        assert!(
            monitor
                .observe(
                    "local",
                    &snapshot(90.0, later + Duration::seconds(3), "a"),
                    &config,
                    later
                )
                .is_empty()
        );
        assert_eq!(
            monitor
                .observe("local", &snapshot(90.0, later, "b"), &config, later)
                .len(),
            1
        );
        assert!(
            monitor
                .observe("local", &snapshot(80.0, now, "b"), &config, later)
                .is_empty()
        ); // stale response
        let later = now + Duration::seconds(400);
        monitor.observe("local", &snapshot(10.0, later, "a"), &config, later);
        assert_eq!(
            monitor
                .observe(
                    "local",
                    &snapshot(90.0, later + Duration::seconds(1), "a"),
                    &config,
                    later
                )
                .len(),
            1
        );
    }
    #[test]
    fn suppressed_crossing_is_delivered_once_when_cooldown_expires() {
        let now = Utc::now();
        let config = AlertConfig {
            enabled: true,
            ..Default::default()
        };
        let mut monitor = QuotaMonitor::default();
        for (seconds, used, count, recovered) in [
            (0, 90.0, 1, false),
            (1, 10.0, 1, true),
            (2, 90.0, 0, false),
            (299, 95.0, 0, false),
            (300, 95.0, 1, false),
            (600, 95.0, 0, false),
        ] {
            let t = now + Duration::seconds(seconds);
            let alerts = monitor.observe("local", &snapshot(used, t, "a"), &config, t);
            assert_eq!(alerts.len(), count, "at {seconds}s");
            if let Some(alert) = alerts.first() {
                assert_eq!(alert.recovered, recovered);
            }
        }
    }
    #[test]
    fn invalid_or_stale_snapshots_do_not_clear_a_pending_alert() {
        let now = Utc::now();
        let config = AlertConfig {
            enabled: true,
            ..Default::default()
        };
        let mut monitor = QuotaMonitor::default();
        for (seconds, used) in [(0, 90.0), (1, 10.0), (2, 90.0)] {
            let t = now + Duration::seconds(seconds);
            monitor.observe("local", &snapshot(used, t, "a"), &config, t);
        }
        let later = now + Duration::seconds(300);
        let healthy = snapshot(10.0, later, "a");
        let mut failed = healthy.clone();
        failed.success = false;
        let mut transient = healthy.clone();
        transient.transient = true;
        let mut expired = healthy.clone();
        expired.credential_status = CredentialStatus::Expired;
        let mut invalid = healthy;
        invalid.tiers[0].utilization = f64::NAN;
        let stale = snapshot(10.0, now + Duration::seconds(1), "a");
        for s in [failed, transient, expired, invalid, stale] {
            assert!(monitor.observe("local", &s, &config, later).is_empty());
        }
        let alerts = monitor.observe("local", &snapshot(90.0, later, "a"), &config, later);
        assert_eq!(alerts.len(), 1);
        assert!(!alerts[0].recovered);
    }
    #[test]
    fn recovery_cancels_pending_alert_even_when_recovery_notifications_are_disabled() {
        let now = Utc::now();
        let config = AlertConfig {
            enabled: true,
            recovery: false,
            ..Default::default()
        };
        let mut monitor = QuotaMonitor::default();
        for (seconds, used, count) in [
            (0, 90.0, 1),
            (1, 10.0, 0),
            (2, 90.0, 0),
            (3, 10.0, 0),
            (300, 10.0, 0),
            (301, 90.0, 1),
            (601, 90.0, 0),
        ] {
            let t = now + Duration::seconds(seconds);
            assert_eq!(
                monitor
                    .observe("local", &snapshot(used, t, "a"), &config, t)
                    .len(),
                count,
                "at {seconds}s"
            );
        }
    }
    #[test]
    fn failed_disabled_and_invalid_snapshots_do_not_change_alert_state() {
        let now = Utc::now();
        let config = AlertConfig {
            enabled: true,
            ..Default::default()
        };
        let mut monitor = QuotaMonitor::default();
        let mut s = snapshot(90.0, now, "a");
        s.transient = true;
        assert!(monitor.observe("local", &s, &config, now).is_empty());
        s.transient = false;
        assert!(
            monitor
                .observe("local", &s, &AlertConfig::default(), now)
                .is_empty()
        );
        s.tiers[0].utilization = f64::NAN;
        assert!(monitor.observe("local", &s, &config, now).is_empty());
        s.tiers[0].utilization = 90.0;
        assert_eq!(monitor.observe("local", &s, &config, now).len(), 1);
    }
}
