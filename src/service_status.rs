//! Public provider status feeds, independent of account credentials and quota requests.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::http::HttpClient;

const TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Operational,
    Maintenance,
    Degraded,
    PartialOutage,
    MajorOutage,
    Unknown,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Operational => "正常",
            Self::Maintenance => "维护中",
            Self::Degraded => "服务降级",
            Self::PartialOutage => "部分故障",
            Self::MajorOutage => "严重故障",
            Self::Unknown => "状态未知",
        }
    }

    fn worst(self, other: Self) -> Self {
        fn rank(state: State) -> u8 {
            match state {
                State::Operational => 0,
                State::Maintenance => 1,
                State::Degraded => 2,
                State::PartialOutage => 3,
                State::MajorOutage => 4,
                State::Unknown => 5,
            }
        }
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Source {
    pub id: &'static str,
    pub name: &'static str,
    pub page_url: &'static str,
    pub endpoint: &'static str,
}

pub const OPENAI: Source = Source {
    id: "openai",
    name: "OpenAI",
    page_url: "https://status.openai.com/",
    endpoint: "https://status.openai.com/api/v2/summary.json",
};
pub const CLAUDE: Source = Source {
    id: "claude",
    name: "Claude",
    page_url: "https://status.claude.com/",
    endpoint: "https://status.claude.com/api/v2/summary.json",
};
pub const GROK: Source = Source {
    id: "grok",
    name: "xAI / Grok",
    page_url: "https://status.x.ai/",
    endpoint: "https://status.x.ai/feed.xml",
};

pub fn source(provider: &str) -> Option<Source> {
    match provider {
        "openai" | "codex" => Some(OPENAI),
        "claude" | "anthropic" => Some(CLAUDE),
        "grok" | "xai" => Some(GROK),
        _ => None,
    }
}

pub fn select(providers: &str) -> Result<Vec<Source>, String> {
    let mut sources = Vec::new();
    for id in providers.split(',').map(str::trim) {
        let source = source(id)
            .ok_or_else(|| format!("未知状态来源：{id}；可选 openai/codex、claude、grok"))?;
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    Ok(sources)
}

#[derive(Debug, Clone, Serialize)]
pub struct Component {
    pub name: String,
    pub state: State,
}

#[derive(Debug, Clone, Serialize)]
pub struct Incident {
    pub name: String,
    pub status: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub provider: String,
    pub display_name: String,
    pub page_url: String,
    pub state: State,
    pub description: String,
    pub components: Vec<Component>,
    pub incidents: Vec<Incident>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub checked_at: DateTime<Utc>,
    pub stale: bool,
    pub error: Option<String>,
}

impl ServiceStatus {
    fn new(source: Source) -> Self {
        Self {
            provider: source.id.into(),
            display_name: source.name.into(),
            page_url: source.page_url.into(),
            state: State::Unknown,
            description: String::new(),
            components: Vec::new(),
            incidents: Vec::new(),
            fetched_at: None,
            checked_at: Utc::now(),
            stale: false,
            error: None,
        }
    }

    /// Preserve the last successful result when the next complete query fails.
    pub fn merge(previous: Option<Self>, next: Self) -> Self {
        if next.fetched_at.is_none()
            && let Some(mut previous) = previous
        {
            previous.checked_at = next.checked_at;
            previous.error = next.error;
            previous.stale = true;
            previous
        } else {
            next
        }
    }

    pub fn label(&self) -> &'static str {
        if self.provider == "grok" && self.state == State::Operational {
            "未报告故障"
        } else {
            self.state.label()
        }
    }

    pub fn summary(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        if let Some(incident) = self.incidents.first() {
            return format!("{} 故障 · {}", self.incidents.len(), incident.name);
        }
        let affected: Vec<_> = self
            .components
            .iter()
            .filter(|c| c.state != State::Operational)
            .map(|c| c.name.as_str())
            .collect();
        if !affected.is_empty() {
            affected.join(" · ")
        } else {
            self.page_url
                .trim_start_matches("https://")
                .trim_end_matches('/')
                .into()
        }
    }
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

async fn get(http: &dyn HttpClient, url: &str) -> Result<Vec<u8>, String> {
    let response = http
        .request(
            "GET",
            url,
            &[("User-Agent", format!("usage/{}", env!("CARGO_PKG_VERSION")))],
            None,
        )
        .await
        .map_err(|_| "状态源网络请求失败".to_string())?;
    if !(200..300).contains(&response.status) {
        return Err(format!("状态源 HTTP {}", response.status));
    }
    Ok(response.body)
}

pub async fn query(http: &dyn HttpClient, source: Source) -> ServiceStatus {
    let mut snapshot = ServiceStatus::new(source);
    let result = tokio::time::timeout(TIMEOUT, async {
        let body = get(http, source.endpoint).await?;
        if source == GROK {
            parse_rss(&body, &mut snapshot)
        } else {
            parse_summary(&body, &mut snapshot)?;
            // OpenAI's compatibility summary does not contain incidents. Its
            // incidents endpoint includes history, so filter resolved entries.
            if source == OPENAI {
                match get(http, "https://status.openai.com/api/v2/incidents.json").await {
                    Ok(body) => match serde_json::from_slice::<IncidentFeed>(&body) {
                        Ok(feed) => apply_incidents(feed.incidents, &mut snapshot),
                        Err(_) => snapshot.error = Some("故障详情格式异常".into()),
                    },
                    Err(error) => snapshot.error = Some(format!("故障详情查询失败：{error}")),
                }
            }
            Ok(())
        }
    })
    .await;
    match result {
        Ok(Ok(())) => snapshot.fetched_at = Some(Utc::now()),
        Ok(Err(error)) => {
            snapshot = ServiceStatus::new(source);
            snapshot.error = Some(error);
        }
        Err(_) => {
            // Discard any partial data when the overall query timed out.
            snapshot = ServiceStatus::new(source);
            snapshot.error = Some("状态查询超时".into());
        }
    }
    snapshot.checked_at = Utc::now();
    snapshot
}

pub async fn collect(http: Arc<dyn HttpClient>, sources: &[Source]) -> Vec<ServiceStatus> {
    let mut jobs = JoinSet::new();
    for (index, source) in sources.iter().copied().enumerate() {
        let http = Arc::clone(&http);
        jobs.spawn(async move { (index, query(http.as_ref(), source).await) });
    }
    let mut results = vec![None; sources.len()];
    while let Some(completion) = jobs.join_next().await {
        if let Ok((index, snapshot)) = completion {
            results[index] = Some(snapshot);
        }
    }
    results
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            result.unwrap_or_else(|| {
                let mut snapshot = ServiceStatus::new(sources[index]);
                snapshot.error = Some("状态查询任务中断".into());
                snapshot
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct Summary {
    status: RawStatus,
    #[serde(default)]
    components: Vec<RawComponent>,
    #[serde(default)]
    incidents: Vec<RawIncident>,
}
#[derive(Deserialize)]
struct RawStatus {
    indicator: String,
    description: String,
}
#[derive(Deserialize)]
struct RawComponent {
    name: String,
    status: String,
}
#[derive(Deserialize)]
struct IncidentFeed {
    incidents: Vec<RawIncident>,
}
#[derive(Deserialize)]
struct RawIncident {
    name: String,
    status: String,
    #[serde(default)]
    impact: String,
    #[serde(default)]
    shortlink: String,
}

fn indicator(value: &str) -> State {
    match value {
        "none" | "operational" => State::Operational,
        "minor" | "degraded_performance" | "disruption" => State::Degraded,
        "major" | "partial_outage" => State::PartialOutage,
        "critical" | "major_outage" | "outage" => State::MajorOutage,
        "maintenance" | "under_maintenance" => State::Maintenance,
        _ => State::Unknown,
    }
}

fn parse_summary(body: &[u8], snapshot: &mut ServiceStatus) -> Result<(), String> {
    let summary: Summary =
        serde_json::from_slice(body).map_err(|_| "状态 JSON 格式异常".to_string())?;
    snapshot.state = indicator(&summary.status.indicator);
    snapshot.description = clean(&summary.status.description);
    snapshot.components = summary
        .components
        .into_iter()
        .map(|c| Component {
            name: clean(&c.name),
            state: indicator(&c.status),
        })
        .collect();
    for component in &snapshot.components {
        snapshot.state = snapshot.state.worst(component.state);
    }
    apply_incidents(summary.incidents, snapshot);
    Ok(())
}

fn apply_incidents(incidents: Vec<RawIncident>, snapshot: &mut ServiceStatus) {
    let mut incident_state = State::Operational;
    for incident in incidents
        .into_iter()
        .filter(|i| !matches!(i.status.as_str(), "resolved" | "postmortem" | "completed"))
    {
        // Even an incident without an impact must not look operational.
        let state = match incident.impact.as_str() {
            "" | "none" => State::Degraded,
            impact => indicator(impact),
        };
        incident_state = incident_state.worst(state);
        snapshot.incidents.push(Incident {
            name: clean(&incident.name),
            status: clean(&incident.status),
            url: if incident.shortlink.starts_with("https://") {
                clean(&incident.shortlink)
            } else {
                snapshot.page_url.clone()
            },
        });
    }
    // Incident impact describes the original event. Current component/overall
    // status can already reflect mitigation. Use impact only when the summary
    // otherwise looks healthy, including OpenAI's older compatibility summary.
    if snapshot.state == State::Operational {
        snapshot.state = incident_state;
    }
}

#[derive(Deserialize)]
struct Rss {
    channel: Channel,
}
#[derive(Deserialize)]
struct Channel {
    title: String,
    #[serde(rename = "item", default)]
    items: Vec<RssItem>,
}
#[derive(Deserialize)]
struct RssItem {
    title: String,
    #[serde(default)]
    link: String,
    #[serde(rename = "category", default)]
    categories: Vec<String>,
}

fn parse_rss(body: &[u8], snapshot: &mut ServiceStatus) -> Result<(), String> {
    let feed: Rss =
        quick_xml::de::from_reader(body).map_err(|_| "状态 RSS 格式异常".to_string())?;
    if feed.channel.title.trim().is_empty() {
        return Err("状态 RSS 缺少标题".into());
    }
    snapshot.state = State::Operational;
    snapshot.description = "官方 RSS 未报告未解决故障；不是实时可用性探测".into();
    for item in feed.channel.items {
        let categories: Vec<_> = item
            .categories
            .iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .collect();
        if categories
            .iter()
            .any(|c| matches!(c.as_str(), "resolved" | "postmortem" | "completed"))
        {
            continue;
        }
        let status = categories.iter().find(|c| {
            matches!(
                c.as_str(),
                "investigating" | "identified" | "monitoring" | "in_progress" | "verifying"
            )
        });
        let Some(status) = status else {
            return Err("无法识别 RSS 故障阶段".into());
        };
        let severity = categories
            .iter()
            .find(|c| matches!(c.as_str(), "outage" | "disruption" | "maintenance"));
        snapshot.state = snapshot
            .state
            .worst(severity.map_or(State::Degraded, |s| indicator(s)));
        snapshot.incidents.push(Incident {
            name: clean(&item.title),
            status: status.clone(),
            url: if item.link.starts_with("https://") {
                clean(&item.link)
            } else {
                snapshot.page_url.clone()
            },
        });
    }
    Ok(())
}

pub fn render_text(snapshots: &[ServiceStatus]) -> String {
    snapshots
        .iter()
        .map(|snapshot| {
            let mut lines = vec![
                format!(
                    "{} · {}{}",
                    snapshot.display_name,
                    snapshot.label(),
                    if snapshot.error.is_some() {
                        " · 查询失败"
                    } else {
                        ""
                    }
                ),
                snapshot.page_url.clone(),
            ];
            if let Some(time) = snapshot.fetched_at {
                lines.push(format!("查询时间：{}", time.to_rfc3339()));
            }
            if let Some(error) = &snapshot.error {
                lines.push(error.clone());
            }
            lines.push(snapshot.description.clone());
            for component in &snapshot.components {
                if component.state != State::Operational {
                    lines.push(format!("  {}：{}", component.name, component.state.label()));
                }
            }
            for incident in &snapshot.incidents {
                lines.push(format!(
                    "  [{}] {}\n  {}",
                    incident.status, incident.name, incident.url
                ));
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{FakeHttp, HttpResponse, TransportError};

    fn response(body: &[u8]) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status: 200,
            headers: Default::default(),
            body: body.to_vec(),
        })
    }
    const HEALTHY: &[u8] = br#"{"status":{"indicator":"none","description":"All Systems Operational"},"components":[]}"#;

    #[test]
    fn provider_aliases_are_deduplicated_and_unknown_names_rejected() {
        assert_eq!(
            select("codex, openai,anthropic,xai").unwrap(),
            vec![OPENAI, CLAUDE, GROK]
        );
        assert!(select("openai,typo").is_err());
        assert!(select("").is_err());
    }

    #[test]
    fn current_degraded_status_takes_precedence_over_original_incident_impact() {
        let mut snapshot = ServiceStatus::new(CLAUDE);
        parse_summary(
            br#"{"status":{"indicator":"minor","description":"Degraded"},
            "incidents":[{"name":"Mitigated outage","status":"monitoring","impact":"major"}]}"#,
            &mut snapshot,
        )
        .unwrap();
        assert_eq!(snapshot.state, State::Degraded);
        assert_eq!(snapshot.incidents.len(), 1);
    }

    #[tokio::test]
    async fn openai_checks_incidents_and_sends_no_credentials() {
        let http = FakeHttp::new();
        http.route(OPENAI.endpoint, response(HEALTHY));
        http.route(
            "https://status.openai.com/api/v2/incidents.json",
            response(
                br#"{"incidents":[
            {"name":"Old outage","status":"resolved","impact":"critical"},
            {"name":"Codex errors","status":"monitoring","impact":"major"}
        ]}"#,
            ),
        );
        let status = query(&http, OPENAI).await;
        assert_eq!(status.state, State::PartialOutage);
        assert_eq!(status.incidents.len(), 1);
        assert_eq!(status.incidents[0].name, "Codex errors");
        assert!(status.fetched_at.is_some());
        assert!(status.error.is_none());
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|c| c.method == "GET"
            && c.body.is_none()
            && c.headers.iter().all(|(h, _)| !matches!(
                h.to_ascii_lowercase().as_str(),
                "authorization" | "cookie"
            ))));
    }

    #[tokio::test]
    async fn affected_components_override_a_healthy_summary() {
        let http = FakeHttp::new();
        http.route(
            CLAUDE.endpoint,
            response(
                br#"{"status":{"indicator":"none","description":"OK"},
            "components":[{"name":"Claude Code","status":"degraded_performance"}],
            "incidents":[{"name":"Old issue","status":"postmortem","impact":"critical"}]}"#,
            ),
        );
        let status = query(&http, CLAUDE).await;
        assert_eq!(status.state, State::Degraded);
        assert_eq!(status.summary(), "Claude Code");
        assert!(status.incidents.is_empty());
    }

    #[tokio::test]
    async fn rss_uses_categories_instead_of_old_resolved_descriptions() {
        let http = FakeHttp::new();
        http.route(GROK.endpoint, response(br#"<rss><channel><title>xAI</title>
            <item><title>Old outage</title><description><![CDATA[Investigating then resolved]]></description>
                <category>outage</category><category>resolved</category></item>
            <item><title>Grok &amp; API</title><category>disruption</category><category>monitoring</category></item>
        </channel></rss>"#));
        let status = query(&http, GROK).await;
        assert_eq!(status.state, State::Degraded);
        assert_eq!(status.incidents.len(), 1);
        assert_eq!(status.incidents[0].name, "Grok & API");
        assert!(status.error.is_none());
    }

    #[tokio::test]
    async fn resolved_only_rss_means_no_reported_incident() {
        let http = FakeHttp::new();
        http.route(
            GROK.endpoint,
            response(
                br#"<rss><channel><title>xAI</title><item>
            <title>Outage</title><category>outage</category><category>resolved</category>
        </item></channel></rss>"#,
            ),
        );
        let status = query(&http, GROK).await;
        assert_eq!(status.label(), "未报告故障");
        assert!(status.description.contains("不是实时可用性探测"));
    }

    #[tokio::test]
    async fn malformed_or_unrecognized_feeds_never_report_healthy() {
        for (source, body) in [
            (CLAUDE, b"<html>unavailable</html>".as_slice()),
            (CLAUDE, br#"{"components":[]}"#.as_slice()),
            (GROK, br#"<rss><channel><title>xAI</title><item><title>Issue</title><category>new_status</category></item></channel></rss>"#.as_slice()),
        ] {
            let http = FakeHttp::new();
            http.route(source.endpoint, response(body));
            let status = query(&http, source).await;
            assert_eq!(status.state, State::Unknown);
            assert!(status.fetched_at.is_none());
            assert!(status.error.is_some());
        }
        let mut status = ServiceStatus::new(CLAUDE);
        parse_summary(
            br#"{"status":{"indicator":"new_status","description":"New"}}"#,
            &mut status,
        )
        .unwrap();
        assert_eq!(status.state, State::Unknown);
    }

    #[tokio::test]
    async fn failed_refresh_preserves_previous_result_and_time() {
        let http = FakeHttp::new();
        http.route(CLAUDE.endpoint, response(HEALTHY));
        let previous = query(&http, CLAUDE).await;
        let time = previous.fetched_at;
        let failing = FakeHttp::new();
        failing.route(
            CLAUDE.endpoint,
            Ok(HttpResponse {
                status: 503,
                headers: Default::default(),
                body: Vec::new(),
            }),
        );
        let status = ServiceStatus::merge(Some(previous), query(&failing, CLAUDE).await);
        assert_eq!(status.state, State::Operational);
        assert_eq!(status.fetched_at, time);
        assert_eq!(status.error.as_deref(), Some("状态源 HTTP 503"));
        assert!(status.stale);
    }

    #[tokio::test]
    async fn failed_incident_request_keeps_summary_but_marks_incomplete() {
        let http = FakeHttp::new();
        http.route(OPENAI.endpoint, response(HEALTHY));
        let status = query(&http, OPENAI).await;
        assert!(status.fetched_at.is_some());
        assert!(status.error.as_ref().unwrap().contains("故障详情查询失败"));
    }

    #[tokio::test]
    async fn collection_preserves_source_order_including_failures() {
        let http = Arc::new(FakeHttp::new());
        http.route(CLAUDE.endpoint, response(HEALTHY));
        let statuses = collect(http, &[GROK, CLAUDE]).await;
        assert_eq!(
            statuses
                .iter()
                .map(|s| s.provider.as_str())
                .collect::<Vec<_>>(),
            vec!["grok", "claude"]
        );
        assert!(statuses[0].error.is_some());
        assert!(statuses[1].error.is_none());
    }
}
