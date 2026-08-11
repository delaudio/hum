use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;

use super::detached::{DetachedRuntime, DetachedServiceStatus, StartReport, StopReport};

pub type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Runtime-native operations used by the project orchestrator. Implementations
/// preserve their own identity model: PID/process-group for local processes,
/// Compose labels and container IDs for Docker Compose.
pub trait RuntimeAdapter: Send + Sync {
    fn owns_service(&self, service: &str) -> bool;

    fn start_services<'a>(&'a self, services: &'a [String]) -> AdapterFuture<'a, StartReport>;
    fn stop_services<'a>(
        &'a self,
        services: &'a [String],
        grace: Duration,
    ) -> AdapterFuture<'a, StopReport>;
    fn status_services<'a>(
        &'a self,
        services: &'a [String],
    ) -> AdapterFuture<'a, Vec<DetachedServiceStatus>>;
    fn monitor_services<'a>(
        &'a self,
        services: &'a [String],
    ) -> AdapterFuture<'a, Vec<DetachedServiceStatus>>;
    fn wait_ready<'a>(&'a self, service: &'a str) -> AdapterFuture<'a, ()>;
    fn log_files(&self, service: &str) -> Result<Option<(PathBuf, PathBuf)>>;
    fn stream_logs<'a>(
        &'a self,
        services: &'a [String],
        lines: usize,
        follow: bool,
    ) -> AdapterFuture<'a, ()>;
    fn capture_logs<'a>(
        &'a self,
        services: &'a [String],
        lines: usize,
    ) -> AdapterFuture<'a, Vec<String>>;
    fn reset<'a>(&'a self) -> AdapterFuture<'a, ()>;
}

impl RuntimeAdapter for DetachedRuntime {
    fn owns_service(&self, service: &str) -> bool {
        DetachedRuntime::owns_service(self, service)
    }

    fn start_services<'a>(&'a self, services: &'a [String]) -> AdapterFuture<'a, StartReport> {
        Box::pin(DetachedRuntime::start_services_exact(self, services))
    }

    fn stop_services<'a>(
        &'a self,
        services: &'a [String],
        grace: Duration,
    ) -> AdapterFuture<'a, StopReport> {
        Box::pin(DetachedRuntime::stop_services_exact(self, services, grace))
    }

    fn status_services<'a>(
        &'a self,
        services: &'a [String],
    ) -> AdapterFuture<'a, Vec<DetachedServiceStatus>> {
        Box::pin(DetachedRuntime::status_services(self, services))
    }

    fn monitor_services<'a>(
        &'a self,
        services: &'a [String],
    ) -> AdapterFuture<'a, Vec<DetachedServiceStatus>> {
        Box::pin(DetachedRuntime::monitor_services_exact(self, services))
    }

    fn wait_ready<'a>(&'a self, service: &'a str) -> AdapterFuture<'a, ()> {
        Box::pin(DetachedRuntime::wait_service_ready(self, service))
    }

    fn log_files(&self, service: &str) -> Result<Option<(PathBuf, PathBuf)>> {
        DetachedRuntime::log_paths(self, service).map(Some)
    }

    fn stream_logs<'a>(
        &'a self,
        _services: &'a [String],
        _lines: usize,
        _follow: bool,
    ) -> AdapterFuture<'a, ()> {
        Box::pin(async { Err(anyhow::anyhow!("process runtime uses persistent log files")) })
    }

    fn capture_logs<'a>(
        &'a self,
        _services: &'a [String],
        _lines: usize,
    ) -> AdapterFuture<'a, Vec<String>> {
        Box::pin(async { Err(anyhow::anyhow!("process runtime uses persistent log files")) })
    }

    fn reset<'a>(&'a self) -> AdapterFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_object_safe(_: &dyn RuntimeAdapter) {}

    #[allow(dead_code)]
    fn accepts_trait_object(adapter: &DetachedRuntime) {
        assert_object_safe(adapter);
    }
}
