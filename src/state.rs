//! Shared application state.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use sqlx::PgPool;

use crate::config::{AuthConfig, JobsConfig, Lifecycle};

/// `hive_http_requests_total{route,code}` counters (bounded: routes are templates).
#[derive(Default)]
pub struct HttpCounters {
    inner: Mutex<BTreeMap<(String, u16), u64>>,
}

impl HttpCounters {
    pub fn inc(&self, route: &str, code: u16) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g.entry((route.to_string(), code)).or_insert(0) += 1;
    }

    pub fn snapshot(&self) -> Vec<((String, u16), u64)> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub auth: Arc<AuthConfig>,
    pub lifecycle: Lifecycle,
    pub jobs: Arc<JobsConfig>,
    pub http: Arc<HttpCounters>,
}

impl AppState {
    /// Replace the jobs settings (the binary passes the environment-derived ones).
    pub fn with_jobs(mut self, jobs: JobsConfig) -> Self {
        self.jobs = Arc::new(jobs);
        self
    }

    pub fn new(pool: PgPool, auth: AuthConfig, lifecycle: Lifecycle) -> Self {
        Self {
            pool,
            auth: Arc::new(auth),
            lifecycle,
            jobs: Arc::new(JobsConfig::default()),
            http: Arc::new(HttpCounters::default()),
        }
    }
}
