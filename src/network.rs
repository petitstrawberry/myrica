//! Small asynchronous HTTP executor shared by the Blitz document loader.

use std::borrow::Cow;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use blitz_traits::net::{Body, Bytes, NetHandler, NetProvider, Request};
use data_url::DataUrl;
use encoding_rs::{Encoding, UTF_8};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::backend::{BackendError, WakeCallback};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "Myrica/0.1 (+https://github.com/petitstrawberry/myrica)";

/// Successful HTTP response passed back to a browser backend.
pub struct FetchResponse {
    pub final_url: String,
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
}

impl FetchResponse {
    /// Decode text using the HTTP charset, with a Unicode BOM taking priority.
    /// Binary resources retain their original bytes for their own decoders.
    pub fn text(&self) -> Cow<'_, str> {
        let mime = self
            .content_type
            .as_deref()
            .and_then(|value| value.parse::<mime::Mime>().ok());
        let encoding = mime
            .as_ref()
            .and_then(|mime| mime.get_param(mime::CHARSET))
            .and_then(|charset| Encoding::for_label(charset.as_str().as_bytes()))
            .unwrap_or(UTF_8);
        encoding.decode(&self.body).0
    }
}

type Completion = Box<dyn FnOnce(Result<FetchResponse, String>) + Send + 'static>;

struct NetworkJob {
    request: Request,
    completion: Completion,
}

/// Cloneable handle to one current-thread Tokio runtime owned by a worker.
#[derive(Clone)]
pub struct NetworkService {
    sender: UnboundedSender<NetworkJob>,
    wake: WakeCallback,
}

impl NetworkService {
    /// Start the network worker.
    pub fn new(wake: WakeCallback) -> Result<Self, BackendError> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| BackendError::new(format!("create HTTP client: {error}")))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| BackendError::new(format!("create network runtime: {error}")))?;
        let (sender, receiver) = unbounded_channel();

        thread::Builder::new()
            .name(String::from("myrica-network"))
            .spawn(move || runtime.block_on(run_worker(client, receiver)))
            .map_err(|error| BackendError::new(format!("start network worker: {error}")))?;

        Ok(Self { sender, wake })
    }

    /// Submit one request without blocking the browser's UI thread.
    pub fn submit(&self, request: Request, completion: Completion) {
        let job = NetworkJob {
            request,
            completion,
        };
        if let Err(error) = self.sender.send(job) {
            (error.0.completion)(Err(String::from("network worker stopped")));
            (self.wake)();
        }
    }
}

impl NetProvider for NetworkService {
    fn fetch(&self, _document_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let requested_url = request.url.to_string();
        let wake = Arc::clone(&self.wake);
        self.submit(
            request,
            Box::new(move |result| {
                match result {
                    Ok(response) => {
                        handler.bytes_with_status(
                            response.status,
                            response.final_url,
                            Bytes::from(response.body),
                        );
                    }
                    Err(error) => {
                        eprintln!("[myrica:network] {requested_url}: {error}");
                        // Delivering an empty response lets Blitz retire critical resource
                        // bookkeeping instead of leaving the document permanently blocked.
                        handler.bytes_with_status(0, requested_url, Bytes::new());
                    }
                }
                wake();
            }),
        );
    }
}

async fn run_worker(client: reqwest::Client, mut receiver: UnboundedReceiver<NetworkJob>) {
    while let Some(job) = receiver.recv().await {
        let client = client.clone();
        tokio::spawn(async move {
            let result = fetch(client, job.request).await;
            (job.completion)(result);
        });
    }
}

async fn fetch(client: reqwest::Client, request: Request) -> Result<FetchResponse, String> {
    if request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.aborted())
    {
        return Err(String::from("request aborted"));
    }

    if request.url.scheme() == "data" {
        let final_url = request.url.to_string();
        let data_url =
            DataUrl::process(&final_url).map_err(|error| format!("invalid data URL: {error:?}"))?;
        let (body, _) = data_url
            .decode_to_vec()
            .map_err(|error| format!("invalid data URL payload: {error:?}"))?;
        let content_type = Some(data_url.mime_type().to_string());
        return Ok(FetchResponse {
            final_url,
            status: 200,
            body,
            content_type,
        });
    }

    if !matches!(request.url.scheme(), "http" | "https") {
        return Err(format!(
            "unsupported resource URL scheme: {}",
            request.url.scheme()
        ));
    }

    let mut builder = client
        .request(request.method, request.url)
        .headers(request.headers);
    if let Some(content_type) = request.content_type {
        builder = builder.header(reqwest::header::CONTENT_TYPE, content_type);
    }
    builder = match request.body {
        Body::Empty => builder,
        Body::Bytes(bytes) => builder.body(bytes),
        Body::Form(_) => return Err(String::from("form submission is not implemented yet")),
    };

    let response = builder.send().await.map_err(|error| error.to_string())?;
    if request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.aborted())
    {
        return Err(String::from("request aborted"));
    }

    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response
        .bytes()
        .await
        .map_err(|error| error.to_string())?
        .to_vec();

    if request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.aborted())
    {
        return Err(String::from("request aborted"));
    }

    Ok(FetchResponse {
        final_url,
        status,
        body,
        content_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::SHIFT_JIS;

    #[test]
    fn shift_jis_http_response_decodes_japanese_without_changing_bytes() {
        let html = "<p>Google 検索にアクセスできない場合はこちら</p>";
        let (bytes, _, errors) = SHIFT_JIS.encode(html);
        assert!(!errors);
        let body = bytes.into_owned();
        let response = FetchResponse {
            final_url: "https://example.com/".into(),
            status: 200,
            content_type: Some("text/html; charset=Shift_JIS".into()),
            body: body.clone(),
        };
        assert_eq!(response.text(), html);
        assert_eq!(response.body, body);
    }

    #[test]
    fn unicode_bom_overrides_header_charset() {
        let response = FetchResponse {
            final_url: "https://example.com/".into(),
            status: 200,
            content_type: Some("text/html; charset=Shift_JIS".into()),
            body: "\u{feff}<p>日本語</p>".as_bytes().to_vec(),
        };
        assert_eq!(response.text(), "<p>日本語</p>");
    }

    #[test]
    fn unknown_or_missing_charset_defaults_to_utf8() {
        for content_type in [None, Some("text/html; charset=unknown".into())] {
            let response = FetchResponse {
                final_url: "https://example.com/".into(),
                status: 200,
                content_type,
                body: "日本語".as_bytes().to_vec(),
            };
            assert_eq!(response.text(), "日本語");
        }
    }
}
