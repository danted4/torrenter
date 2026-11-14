use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::Value;
use url::Url;

use super::DebridError;

const USER_AGENT: &str = concat!("torrenter/", env!("CARGO_PKG_VERSION"));

/// A provider response: status and the body parsed as JSON when it is
/// JSON (`Value::Null` otherwise).
pub(crate) struct Reply {
    pub status: StatusCode,
    pub json: Value,
}

/// JSON API client shared by the adapters. Unlike the download client it keeps
/// compression on, and its errors never carry the request URL.
#[derive(Clone)]
pub(crate) struct Api {
    client: Client,
    base: Url,
}

impl Api {
    pub fn new(base: Url) -> Result<Api, DebridError> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(5))
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .build()
            .map_err(|error| DebridError::Network(network_text(&error)))?;
        Ok(Api { client, base })
    }

    /// `path` is relative to the base (no leading slash).
    pub fn url(&self, path: &str) -> Url {
        self.base.join(path).unwrap_or_else(|_| self.base.clone())
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn send(&self, request: RequestBuilder) -> Result<Reply, DebridError> {
        let response = request
            .send()
            .await
            .map_err(|error| DebridError::Network(network_text(&error)))?;
        let status = response.status();
        let headers = response.headers().clone();
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            return Err(DebridError::RateLimited(retry_after(&headers)));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| DebridError::Network(network_text(&error)))?;
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(Reply { status, json })
    }
}

/// A short, URL-free description of a transport error.
pub(crate) fn network_text(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".into()
    } else if error.is_connect() {
        "could not connect".into()
    } else if error.is_decode() || error.is_body() {
        "bad response".into()
    } else {
        "request failed".into()
    }
}

/// `Retry-After` in seconds (dates are treated as unknown).
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let seconds: f64 = value.parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds.min(600.0)))
}

/// Text of a JSON field, accepting strings or numbers (ids flip between them).
pub(crate) fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Number of a JSON field, accepting numbers or numeric strings.
pub(crate) fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// 0–1000 from a progress value that may be a 0–1 fraction or a 0–100 percent.
pub(crate) fn permille(progress: f64) -> u16 {
    if !progress.is_finite() || progress <= 0.0 {
        return 0;
    }
    let fraction = if progress <= 1.0 {
        progress
    } else {
        progress / 100.0
    };
    (fraction.min(1.0) * 1000.0).round() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_accepts_fraction_or_percent() {
        assert_eq!(permille(0.0), 0);
        assert_eq!(permille(0.5), 500);
        assert_eq!(permille(1.0), 1000);
        assert_eq!(permille(45.0), 450);
        assert_eq!(permille(100.0), 1000);
        assert_eq!(permille(f64::NAN), 0);
    }

    #[test]
    fn ids_and_numbers_accept_both_shapes() {
        assert_eq!(text(&serde_json::json!(12)).as_deref(), Some("12"));
        assert_eq!(text(&serde_json::json!("ab")).as_deref(), Some("ab"));
        assert_eq!(number(&serde_json::json!("7")), Some(7.0));
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, "61".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(61)));
    }
}
