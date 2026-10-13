//! One-hop HTTP transport for WebCore's asynchronous resource loader.
//!
//! WebCore owns redirects, cookies, credentials, CSP, and CORS. This executor
//! sends exactly the request it receives and never enters WebCore from its
//! network thread. Completions cross a standard channel to the browser thread.

use std::collections::HashMap;
use std::io::{self, Read};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use reqwest::Url;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::backend::{BackendError, WakeCallback};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const MAX_CONTENT_ENCODINGS: usize = 16;
const USER_AGENT: &str = "Myrica/0.1 (+https://github.com/petitstrawberry/myrica)";

#[derive(Debug)]
pub(super) struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub(super) struct FetchResponse {
    pub final_url: String,
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub(super) struct Completion {
    pub id: u64,
    pub result: Result<FetchResponse, String>,
}

enum NetworkJob {
    Submit { id: u64, request: Request },
    Cancel(u64),
    Finished(u64, Result<FetchResponse, String>),
    Shutdown,
}

pub(super) struct NetworkService {
    sender: UnboundedSender<NetworkJob>,
    completion_sender: mpsc::Sender<Completion>,
    completions: mpsc::Receiver<Completion>,
    wake: WakeCallback,
}

impl NetworkService {
    pub(super) fn new(wake: WakeCallback) -> Result<Self, BackendError> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .cookie_store(false)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| BackendError::new(format!("create HTTP client: {error}")))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| BackendError::new(format!("create network runtime: {error}")))?;
        let (sender, receiver) = unbounded_channel();
        let (completion_sender, completions) = mpsc::channel();
        let worker_sender = sender.clone();
        let worker_completions = completion_sender.clone();
        let worker_wake = Arc::clone(&wake);

        thread::Builder::new()
            .name(String::from("myrica-webkit-network"))
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                runtime.block_on(run_worker(
                    client,
                    receiver,
                    worker_sender,
                    worker_completions,
                    worker_wake,
                ));
            })
            .map_err(|error| BackendError::new(format!("start network worker: {error}")))?;

        Ok(Self {
            sender,
            completion_sender,
            completions,
            wake,
        })
    }

    /// IDs must be unique until their completion has been received.
    pub(super) fn submit(&self, id: u64, request: Request) {
        if self
            .sender
            .send(NetworkJob::Submit { id, request })
            .is_err()
        {
            let _ = self.completion_sender.send(Completion {
                id,
                result: Err(String::from("network worker stopped")),
            });
            (self.wake)();
        }
    }

    /// Abort the actual HTTP future. The resulting cancellation completion
    /// lets callers retire bookkeeping; cancelled bridge requests can ignore it.
    pub(super) fn cancel(&self, id: u64) {
        let _ = self.sender.send(NetworkJob::Cancel(id));
    }

    pub(super) fn try_recv(&self) -> Option<Completion> {
        self.completions.try_recv().ok()
    }

    #[cfg(test)]
    pub(super) fn poll(&self) -> impl Iterator<Item = Completion> + '_ {
        self.completions.try_iter()
    }
}

impl Drop for NetworkService {
    fn drop(&mut self) {
        // Tasks also own senders. Shutdown prevents those senders from keeping
        // a runtime alive after the backend that created it has gone away.
        let _ = self.sender.send(NetworkJob::Shutdown);
    }
}

async fn run_worker(
    client: reqwest::Client,
    mut receiver: UnboundedReceiver<NetworkJob>,
    commands: UnboundedSender<NetworkJob>,
    completions: mpsc::Sender<Completion>,
    wake: WakeCallback,
) {
    let mut active: HashMap<u64, tokio::task::AbortHandle> = HashMap::new();
    while let Some(job) = receiver.recv().await {
        match job {
            NetworkJob::Submit { id, request } => {
                if active.contains_key(&id) {
                    // A malformed caller cannot replace an uncancelled task.
                    // Abort and retire it before reporting the protocol error.
                    active.remove(&id).unwrap().abort();
                    if completions
                        .send(Completion {
                            id,
                            result: Err(String::from("duplicate active resource request ID")),
                        })
                        .is_ok()
                    {
                        wake();
                    }
                    continue;
                }
                let client = client.clone();
                let commands = commands.clone();
                let task = tokio::spawn(async move {
                    let result = fetch(client, request).await;
                    // The owner serializes finish and cancel on one queue.
                    // Tasks never publish completions independently.
                    let _ = commands.send(NetworkJob::Finished(id, result));
                });
                active.insert(id, task.abort_handle());
            }
            NetworkJob::Cancel(id) => {
                if let Some(task) = active.remove(&id) {
                    task.abort();
                    if completions
                        .send(Completion {
                            id,
                            result: Err(String::from("resource request cancelled")),
                        })
                        .is_ok()
                    {
                        wake();
                    }
                }
            }
            NetworkJob::Finished(id, result) => {
                if active.remove(&id).is_some()
                    && completions.send(Completion { id, result }).is_ok()
                {
                    wake();
                }
            }
            NetworkJob::Shutdown => break,
        }
    }
    for task in active.into_values() {
        task.abort();
    }
}

async fn fetch(client: reqwest::Client, request: Request) -> Result<FetchResponse, String> {
    let url = Url::parse(&request.url).map_err(|error| format!("invalid resource URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("unsupported resource URL scheme: {}", url.scheme()));
    }

    // Reqwest converts URL user-info into Basic authorization. The initial
    // embedding profile cannot support that without both inventing credentials
    // and changing the response URL checked by WebCore, so reject it outright.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(String::from("HTTP URL user-info is unsupported"));
    }

    let method = reqwest::Method::from_bytes(request.method.as_bytes())
        .map_err(|error| format!("invalid HTTP method: {error}"))?;
    let request_has_entity = method != reqwest::Method::HEAD;
    let mut builder = client.request(method, url);
    for (name, value) in request.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("invalid request header name: {error}"))?;
        // The C ABI presents HTTP ByteString octets as Unicode Latin-1
        // characters. Encode that mapping back to bytes, never to UTF-8.
        let bytes = value
            .chars()
            .map(|character| u8::try_from(u32::from(character)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                String::from("invalid request header value: character outside Latin-1 byte range")
            })?;
        let value = reqwest::header::HeaderValue::from_bytes(&bytes)
            .map_err(|error| format!("invalid request header value: {error}"))?;
        builder = builder.header(name, value);
    }
    // Non-empty bodies retain their original bytes, including form and binary
    // uploads. An empty GET does not acquire an invented payload.
    if !request.body.is_empty() {
        builder = builder.body(request.body);
    }

    let mut response = builder.send().await.map_err(|error| error.to_string())?;
    let response_has_entity =
        request_has_entity && !matches!(response.status().as_u16(), 204 | 304);
    if response_has_entity
        && response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return Err(body_limit_error());
    }
    let final_url = response.url().to_string();
    let status = response.status().as_u16();
    let status_text = response
        .status()
        .canonical_reason()
        .unwrap_or("")
        .to_owned();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            // Isomorphic Latin-1 mapping keeps obs-text header bytes as well.
            (
                name.as_str().to_owned(),
                value.as_bytes().iter().copied().map(char::from).collect(),
            )
        })
        .collect();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        append_body(&mut body, &chunk, MAX_BODY_BYTES)?;
    }
    if response_has_entity {
        body = decode_entity(&headers, body, MAX_BODY_BYTES)?;
    }
    Ok(FetchResponse {
        final_url,
        status,
        status_text,
        headers,
        body,
    })
}

/// ResourceHandle receives entity bytes after HTTP content decoding, as it
/// does with the Curl loader. Original headers are kept for WebCore's response
/// policy checks; reqwest's automatic decoders would remove those headers.
fn decode_entity(
    headers: &[(String, String)],
    mut body: Vec<u8>,
    limit: usize,
) -> Result<Vec<u8>, String> {
    if body.len() > limit {
        return Err(body_limit_error());
    }
    let mut encodings = Vec::new();
    for (_, value) in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-encoding"))
    {
        if value.trim().is_empty() {
            continue;
        }
        for token in value.split(',') {
            let encoding = token.trim().to_ascii_lowercase();
            if encoding.is_empty() {
                return Err(String::from("invalid empty HTTP content encoding"));
            }
            if !matches!(
                encoding.as_str(),
                "gzip" | "x-gzip" | "br" | "deflate" | "identity"
            ) {
                return Err(format!("unsupported HTTP content encoding: {encoding}"));
            }
            encodings.push(encoding);
            if encodings.len() > MAX_CONTENT_ENCODINGS {
                return Err(String::from("too many HTTP content encoding layers"));
            }
        }
    }
    // Content-Encoding lists codings in the order applied to the entity.
    // Independently bound every reversed layer, not just the final result.
    for encoding in encodings.into_iter().rev() {
        body = match encoding.as_str() {
            "identity" => body,
            "gzip" | "x-gzip" => {
                let mut decoder = flate2::bufread::MultiGzDecoder::new(body.as_slice());
                read_decoded(&mut decoder, &encoding, limit)?
            }
            "deflate" => decode_deflate(&body, limit)?,
            "br" => {
                let mut decoder = StrictBrotliDecoder::new(&body);
                read_decoded(&mut decoder, &encoding, limit)?
            }
            _ => unreachable!("encoding was validated above"),
        };
    }
    Ok(body)
}

fn read_decoded(decoder: &mut impl Read, encoding: &str, limit: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    // The extra byte distinguishes a real EOF from an output limit. At the
    // exact limit the decoder must still reach EOF and validate its checksum.
    decoder
        .take(limit as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|error| format!("invalid HTTP {encoding} entity: {error}"))?;
    if body.len() > limit {
        return Err(body_limit_error());
    }
    Ok(body)
}

fn decode_deflate(body: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    // HTTP deflate is RFC 1950 zlib. Some servers send raw RFC 1951 instead;
    // identify the wrapper from CMF/FLG instead of retrying a corrupt zlib
    // stream as raw data after an Adler-32 or dictionary error.
    let zlib_header = body.len() >= 2
        && body[0] & 0x0f == 8
        && body[0] >> 4 <= 7
        && u16::from_be_bytes([body[0], body[1]]) % 31 == 0;
    let (decoded, remaining) = if zlib_header {
        let mut decoder = flate2::bufread::ZlibDecoder::new(body);
        let decoded = read_decoded(&mut decoder, "deflate", limit)?;
        (decoded, decoder.get_ref().len())
    } else {
        let mut decoder = flate2::bufread::DeflateDecoder::new(body);
        let decoded = read_decoded(&mut decoder, "deflate", limit)?;
        (decoded, decoder.get_ref().len())
    };
    if remaining != 0 {
        return Err(String::from("invalid HTTP deflate entity: trailing bytes"));
    }
    Ok(decoded)
}

/// The crate's usual Read decoder accepts the nonstandard large-window
/// extension and may return EOF before validating trailing bytes. A small
/// Read adapter over its strict streaming API keeps standard Brotli windows
/// and requires the entire encoded entity to be consumed before success.
struct StrictBrotliDecoder<'a> {
    input: &'a [u8],
    input_offset: usize,
    available_in: usize,
    total_out: usize,
    state: brotli_decompressor::BrotliState<
        brotli_decompressor::StandardAlloc,
        brotli_decompressor::StandardAlloc,
        brotli_decompressor::StandardAlloc,
    >,
    finished: bool,
}

impl<'a> StrictBrotliDecoder<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            input_offset: 0,
            available_in: input.len(),
            total_out: 0,
            state: brotli_decompressor::BrotliState::new_strict(
                brotli_decompressor::StandardAlloc::default(),
                brotli_decompressor::StandardAlloc::default(),
                brotli_decompressor::StandardAlloc::default(),
            ),
            finished: false,
        }
    }
}

impl Read for StrictBrotliDecoder<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.finished {
            return Ok(0);
        }
        let mut available_out = output.len();
        let mut output_offset = 0;
        let result = brotli_decompressor::BrotliDecompressStream(
            &mut self.available_in,
            &mut self.input_offset,
            self.input,
            &mut available_out,
            &mut output_offset,
            output,
            &mut self.total_out,
            &mut self.state,
        );
        match result {
            brotli_decompressor::BrotliResult::ResultSuccess => {
                if self.available_in != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "trailing Brotli bytes",
                    ));
                }
                self.finished = true;
                Ok(output_offset)
            }
            brotli_decompressor::BrotliResult::NeedsMoreOutput if output_offset != 0 => {
                Ok(output_offset)
            }
            brotli_decompressor::BrotliResult::NeedsMoreInput => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete Brotli stream",
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "corrupt Brotli stream",
            )),
        }
    }
}

fn append_body(body: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<(), String> {
    if bytes.len() > limit.saturating_sub(body.len()) {
        return Err(body_limit_error());
    }
    body.extend_from_slice(bytes);
    Ok(())
}

fn body_limit_error() -> String {
    String::from("resource body exceeds the 64 MiB limit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};

    const ENCODED_HTML: &str = "<!doctype html><p>いちご</p>";
    // Interoperability fixtures produced by Node's native Brotli encoder,
    // independently of the Rust decoder used by the transport.
    const BROTLI_HTML: &[u8] = &[
        27, 30, 0, 248, 141, 148, 172, 229, 153, 58, 144, 144, 50, 84, 243, 248, 192, 141, 107, 4,
        20, 129, 69, 24, 73, 0, 6, 200, 92, 33, 71, 108, 19,
    ];
    const BROTLI_GZIP_HTML: &[u8] = &[
        11, 25, 128, 31, 139, 8, 0, 0, 0, 0, 0, 0, 19, 179, 81, 76, 201, 79, 46, 169, 44, 72, 85,
        200, 40, 201, 205, 177, 179, 41, 176, 123, 220, 216, 242, 184, 113, 225, 227, 198, 41, 54,
        250, 5, 118, 0, 113, 127, 222, 211, 31, 0, 0, 0, 3,
    ];

    fn gzip(body: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        encoder.finish().unwrap()
    }

    fn zlib(body: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        encoder.finish().unwrap()
    }

    fn raw_deflate(body: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        encoder.finish().unwrap()
    }

    fn encoding_headers(encoding: &str) -> Vec<(String, String)> {
        vec![(String::from("content-encoding"), encoding.to_owned())]
    }

    fn get(url: impl Into<String>) -> Request {
        Request {
            method: String::from("GET"),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn service() -> (NetworkService, mpsc::Receiver<()>) {
        let (wake_sender, wakes) = mpsc::channel();
        let service = NetworkService::new(Arc::new(move || {
            let _ = wake_sender.send(());
        }))
        .unwrap();
        (service, wakes)
    }

    fn complete(service: &NetworkService, wakes: &mpsc::Receiver<()>) -> Completion {
        wakes.recv_timeout(Duration::from_secs(5)).unwrap();
        service.try_recv().expect("wake follows completion enqueue")
    }

    fn read_request(stream: &mut TcpStream) -> (String, Vec<String>, Vec<u8>) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            assert!(!line.is_empty(), "connection closed before request headers");
            headers.push(line.trim_end().to_owned());
        }
        let length = headers
            .iter()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        (request, headers, body)
    }

    #[test]
    fn redirects_and_cookies_are_left_to_webcore() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (request, _, _) = read_request(&mut stream);
            assert!(request.starts_with("GET /redirect "));
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: /final\r\nSet-Cookie: session=ok; Path=/\r\nSet-Cookie: second=retained; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let (request, headers, _) = read_request(&mut stream);
            // An automatic redirect would request /final instead.
            assert!(request.starts_with("GET /probe "));
            assert!(!headers.iter().any(|header| header.starts_with("cookie:")));
            stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing").unwrap();
        });
        let (service, wakes) = service();
        service.submit(42, get(format!("{origin}/redirect")));
        let completion = complete(&service, &wakes);
        assert_eq!(completion.id, 42);
        let response = completion.result.unwrap();
        assert_eq!(response.final_url, format!("{origin}/redirect"));
        assert_eq!(response.status, 302);
        assert_eq!(response.status_text, "Found");
        assert!(response.body.is_empty());
        assert!(
            response
                .headers
                .contains(&(String::from("location"), String::from("/final")))
        );
        assert_eq!(
            response
                .headers
                .iter()
                .filter(|(name, _)| name == "set-cookie")
                .count(),
            2
        );
        service.submit(43, get(format!("{origin}/probe")));
        let response = complete(&service, &wakes).result.unwrap();
        assert_eq!(response.status, 404);
        assert_eq!(response.status_text, "Not Found");
        assert_eq!(response.body, b"missing");
        server.join().unwrap();
    }

    #[test]
    fn post_body_headers_and_response_encoding_are_preserved() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/upload", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (request, headers, body) = read_request(&mut stream);
            assert!(request.starts_with("POST /upload "));
            assert!(headers.iter().any(|header| header == "x-test: upload"));
            assert!(
                headers
                    .iter()
                    .any(|header| header == "cookie: explicit=only")
            );
            assert_eq!(body, [0, 255, b'&']);
            stream.write_all(b"HTTP/1.1 201 Created\r\nContent-Type: text/html; charset=Shift_JIS\r\nContent-Length: 3\r\nConnection: close\r\n\r\n\x82\xa0!").unwrap();
        });
        let (service, wakes) = service();
        service.submit(
            7,
            Request {
                method: String::from("POST"),
                url,
                headers: vec![
                    (String::from("X-Test"), String::from("upload")),
                    (String::from("Cookie"), String::from("explicit=only")),
                ],
                body: vec![0, 255, b'&'],
            },
        );
        let response = complete(&service, &wakes).result.unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(response.body, [0x82, 0xa0, b'!']);
        assert!(response.headers.contains(&(
            String::from("content-type"),
            String::from("text/html; charset=Shift_JIS")
        )));
        server.join().unwrap();
    }

    #[test]
    fn url_user_info_is_rejected_before_tcp_and_explicit_authorization_is_preserved() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (service, wakes) = service();
        for (id, user_info, authorization) in [
            (1, "hidden:password", false),
            (2, "hidden:password", true),
            (3, ":password", false),
            (4, "hidden", false),
        ] {
            let mut request = get(format!("http://{user_info}@{address}/credentials"));
            if authorization {
                request.headers.push((
                    String::from("Authorization"),
                    String::from("Bearer explicit"),
                ));
            }
            service.submit(id, request);
            let error = complete(&service, &wakes).result.unwrap_err();
            assert!(error.contains("URL user-info is unsupported"), "{error}");
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }

        listener.set_nonblocking(false).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (_, headers, _) = read_request(&mut stream);
            assert_eq!(
                headers
                    .iter()
                    .filter(|header| header.starts_with("authorization:"))
                    .count(),
                1
            );
            assert!(
                headers
                    .iter()
                    .any(|header| header == "authorization: Bearer explicit")
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let url = format!("http://{address}/credentials");
        let mut request = get(url.clone());
        request.headers.push((
            String::from("Authorization"),
            String::from("Bearer explicit"),
        ));
        service.submit(5, request);
        let response = complete(&service, &wakes).result.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.final_url, url);
        server.join().unwrap();
    }

    #[test]
    fn header_latin1_octets_are_preserved_and_non_byte_characters_are_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/header-bytes", listener.local_addr().unwrap());
        let server_listener = listener.try_clone().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = server_listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut found = false;
            loop {
                let mut line = Vec::new();
                assert_ne!(reader.read_until(b'\n', &mut line).unwrap(), 0);
                if line == b"\r\n" {
                    break;
                }
                if line.starts_with(b"x-byte:") {
                    assert_eq!(line, b"x-byte: \xff\r\n");
                    found = true;
                }
            }
            assert!(found);
            stream.write_all(b"HTTP/1.1 200 OK\r\nX-Byte: \xff\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let (service, wakes) = service();
        let mut request = get(url.clone());
        request
            .headers
            .push((String::from("X-Byte"), String::from("ÿ")));
        service.submit(1, request);
        let response = complete(&service, &wakes).result.unwrap();
        assert!(
            response
                .headers
                .contains(&(String::from("x-byte"), String::from("ÿ")))
        );
        server.join().unwrap();

        let mut request = get(url);
        request
            .headers
            .push((String::from("X-Byte"), String::from("苺")));
        service.submit(2, request);
        let error = complete(&service, &wakes).result.unwrap_err();
        assert!(error.contains("outside Latin-1 byte range"), "{error}");
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn excessive_content_length_and_non_http_urls_fail_explicitly() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/large", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_BODY_BYTES + 1
            )
            .unwrap();
        });
        let (service, wakes) = service();
        service.submit(1, get(url));
        assert!(
            complete(&service, &wakes)
                .result
                .unwrap_err()
                .contains("64 MiB")
        );
        for (id, url) in [
            (2, "ftp://example.invalid/resource"),
            (3, "file:///resource"),
            (4, "data:,hello"),
        ] {
            service.submit(id, get(url));
            assert!(
                complete(&service, &wakes)
                    .result
                    .unwrap_err()
                    .contains("unsupported")
            );
        }
        server.join().unwrap();
    }

    #[test]
    fn cancellation_aborts_a_pending_request_and_completes_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/pending", listener.local_addr().unwrap());
        let (arrived_sender, arrived) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            arrived_sender.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nlate",
            );
        });
        let (service, wakes) = service();
        service.submit(9, get(url));
        arrived.recv_timeout(Duration::from_secs(5)).unwrap();
        service.cancel(9);
        let completion = complete(&service, &wakes);
        assert_eq!(completion.id, 9);
        assert!(completion.result.unwrap_err().contains("cancelled"));
        release.send(()).unwrap();
        server.join().unwrap();
        assert!(wakes.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(service.poll().next().is_none());
    }

    #[test]
    fn concurrent_http_finish_and_cancel_emit_one_completion_per_id() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/fast", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            while !server_stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("accept test connection: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 1024];
                // Some requests are cancelled before their headers arrive.
                while bytes.len() < 4096 {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(length) => bytes.extend_from_slice(&chunk[..length]),
                    }
                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        let _ = stream
                            .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
                        break;
                    }
                }
            }
        });
        let (service, wakes) = service();
        for id in 0..128 {
            service.submit(id, get(url.clone()));
            if id % 3 != 0 {
                thread::yield_now();
                service.cancel(id);
            }
        }
        let mut seen = std::collections::HashSet::new();
        for _ in 0..128 {
            let completion = complete(&service, &wakes);
            assert!(seen.insert(completion.id), "duplicate completion");
            match completion.result {
                Ok(response) => assert_eq!(response.status, 204),
                Err(error) => assert!(error.contains("cancelled"), "{error}"),
            }
        }
        // Cancelling a retired request also must not publish another result.
        for id in 0..128 {
            service.cancel(id);
        }
        assert!(wakes.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(service.poll().next().is_none());
        stop.store(true, Ordering::Relaxed);
        drop(service);
        server.join().unwrap();
    }

    #[test]
    fn compressed_http_entities_are_decoded_and_original_headers_are_preserved() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/encoded", listener.local_addr().unwrap());
        let cases = vec![
            (encoding_headers("gzip"), gzip(ENCODED_HTML.as_bytes())),
            (encoding_headers("br"), BROTLI_HTML.to_vec()),
            (encoding_headers("deflate"), zlib(ENCODED_HTML.as_bytes())),
            (
                encoding_headers("deflate"),
                raw_deflate(ENCODED_HTML.as_bytes()),
            ),
            (encoding_headers("gzip, br"), BROTLI_GZIP_HTML.to_vec()),
            (
                vec![
                    (String::from("content-encoding"), String::from("gzip")),
                    (String::from("content-encoding"), String::from("br")),
                ],
                BROTLI_GZIP_HTML.to_vec(),
            ),
            (
                encoding_headers("identity"),
                ENCODED_HTML.as_bytes().to_vec(),
            ),
        ];
        let expected: Vec<_> = cases
            .iter()
            .map(|(headers, body)| (headers.clone(), body.len()))
            .collect();
        let server = thread::spawn(move || {
            for (headers, body) in cases {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_request(&mut stream);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Length: {}\r\nConnection: close\r\n", body.len()).unwrap();
                for (name, value) in headers {
                    write!(stream, "{name}: {value}\r\n").unwrap();
                }
                stream.write_all(b"\r\n").unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        let (service, wakes) = service();
        for (id, (headers, encoded_size)) in expected.into_iter().enumerate() {
            service.submit(id as u64, get(url.clone()));
            let response = complete(&service, &wakes).result.unwrap();
            assert_eq!(response.body, ENCODED_HTML.as_bytes());
            assert_eq!(response.final_url, url);
            assert!(
                response
                    .headers
                    .contains(&(String::from("content-length"), encoded_size.to_string()))
            );
            let actual_encodings: Vec<_> = response
                .headers
                .iter()
                .filter(|(name, _)| name == "content-encoding")
                .cloned()
                .collect();
            assert_eq!(actual_encodings, headers);
        }
        server.join().unwrap();
    }

    #[test]
    fn malformed_compressed_entities_and_unsupported_encodings_are_errors() {
        let gzip_body = gzip(ENCODED_HTML.as_bytes());
        let mut wrong_crc = gzip_body.clone();
        let crc_offset = wrong_crc.len() - 8;
        wrong_crc[crc_offset] ^= 1;
        let mut gzip_trailing = gzip_body.clone();
        gzip_trailing.push(0);
        let mut zlib_bad_checksum = zlib(ENCODED_HTML.as_bytes());
        *zlib_bad_checksum.last_mut().unwrap() ^= 1;
        let mut raw_trailing = raw_deflate(ENCODED_HTML.as_bytes());
        raw_trailing.push(0);
        let mut brotli_trailing = BROTLI_HTML.to_vec();
        brotli_trailing.push(0);
        for (encoding, body) in [
            ("gzip", gzip_body[..gzip_body.len() - 1].to_vec()),
            ("gzip", wrong_crc),
            ("gzip", gzip_trailing),
            ("deflate", zlib_bad_checksum),
            ("deflate", raw_trailing),
            ("br", BROTLI_HTML[..BROTLI_HTML.len() - 1].to_vec()),
            ("br", brotli_trailing),
            ("zstd", ENCODED_HTML.as_bytes().to_vec()),
            ("gzip,,br", BROTLI_GZIP_HTML.to_vec()),
        ] {
            assert!(
                decode_entity(&encoding_headers(encoding), body, MAX_BODY_BYTES).is_err(),
                "{encoding}"
            );
        }
        // A nonstandard large-window Brotli stream generated with LGWIN=25.
        let large_window = vec![
            17, 89, 120, 0, 224, 55, 82, 178, 150, 103, 234, 64, 66, 202, 80, 205, 227, 3, 55, 174,
            17, 80, 4, 22, 97, 36, 1, 48, 128, 140, 43, 228, 136, 109, 2,
        ];
        assert!(decode_entity(&encoding_headers("br"), large_window, MAX_BODY_BYTES).is_err());
    }

    #[test]
    fn decoded_size_and_nesting_are_bounded_and_gzip_members_are_validated() {
        let plain = vec![b'a'; 4096];
        let compressed = gzip(&plain);
        assert!(compressed.len() < 128);
        assert!(decode_entity(&encoding_headers("gzip"), compressed.clone(), 128).is_err());
        assert_eq!(
            decode_entity(&encoding_headers("gzip"), compressed, plain.len()).unwrap(),
            plain
        );
        let mut members = gzip(&ENCODED_HTML.as_bytes()[..5]);
        members.extend_from_slice(&gzip(&ENCODED_HTML.as_bytes()[5..]));
        assert_eq!(
            decode_entity(&encoding_headers("gzip"), members.clone(), MAX_BODY_BYTES).unwrap(),
            ENCODED_HTML.as_bytes()
        );
        *members.last_mut().unwrap() ^= 1;
        assert!(decode_entity(&encoding_headers("gzip"), members, MAX_BODY_BYTES).is_err());
        let layers = std::iter::repeat_n("identity", MAX_CONTENT_ENCODINGS + 1)
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            decode_entity(
                &encoding_headers(&layers),
                ENCODED_HTML.as_bytes().to_vec(),
                MAX_BODY_BYTES
            )
            .is_err()
        );
    }

    #[test]
    fn head_and_not_modified_responses_do_not_decode_a_missing_entity() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/no-entity", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (request, _, _) = read_request(&mut stream);
            assert!(request.starts_with("HEAD "));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 100000000\r\nConnection: close\r\n\r\n").unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            stream.write_all(b"HTTP/1.1 304 Not Modified\r\nContent-Encoding: br\r\nContent-Length: 100000000\r\nConnection: close\r\n\r\n").unwrap();
        });
        let (service, wakes) = service();
        let mut request = get(url.clone());
        request.method = String::from("HEAD");
        service.submit(1, request);
        let response = complete(&service, &wakes).result.unwrap();
        assert_eq!(response.status, 200);
        assert!(response.body.is_empty());
        service.submit(2, get(url));
        let response = complete(&service, &wakes).result.unwrap();
        assert_eq!(response.status, 304);
        assert!(response.body.is_empty());
        server.join().unwrap();
    }

    #[test]
    fn body_bound_rejects_chunks_without_partially_appending_them() {
        let mut body = b"12".to_vec();
        assert!(append_body(&mut body, b"345", 4).is_err());
        assert_eq!(body, b"12");
        append_body(&mut body, b"34", 4).unwrap();
        assert_eq!(body, b"1234");
        assert!(append_body(&mut body, b"5", 4).is_err());
    }

    #[test]
    fn invalid_method_and_header_values_are_not_sent() {
        let (service, wakes) = service();
        let mut request = get("http://127.0.0.1:9/");
        request.method = String::from("bad method");
        service.submit(1, request);
        assert!(
            complete(&service, &wakes)
                .result
                .unwrap_err()
                .contains("invalid HTTP method")
        );
        let mut request = get("http://127.0.0.1:9/");
        request.headers.push((
            String::from("X-Test"),
            String::from("injected\r\nHeader: value"),
        ));
        service.submit(2, request);
        assert!(
            complete(&service, &wakes)
                .result
                .unwrap_err()
                .contains("invalid request header value")
        );
    }
}
