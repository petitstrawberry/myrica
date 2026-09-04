//! Small asynchronous HTTP executor shared by the Blitz document loader.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use blitz_traits::net::{Body, Bytes, NetHandler, NetProvider, Request};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::backend::{BackendError, WakeCallback};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "Myrica/0.1 (+https://github.com/petitstrawberry/myrica)";

/// Successful HTTP response passed back to a browser backend.
pub struct FetchResponse {
    pub final_url: String,
    pub status: u16,
    pub body: Vec<u8>,
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
                        handler.bytes(response.final_url, Bytes::from(response.body));
                    }
                    Err(error) => {
                        eprintln!("[myrica:network] {requested_url}: {error}");
                        // Delivering an empty response lets Blitz retire critical resource
                        // bookkeeping instead of leaving the document permanently blocked.
                        handler.bytes(requested_url, Bytes::new());
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
    })
}
