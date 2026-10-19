//! Local, bounded preflight; checking a reference bank never starts the sampler.
use super::*;
use std::{collections::HashMap, io::Read, sync::Mutex, time::SystemTime};

type Stamp = Option<(SystemTime, u64)>;
struct Cached {
    stamp: Stamp,
    reason: Option<String>,
}
#[derive(Default)]
pub(super) struct ReferenceBank {
    cache: Mutex<HashMap<PathBuf, Cached>>,
}
#[derive(Deserialize)]
struct Bank {
    models: Vec<Model>,
}
#[derive(Deserialize)]
struct Model {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
}
pub(super) fn matches_target(id: &str, display_name: Option<&str>) -> bool {
    if id == MODEL {
        return true;
    }
    // LMFPD can group models under one ID, e.g. id=gpt-6-astra with
    // display_name=gpt-6-astra/6.1-sol. Match explicit slash-separated labels,
    // expanding only the ID's family prefix; never use substring similarity.
    let short_target = id
        .split_once('-')
        .and_then(|(family, _)| MODEL.strip_prefix(family).and_then(|s| s.strip_prefix('-')));
    display_name.is_some_and(|name| {
        name.split('/')
            .map(str::trim)
            .any(|label| label == MODEL || short_target == Some(label))
    })
}
impl ReferenceBank {
    pub fn reason(&self, fpd: &Path) -> Option<String> {
        let unknown = || Some("无法确认本地参考库，已跳过测试以避免消耗订阅额度。".into());
        // Only inspect a known LMFPD package, never execute an arbitrary wrapper.
        if update::installed(fpd).is_none() {
            return unknown();
        }
        let Some(package) = fpd
            .canonicalize()
            .ok()
            .and_then(|p| p.parent()?.parent().map(Path::to_owned))
        else {
            return unknown();
        };
        let path = package.join("data/unified_bank.json");
        let stamp = fs::metadata(&path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        let mut cache = self.cache.lock().expect("fingerprint reference cache");
        if let Some(cached) = cache.get(&path).filter(|c| c.stamp == stamp) {
            return cached.reason.clone();
        }
        let reason = (|| {
            let mut bytes = Vec::new();
            fs::File::open(&path)
                .ok()?
                .take(32 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > 32 * 1024 * 1024 {
                return None;
            }
            let bank: Bank = serde_json::from_slice(&bytes).ok()?;
            Some(
                if bank
                    .models
                    .iter()
                    .any(|m| matches_target(&m.id, m.display_name.as_deref()))
                {
                    None
                } else {
                    Some(format!(
                        "参考库未收录 {MODEL}，已跳过测试以避免消耗订阅额度。"
                    ))
                },
            )
        })()
        .unwrap_or_else(unknown);
        cache.insert(
            path,
            Cached {
                stamp,
                reason: reason.clone(),
            },
        );
        reason
    }
}
