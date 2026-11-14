//! In-process HTTP server for provider fakes. Tests never reach a real API.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone, Default)]
pub(crate) struct FakeRequest {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl FakeRequest {
    pub fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn bearer(&self) -> Option<&str> {
        self.header("authorization")?.strip_prefix("Bearer ")
    }

    pub fn is_form(&self) -> bool {
        self.header("content-type")
            .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"))
    }

    pub fn is_json(&self) -> bool {
        self.header("content-type")
            .is_some_and(|value| value.starts_with("application/json"))
    }

    /// `application/x-www-form-urlencoded` fields.
    pub fn form(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(&self.body)
            .into_owned()
            .collect()
    }

    pub fn form_value(&self, name: &str) -> Option<String> {
        self.form()
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FakeResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl FakeResponse {
    pub fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: serde_json::to_vec(&value).unwrap(),
        }
    }

    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Serve `body` honouring `Range: bytes=a-b`. With `ignore_range_end`
    /// (Real-Debrid's CDN) a range streams to the end of the file.
    pub fn file(request: &FakeRequest, body: &[u8], ignore_range_end: bool) -> Self {
        let total = body.len() as u64;
        let range = request
            .header("range")
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.split_once('-'))
            .and_then(|(start, end)| {
                let start: u64 = start.parse().ok()?;
                let end: u64 = end.parse().unwrap_or(total.saturating_sub(1));
                Some((start, end))
            });
        let Some((start, end)) = range else {
            return Self {
                status: 200,
                headers: vec![
                    ("Content-Length".into(), total.to_string()),
                    ("Accept-Ranges".into(), "bytes".into()),
                ],
                body: body.to_vec(),
            };
        };
        if start >= total {
            return Self::empty(416).with_header("Content-Range", &format!("bytes */{total}"));
        }
        let end = end.min(total - 1);
        let sent_end = if ignore_range_end { total - 1 } else { end };
        let slice = &body[start as usize..=sent_end as usize];
        Self {
            status: 206,
            headers: vec![
                ("Content-Length".into(), slice.len().to_string()),
                (
                    "Content-Range".into(),
                    format!("bytes {start}-{end}/{total}"),
                ),
                ("Accept-Ranges".into(), "bytes".into()),
            ],
            body: slice.to_vec(),
        }
    }
}

type Handler = dyn Fn(&FakeRequest) -> FakeResponse + Send + Sync;

pub(crate) struct FakeServer {
    /// `http://127.0.0.1:<port>`
    pub origin: String,
    /// Every request received, for assertions.
    pub log: Arc<Mutex<Vec<FakeRequest>>>,
    accept: tokio::task::JoinHandle<()>,
}

impl FakeServer {
    pub fn requests(&self) -> Vec<FakeRequest> {
        self.log.lock().unwrap().clone()
    }

    pub fn count(&self, path_suffix: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.path.ends_with(path_suffix))
            .count()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

pub(crate) async fn serve(
    handler: impl Fn(&FakeRequest) -> FakeResponse + Send + Sync + 'static,
) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let handler: Arc<Handler> = Arc::new(handler);
    let log = Arc::new(Mutex::new(Vec::new()));
    let accept_log = Arc::clone(&log);
    let accept = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = Arc::clone(&handler);
            let log = Arc::clone(&accept_log);
            tokio::spawn(async move {
                let _ = handle(stream, handler, log).await;
            });
        }
    });
    FakeServer {
        origin,
        log,
        accept,
    }
}

async fn handle(
    mut stream: TcpStream,
    handler: Arc<Handler>,
    log: Arc<Mutex<Vec<FakeRequest>>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_string();
    let target = first.next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let parsed = url::Url::parse(&format!("http://fake{target}")).ok();
    let request = FakeRequest {
        method,
        path: parsed
            .as_ref()
            .map(|url| url.path().to_string())
            .unwrap_or(target),
        query: parsed
            .map(|url| url.query_pairs().into_owned().collect())
            .unwrap_or_default(),
        headers,
        body,
    };
    log.lock().unwrap().push(request.clone());
    let response = handler(&request);
    let reason = match response.status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        410 => "Gone",
        416 => "Range Not Satisfiable",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        451 => "Unavailable For Legal Reasons",
        _ => "Status",
    };
    let mut out = format!("HTTP/1.1 {} {reason}\r\n", response.status);
    let mut has_length = false;
    for (key, value) in &response.headers {
        has_length |= key.eq_ignore_ascii_case("content-length");
        out.push_str(&format!("{key}: {value}\r\n"));
    }
    if !has_length {
        out.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    }
    out.push_str("Connection: close\r\n\r\n");
    stream.write_all(out.as_bytes()).await?;
    if request_method_has_body(&response) {
        stream.write_all(&response.body).await?;
    }
    stream.flush().await
}

fn request_method_has_body(response: &FakeResponse) -> bool {
    response.status != 204
}
