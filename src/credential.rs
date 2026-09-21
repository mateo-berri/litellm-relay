//! Liveness and expiry of the Gateway session credential that `relay setup` saves.

use chrono::{DateTime, Duration, Utc};
use reqwest::StatusCode;
use serde_json::Value;

pub const EXPIRY_WARNING_WINDOW: Duration = Duration::hours(24);
pub const REENROLL_HINT: &str = "Run `litellm-relay setup` to sign in again.";

#[derive(Clone, Debug, PartialEq)]
pub enum CredentialCheck {
    Valid,
    Rejected { status: u16, detail: String },
    Unverifiable { detail: String },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExpiryState {
    Unknown,
    Ok { at: DateTime<Utc> },
    ExpiringSoon { at: DateTime<Utc> },
    Expired { at: DateTime<Utc> },
}

/// Ask the Gateway whether it still accepts `api_key`; only 2xx or 401/403 is conclusive.
pub async fn check_credential(
    http: &reqwest::Client,
    gateway_url: &str,
    api_key: &str,
) -> CredentialCheck {
    let url = format!("{}/v1/models", gateway_url.trim_end_matches('/'));
    let response = match http.get(url).bearer_auth(api_key).send().await {
        Ok(response) => response,
        Err(error) => {
            return CredentialCheck::Unverifiable {
                detail: error.to_string(),
            }
        }
    };
    let status = response.status();
    if status.is_success() {
        return CredentialCheck::Valid;
    }
    if !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return CredentialCheck::Unverifiable {
            detail: format!("HTTP {}", status.as_u16()),
        };
    }
    let body = response.text().await.unwrap_or_default();
    CredentialCheck::Rejected {
        status: status.as_u16(),
        detail: rejection_detail(&body).unwrap_or_else(|| status.to_string()),
    }
}

fn rejection_detail(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("error")?
        .get("message")?
        .as_str()
        .map(str::to_string)
}

pub fn expiry_state(expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> ExpiryState {
    let Some(at) = expires_at else {
        return ExpiryState::Unknown;
    };
    if at <= now {
        return ExpiryState::Expired { at };
    }
    if at - now <= EXPIRY_WARNING_WINDOW {
        return ExpiryState::ExpiringSoon { at };
    }
    ExpiryState::Ok { at }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    const EXPIRED_KEY_BODY: &str = r#"{"error":{"message":"Authentication Error - Expired Key. Key Expiry time 2026-09-21 22:27:58.096000+00:00 and current time 2026-09-21 22:28:15.211000+00:00","type":"expired_key","code":"401"}}"#;

    async fn serve_once(status_line: &str, body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let _ = stream.read(&mut request).await;
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        base_url
    }

    async fn unused_port_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        base_url
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(StdDuration::from_secs(5))
            .build()
            .unwrap()
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[tokio::test]
    async fn should_accept_credential_the_gateway_accepts() {
        let gateway = serve_once("200 OK", r#"{"data":[]}"#).await;
        assert_eq!(
            check_credential(&client(), &gateway, "sk-live").await,
            CredentialCheck::Valid
        );
    }

    #[tokio::test]
    async fn should_reject_credential_with_the_gateway_message() {
        let gateway = serve_once("401 Unauthorized", EXPIRED_KEY_BODY).await;
        let check = check_credential(&client(), &format!("{gateway}/"), "sk-expired").await;
        let CredentialCheck::Rejected { status, detail } = check else {
            panic!("expected Rejected, got {check:?}");
        };
        assert_eq!(status, 401);
        assert!(
            detail.starts_with("Authentication Error - Expired Key"),
            "{detail}"
        );
        assert!(!detail.contains("sk-expired"));
    }

    #[tokio::test]
    async fn should_fall_back_to_status_line_when_rejection_body_is_not_json() {
        let gateway = serve_once("403 Forbidden", "nope").await;
        assert_eq!(
            check_credential(&client(), &gateway, "sk-forbidden").await,
            CredentialCheck::Rejected {
                status: 403,
                detail: "403 Forbidden".into(),
            }
        );
    }

    #[tokio::test]
    async fn should_treat_gateway_errors_as_unverifiable() {
        let gateway = serve_once("502 Bad Gateway", "").await;
        assert_eq!(
            check_credential(&client(), &gateway, "sk-live").await,
            CredentialCheck::Unverifiable {
                detail: "HTTP 502".into(),
            }
        );
    }

    #[tokio::test]
    async fn should_treat_unreachable_gateway_as_unverifiable() {
        let gateway = unused_port_url().await;
        let check = check_credential(&client(), &gateway, "sk-live").await;
        let CredentialCheck::Unverifiable { detail } = check else {
            panic!("expected Unverifiable, got {check:?}");
        };
        assert!(!detail.is_empty());
        assert!(!detail.contains("sk-live"));
    }

    #[test]
    fn should_report_unknown_expiry_without_a_timestamp() {
        assert_eq!(
            expiry_state(None, at("2026-09-21T22:00:00Z")),
            ExpiryState::Unknown
        );
    }

    #[test]
    fn should_report_ok_expiry_outside_the_warning_window() {
        let expires_at = at("2026-09-23T22:00:01Z");
        assert_eq!(
            expiry_state(Some(expires_at), at("2026-09-22T22:00:00Z")),
            ExpiryState::Ok { at: expires_at }
        );
    }

    #[test]
    fn should_warn_inside_the_warning_window() {
        let expires_at = at("2026-09-23T22:00:00Z");
        assert_eq!(
            expiry_state(Some(expires_at), at("2026-09-22T22:00:00Z")),
            ExpiryState::ExpiringSoon { at: expires_at }
        );
        assert_eq!(
            expiry_state(Some(expires_at), at("2026-09-23T21:59:59Z")),
            ExpiryState::ExpiringSoon { at: expires_at }
        );
    }

    #[test]
    fn should_report_expired_at_and_after_the_timestamp() {
        let expires_at = at("2026-09-21T22:27:58Z");
        assert_eq!(
            expiry_state(Some(expires_at), expires_at),
            ExpiryState::Expired { at: expires_at }
        );
        assert_eq!(
            expiry_state(Some(expires_at), at("2026-09-21T22:28:15Z")),
            ExpiryState::Expired { at: expires_at }
        );
    }
}
