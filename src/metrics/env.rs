// author: kodeholic (powered by Claude)
//! EnvironmentMeta — 서버 실행 환경 (시작 시 1회 결정, immutable)

pub(crate) struct EnvironmentMeta {
    build_mode:    &'static str,
    log_level:     String,
    worker_count:  usize,
    bwe_mode:      String,
    version:       &'static str,
}

impl EnvironmentMeta {
    pub(crate) fn capture(worker_count: usize, bwe_mode: &crate::config::BweMode) -> Self {
        Self {
            build_mode: if cfg!(debug_assertions) { "debug" } else { "release" },
            log_level: std::env::var("RUST_LOG")
                .or_else(|_| std::env::var("LOG_LEVEL"))
                .unwrap_or_else(|_| "info".to_string()),
            worker_count,
            bwe_mode: bwe_mode.to_string(),
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "build_mode":   self.build_mode,
            "log_level":    self.log_level,
            "worker_count": self.worker_count,
            "bwe_mode":     self.bwe_mode,
            "version":      self.version,
        })
    }
}
