use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::config::HealthcheckConfig;

/// RF-13/RF-14: run a single health check attempt, respecting its timeout.
/// Returns `Ok(())` on success, `Err(reason)` with a human-readable reason
/// on failure.
pub async fn check_once(hc: &HealthcheckConfig) -> Result<(), String> {
    match hc {
        HealthcheckConfig::Http {
            url,
            timeout: to,
            expected_status,
            ..
        } => check_http(url, *to, expected_status).await,
        HealthcheckConfig::Tcp {
            host,
            port,
            timeout: to,
            ..
        } => check_tcp(host, *port, *to).await,
    }
}

async fn check_http(url: &str, to: Duration, expected: &[u16]) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(to)
        .build()
        .map_err(|e| e.to_string())?;
    match client.get(url).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if expected.contains(&status) {
                Ok(())
            } else {
                Err(format!("unexpected status {status}"))
            }
        }
        Err(e) => Err(format!("request failed: {e}")),
    }
}

async fn check_tcp(host: &str, port: u16, to: Duration) -> Result<(), String> {
    let addr = format!("{host}:{port}");
    match timeout(to, TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("connection failed: {e}")),
        Err(_) => Err("connection timed out".to_string()),
    }
}

pub fn interval(hc: &HealthcheckConfig) -> Duration {
    match hc {
        HealthcheckConfig::Http { interval, .. } => *interval,
        HealthcheckConfig::Tcp { interval, .. } => *interval,
    }
}

pub fn retries(hc: &HealthcheckConfig) -> u32 {
    match hc {
        HealthcheckConfig::Http { retries, .. } => *retries,
        HealthcheckConfig::Tcp { retries, .. } => *retries,
    }
}

/// Poll until the check succeeds or `retries` attempts are exhausted.
/// Used both for RF-06 (waiting for a dependency to become healthy) and for
/// the initial readiness probe after starting a service.
pub async fn wait_until_healthy(hc: &HealthcheckConfig) -> bool {
    let attempts = retries(hc).max(1);
    let wait = interval(hc);
    for _ in 0..attempts {
        if check_once(hc).await.is_ok() {
            return true;
        }
        tokio::time::sleep(wait).await;
    }
    false
}
