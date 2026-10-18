//! Background work owned by the lifetime of an interactive Usage invocation.
use crate::codex_manager::fingerprint::{FingerprintService, runtime::Runtime};
use std::sync::Arc;
pub(crate) struct AppRuntime {
    _fingerprint: Runtime,
}
impl AppRuntime {
    pub fn start() -> Self {
        Self {
            _fingerprint: Runtime::start(
                &tokio::runtime::Handle::current(),
                Arc::new(FingerprintService::from_environment()),
            ),
        }
    }
}
