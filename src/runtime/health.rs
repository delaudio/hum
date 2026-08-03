use std::sync::OnceLock;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::config::HealthcheckConfig;

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(reqwest::Client::new)
}

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
    match http_client().get(url).timeout(to).send().await {
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
    match timeout(to, TcpStream::connect((host, port))).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_checks_share_one_client_pool() {
        assert!(std::ptr::eq(http_client(), http_client()));
    }

    #[tokio::test]
    async fn tcp_check_supports_ipv6_hosts() {
        let Ok(listener) = tokio::net::TcpListener::bind(("::1", 0)).await else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        assert!(check_tcp("::1", port, Duration::from_millis(200))
            .await
            .is_ok());
    }
}
