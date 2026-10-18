//! Scarlet-native client for the widevine-host CDM service process.
//!
//! The library owns the host lifecycle: it creates the shared memory region
//! and the socket pair, spawns widevine-host inside the linux-aarch64 view
//! with the handles mapped to fixed descriptor numbers, and speaks the control
//! protocol defined in tools/widevine-host/protocol.h.
//!
//! myrica's WebKit EME integration uses this module from the Scarlet side: the
//! browser never loads the CDM itself.

use std::io::{Read, Write};

use scarlet_os::environment::{Environment, HandleMapping};
use scarlet_os::handle::capability::memory_mapping::{flags as map_flags, prot};
use scarlet_os::handle::{Handle, HandleError, HandleResult};
use scarlet_os::ipc::{permissions, SharedMemory};
use scarlet_os::socket::Socket;
use scarlet_sys::{Syscall, syscall1};

pub const WVH_MAGIC: u32 = 0x3156_4857;
pub const WVH_VERSION: u32 = 1;

pub const CMD_HELLO: u32 = 1;
pub const CMD_INIT: u32 = 2;
pub const CMD_CREATE_SESSION: u32 = 3;
pub const CMD_UPDATE_SESSION: u32 = 4;
pub const CMD_CLOSE_SESSION: u32 = 5;
pub const CMD_REMOVE_SESSION: u32 = 6;
pub const CMD_DECRYPT: u32 = 7;
pub const CMD_SHUTDOWN: u32 = 8;
pub const CMD_RELEASE: u32 = 9;
pub const CMD_CONFIGURE_VIDEO: u32 = 10;
pub const CMD_DECODE: u32 = 11;

pub const EV_READY: u32 = 100;
pub const EV_RESULT: u32 = 101;
pub const EV_SESSION: u32 = 102;
pub const EV_MESSAGE: u32 = 103;
pub const EV_KEYS: u32 = 104;
pub const EV_EXPIRATION: u32 = 105;
pub const EV_SESSION_CLOSED: u32 = 106;
pub const EV_FRAME: u32 = 107;

pub const STATUS_OK: i32 = 0;
pub const STATUS_ERROR: i32 = -1;
pub const STATUS_NO_KEY: i32 = -2;
pub const STATUS_DECRYPT_ERROR: i32 = -3;
pub const STATUS_UNSUPPORTED: i32 = -4;
pub const STATUS_TIMEOUT: i32 = -5;

pub const SESSION_TEMPORARY: u32 = 0;
pub const SESSION_PERSISTENT_LICENSE: u32 = 1;

pub const INIT_DATA_CENC: u32 = 0;
pub const INIT_DATA_KEYIDS: u32 = 1;
pub const INIT_DATA_WEBM: u32 = 2;

pub const MESSAGE_LICENSE_REQUEST: u32 = 0;
pub const MESSAGE_LICENSE_RENEWAL: u32 = 1;
pub const MESSAGE_LICENSE_RELEASE: u32 = 2;
pub const MESSAGE_INDIVIDUALIZATION_REQUEST: u32 = 3;

pub const SCHEME_UNENCRYPTED: u32 = 0;
pub const SCHEME_CENC: u32 = 1;
pub const SCHEME_CBCS: u32 = 2;

pub const SHM_HEADER_BYTES: usize = 4096;

/// Everything the host process needs to start.
pub struct CdmHostOptions {
    /// Path of widevine-host as seen inside the linux-aarch64 view.
    pub host_path: String,
    /// Path of the CDM shared library as seen inside the linux-aarch64 view.
    pub cdm_path: String,
    /// Optional LD_LIBRARY_PATH for the host process (libnspr4 and friends).
    pub host_ld_path: Option<String>,
    pub slot_count: u32,
    pub slot_size: u32,
}

impl CdmHostOptions {
    pub fn new(host_path: impl Into<String>, cdm_path: impl Into<String>) -> Self {
        Self {
            host_path: host_path.into(),
            cdm_path: cdm_path.into(),
            host_ld_path: None,
            slot_count: 4,
            slot_size: 1024 * 1024,
        }
    }

    pub fn region_size(&self) -> usize {
        SHM_HEADER_BYTES + self.slot_count as usize * self.slot_size as usize
    }
}

#[derive(Debug)]
pub enum CdmHostError {
    Handle(HandleError),
    Io(std::io::Error),
    Protocol(&'static str),
    Status(i32),
    Spawn(String),
}

impl From<HandleError> for CdmHostError {
    fn from(error: HandleError) -> Self {
        CdmHostError::Handle(error)
    }
}

impl From<std::io::Error> for CdmHostError {
    fn from(error: std::io::Error) -> Self {
        CdmHostError::Io(error)
    }
}

pub type Result<T> = core::result::Result<T, CdmHostError>;

/// One key as the CDM reports it in a key status change.
#[derive(Debug, Clone)]
pub struct KeyStatus {
    pub key_id: Vec<u8>,
    pub status: u32,
}

/// Events the host pushes asynchronously.
#[derive(Debug, Clone)]
pub enum HostEvent {
    Session {
        request_id: u32,
        session_id: String,
    },
    Message {
        session_id: String,
        message_type: u32,
        data: Vec<u8>,
    },
    Keys {
        session_id: String,
        keys: Vec<KeyStatus>,
    },
    Expiration {
        session_id: String,
        expiry: f64,
    },
    SessionClosed {
        session_id: String,
    },
    Frame(FrameInfo),
    Unknown(u32),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FrameInfo {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub slot: u32,
    pub size: u32,
    pub stride_y: u32,
    pub stride_u: u32,
    pub stride_v: u32,
    pub offset_u: u32,
    pub offset_v: u32,
    pub timestamp: i64,
}

/// One subsample region of a CENC sample.
#[derive(Debug, Clone, Copy)]
pub struct Subsample {
    pub clear_bytes: u32,
    pub cipher_bytes: u32,
}

/// A decryption request for one sample.
pub struct DecryptRequest<'a> {
    pub sample: &'a [u8],
    pub key_id: &'a [u8],
    pub iv: &'a [u8],
    pub subsamples: &'a [Subsample],
    pub scheme: u32,
    pub timestamp: i64,
}

/// The CDM host process plus the control channel and shared region it uses.
pub struct CdmHost {
    socket: Socket,
    #[allow(dead_code)]
    memory: SharedMemory,
    base: usize,
    slot_size: u32,
    region_size: usize,
    next_request_id: u32,
}

impl CdmHost {
    /// Create the shared region and the socket pair, spawn the host in the
    /// linux-aarch64 view, and run the HELLO/READY handshake.
    pub fn spawn(options: &CdmHostOptions) -> Result<Self> {
        let region_size = options.region_size();
        let memory = SharedMemory::create(region_size, permissions::READ_WRITE)
            .map_err(|_| CdmHostError::Protocol("shared memory create failed"))?;
        let base = unsafe {
            memory
                .as_handle()
                .as_memory_mapping()
                .map_err(CdmHostError::Handle)?
                .mmap(
                    0,
                    region_size,
                    prot::READ | prot::WRITE,
                    map_flags::SHARED,
                    0,
                )
                .map_err(|_| CdmHostError::Protocol("shared memory mmap failed"))?
        };

        let (client_socket, host_socket) =
            Socket::pair().map_err(|_| CdmHostError::Protocol("socketpair failed"))?;

        let environment = Environment::current()?;
        let view = environment.root("linux-aarch64")?;
        let executable = view
            .open(&options.host_path, 0)
            .map_err(|_| CdmHostError::Spawn("cannot open the host executable".into()))?;

        let slot_count = options.slot_count.to_string();
        let slot_size = options.slot_size.to_string();
        let host_argv = [
            "widevine-host",
            "--cdm",
            options.cdm_path.as_str(),
            "--fd",
            "3",
            "--shm-fd",
            "4",
            "--slot-count",
            slot_count.as_str(),
            "--slot-size",
            slot_size.as_str(),
        ];

        let stdin = duplicate_stdio(0)?;
        let stdout = duplicate_stdio(1)?;
        let stderr = duplicate_stdio(2)?;
        let mappings = [
            HandleMapping {
                source: &stdin,
                target: 0,
            },
            HandleMapping {
                source: &stdout,
                target: 1,
            },
            HandleMapping {
                source: &stderr,
                target: 2,
            },
            HandleMapping {
                source: host_socket.as_handle(),
                target: 3,
            },
            HandleMapping {
                source: memory.as_handle(),
                target: 4,
            },
        ];

        let mut host_env_owned: Vec<String> = Vec::new();
        if let Some(path) = &options.host_ld_path {
            host_env_owned.push(format!("LD_LIBRARY_PATH={path}"));
        }
        let host_envp: Vec<&str> = host_env_owned.iter().map(String::as_str).collect();

        environment
            .spawn_with_abi(
                "linux-aarch64",
                &executable,
                &host_argv,
                &host_envp,
                "/",
                &mappings,
            )
            .map_err(|error| CdmHostError::Spawn(format!("spawn failed: {error:?}")))?;
        drop(host_socket);

        let mut host = Self {
            socket: client_socket,
            memory,
            base,
            slot_size: options.slot_size,
            region_size,
            next_request_id: 0,
        };
        host.handshake()?;
        Ok(host)
    }

    fn handshake(&mut self) -> Result<()> {
        let hello = frame(CMD_HELLO, &WVH_VERSION.to_le_bytes());
        self.socket.write_all(&hello)?;
        let (kind, payload) = read_frame(&mut self.socket)?;
        if kind != EV_READY || payload.len() < 16 {
            return Err(CdmHostError::Protocol("unexpected READY frame"));
        }
        Ok(())
    }

    /// Ask the CDM to initialize; Ok(()) means the CDM reported success.
    pub fn initialize(
        &mut self,
        allow_distinctive_identifier: bool,
        allow_persistent_state: bool,
        use_hw_secure_codecs: bool,
    ) -> Result<()> {
        let mut payload = Vec::new();
        push_u32(&mut payload, u32::from(allow_distinctive_identifier));
        push_u32(&mut payload, u32::from(allow_persistent_state));
        push_u32(&mut payload, u32::from(use_hw_secure_codecs));
        self.socket.write_all(&frame(CMD_INIT, &payload))?;
        let (status, _, _) = self.wait_result(REQUEST_INIT, None)?;
        if status == STATUS_OK {
            Ok(())
        } else {
            Err(CdmHostError::Status(status))
        }
    }

    /// Create a session and return its id plus the first message the CDM
    /// produced for it, which is normally the licence request.
    pub fn create_session(
        &mut self,
        init_data: &[u8],
        session_type: u32,
        init_data_type: u32,
        events: &mut Vec<HostEvent>,
    ) -> Result<(String, Option<Vec<u8>>)> {
        let request_id = self.take_request_id();
        let mut payload = Vec::new();
        push_u32(&mut payload, request_id);
        push_u32(&mut payload, session_type);
        push_u32(&mut payload, init_data_type);
        push_u32(&mut payload, init_data.len() as u32);
        payload.extend_from_slice(init_data);
        self.socket.write_all(&frame(CMD_CREATE_SESSION, &payload))?;

        let mut session_id = String::new();
        let mut message: Option<Vec<u8>> = None;
        let (status, _, _) =
            self.wait_result(request_id, Some(&mut |event: HostEvent| {
                match &event {
                    HostEvent::Session {
                        session_id: id, ..
                    } => session_id = id.clone(),
                    HostEvent::Message { data, .. } => {
                        if message.is_none() {
                            message = Some(data.clone());
                        }
                    }
                    _ => {}
                }
                events.push(event);
            }))?;
        if status != STATUS_OK {
            return Err(CdmHostError::Status(status));
        }
        Ok((session_id, message))
    }

    /// Apply a licence (or provisioning) response and return the key statuses
    /// the CDM reported while the update was in flight.
    pub fn update_session(
        &mut self,
        session_id: &str,
        response: &[u8],
        events: &mut Vec<HostEvent>,
    ) -> Result<Vec<KeyStatus>> {
        let request_id = self.take_request_id();
        let mut payload = Vec::new();
        push_u32(&mut payload, request_id);
        push_string(&mut payload, session_id);
        push_u32(&mut payload, response.len() as u32);
        payload.extend_from_slice(response);
        self.socket.write_all(&frame(CMD_UPDATE_SESSION, &payload))?;

        let mut keys = Vec::new();
        let (status, _, _) =
            self.wait_result(request_id, Some(&mut |event: HostEvent| {
                if let HostEvent::Keys { keys: found, .. } = &event {
                    keys = found.clone();
                }
                events.push(event);
            }))?;
        if status != STATUS_OK {
            return Err(CdmHostError::Status(status));
        }
        Ok(keys)
    }

    pub fn close_session(&mut self, session_id: &str) -> Result<()> {
        let request_id = self.take_request_id();
        let mut payload = Vec::new();
        push_u32(&mut payload, request_id);
        push_string(&mut payload, session_id);
        self.socket.write_all(&frame(CMD_CLOSE_SESSION, &payload))?;
        let (status, _, _) = self.wait_result(request_id, None)?;
        if status == STATUS_OK {
            Ok(())
        } else {
            Err(CdmHostError::Status(status))
        }
    }

    /// Decrypt one sample. The CDM refuses a subsample map with more than one
    /// entry, so every region is decrypted on its own with the sample IV; the
    /// packager restarts the keystream per region, which is what makes the two
    /// equivalent. A zero-filled success is retried once.
    pub fn decrypt(&mut self, request: &DecryptRequest<'_>) -> Result<Vec<u8>> {
        let regions: Vec<Subsample> = if request.subsamples.is_empty() {
            vec![Subsample {
                clear_bytes: 0,
                cipher_bytes: request.sample.len() as u32,
            }]
        } else {
            request.subsamples.to_vec()
        };

        let mut plaintext = vec![0u8; request.sample.len()];
        let mut position = 0usize;
        for region in &regions {
            let size = (region.clear_bytes + region.cipher_bytes) as usize;
            if position + size > request.sample.len() {
                return Err(CdmHostError::Protocol("subsample map exceeds the sample"));
            }
            if region.cipher_bytes == 0 {
                plaintext[position..position + size]
                    .copy_from_slice(&request.sample[position..position + size]);
                position += size;
                continue;
            }
            let chunk = &request.sample[position..position + size];
            let mut decrypted = self.decrypt_region(chunk, region, request)?;
            if decrypted.iter().all(|byte| *byte == 0) {
                decrypted = self.decrypt_region(chunk, region, request)?;
            }
            plaintext[position..position + size].copy_from_slice(&decrypted);
            position += size;
        }
        Ok(plaintext)
    }

    fn decrypt_region(
        &mut self,
        chunk: &[u8],
        region: &Subsample,
        request: &DecryptRequest<'_>,
    ) -> Result<Vec<u8>> {
        let request_id = self.take_request_id();
        let mut payload = Vec::new();
        push_u32(&mut payload, request_id);
        push_u32(&mut payload, 0);
        push_u32(&mut payload, 0);
        push_u32(&mut payload, chunk.len() as u32);
        push_u32(&mut payload, request.key_id.len() as u32);
        payload.extend_from_slice(request.key_id);
        push_u32(&mut payload, request.iv.len() as u32);
        payload.extend_from_slice(request.iv);
        push_u32(&mut payload, 1);
        push_u32(&mut payload, region.clear_bytes);
        push_u32(&mut payload, region.cipher_bytes);
        push_u32(&mut payload, request.scheme);
        payload.extend_from_slice(&request.timestamp.to_le_bytes());

        self.write_slot(0, chunk);
        self.socket.write_all(&frame(CMD_DECRYPT, &payload))?;
        let (status, slot, size) = self.wait_result(request_id, None)?;
        if status != STATUS_OK {
            return Err(CdmHostError::Status(status));
        }
        let out = self.read_slot_vec(slot, size as usize);
        let mut release = Vec::new();
        push_u32(&mut release, slot);
        let _ = self.socket.write_all(&frame(CMD_RELEASE, &release));
        Ok(out)
    }

    /// Stop the host process.
    pub fn shutdown(mut self) {
        let _ = self.socket.write_all(&frame(CMD_SHUTDOWN, &[]));
    }

    fn take_request_id(&mut self) -> u32 {
        self.next_request_id += 1;
        self.next_request_id
    }

    fn write_slot(&self, slot: u32, data: &[u8]) {
        let start = self.base + SHM_HEADER_BYTES + slot as usize * self.slot_size as usize;
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), start as *mut u8, data.len());
        }
    }

    fn read_slot(&self, slot: u32, out: &mut [u8]) {
        let start = self.base + SHM_HEADER_BYTES + slot as usize * self.slot_size as usize;
        unsafe {
            std::ptr::copy_nonoverlapping(start as *const u8, out.as_mut_ptr(), out.len());
        }
    }

    fn read_slot_vec(&self, slot: u32, size: usize) -> Vec<u8> {
        let mut out = vec![0u8; size];
        self.read_slot(slot, &mut out);
        out
    }

    /// Read frames until the result for the request arrives, handing every
    /// other event to the observer.
    fn wait_result(
        &mut self,
        request_id: u32,
        mut observer: Option<&mut dyn FnMut(HostEvent)>,
    ) -> Result<(i32, u32, u32)> {
        loop {
            let (kind, payload) = read_frame(&mut self.socket)?;
            if kind == EV_RESULT {
                let mut offset = 0usize;
                let id = take_u32(&payload, &mut offset).unwrap_or(0);
                let status = take_u32(&payload, &mut offset).unwrap_or(0) as i32;
                let value0 = take_u32(&payload, &mut offset).unwrap_or(0);
                let value1 = take_u32(&payload, &mut offset).unwrap_or(0);
                if id == request_id {
                    return Ok((status, value0, value1));
                }
                continue;
            }
            let event = self.decode_event(kind, &payload);
            if let Some(callback) = observer.as_deref_mut() {
                callback(event);
            }
        }
    }

    fn decode_event(&self, kind: u32, payload: &[u8]) -> HostEvent {
        let mut offset = 0usize;
        match kind {
            EV_SESSION => {
                let request_id = take_u32(payload, &mut offset).unwrap_or(0);
                let session_id = take_string(payload, &mut offset).unwrap_or_default();
                HostEvent::Session {
                    request_id,
                    session_id,
                }
            }
            EV_MESSAGE => {
                let session_id = take_string(payload, &mut offset).unwrap_or_default();
                let message_type = take_u32(payload, &mut offset).unwrap_or(0);
                let slot = take_u32(payload, &mut offset).unwrap_or(0);
                let message_offset = take_u32(payload, &mut offset).unwrap_or(0);
                let size = take_u32(payload, &mut offset).unwrap_or(0);
                let data = self.read_slot_vec(slot, message_offset as usize + size as usize);
                let data = data[message_offset as usize..].to_vec();
                HostEvent::Message {
                    session_id,
                    message_type,
                    data,
                }
            }
            EV_KEYS => {
                let session_id = take_string(payload, &mut offset).unwrap_or_default();
                let count = take_u32(payload, &mut offset).unwrap_or(0);
                let mut keys = Vec::new();
                for _ in 0..count {
                    let size = take_u32(payload, &mut offset).unwrap_or(0) as usize;
                    if offset + size > payload.len() {
                        break;
                    }
                    let key_id = payload[offset..offset + size].to_vec();
                    offset += size;
                    let status = take_u32(payload, &mut offset).unwrap_or(0);
                    keys.push(KeyStatus { key_id, status });
                }
                HostEvent::Keys { session_id, keys }
            }
            EV_EXPIRATION => {
                let session_id = take_string(payload, &mut offset).unwrap_or_default();
                let mut raw = [0u8; 8];
                if offset + 8 <= payload.len() {
                    raw.copy_from_slice(&payload[offset..offset + 8]);
                }
                HostEvent::Expiration {
                    session_id,
                    expiry: f64::from_bits(u64::from_le_bytes(raw)),
                }
            }
            EV_SESSION_CLOSED => HostEvent::SessionClosed {
                session_id: take_string(payload, &mut offset).unwrap_or_default(),
            },
            EV_FRAME => {
                let mut info = FrameInfo::default();
                info.format = take_u32(payload, &mut offset).unwrap_or(0);
                info.width = take_u32(payload, &mut offset).unwrap_or(0);
                info.height = take_u32(payload, &mut offset).unwrap_or(0);
                info.slot = take_u32(payload, &mut offset).unwrap_or(0);
                info.size = take_u32(payload, &mut offset).unwrap_or(0);
                info.stride_y = take_u32(payload, &mut offset).unwrap_or(0);
                info.stride_u = take_u32(payload, &mut offset).unwrap_or(0);
                info.stride_v = take_u32(payload, &mut offset).unwrap_or(0);
                info.offset_u = take_u32(payload, &mut offset).unwrap_or(0);
                info.offset_v = take_u32(payload, &mut offset).unwrap_or(0);
                let mut raw = [0u8; 8];
                if offset + 8 <= payload.len() {
                    raw.copy_from_slice(&payload[offset..offset + 8]);
                }
                info.timestamp = i64::from_le_bytes(raw);
                HostEvent::Frame(info)
            }
            other => HostEvent::Unknown(other),
        }
    }

    pub fn slot_size(&self) -> u32 {
        self.slot_size
    }

    pub fn region_size(&self) -> usize {
        self.region_size
    }
}

const REQUEST_INIT: u32 = 0;

fn duplicate_stdio(raw: usize) -> HandleResult<Handle> {
    // SAFETY: duplication only borrows the standard descriptor for this call.
    let duplicate = unsafe { syscall1(Syscall::HandleDuplicate, raw) };
    if duplicate == usize::MAX {
        return Err(HandleError::SystemError(-1));
    }
    // SAFETY: the successful syscall returned a new, exclusively owned handle.
    unsafe { Handle::from_raw(duplicate as i32) }
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_string(out: &mut Vec<u8>, value: &str) {
    push_u32(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}

fn take_u32(data: &[u8], offset: &mut usize) -> Option<u32> {
    if *offset + 4 > data.len() {
        return None;
    }
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&data[*offset..*offset + 4]);
    *offset += 4;
    Some(u32::from_le_bytes(bytes))
}

fn take_string(data: &[u8], offset: &mut usize) -> Option<String> {
    let size = take_u32(data, offset)? as usize;
    if *offset + size > data.len() {
        return None;
    }
    let text = String::from_utf8_lossy(&data[*offset..*offset + size]).into_owned();
    *offset += size;
    Some(text)
}

pub fn frame(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + payload.len());
    push_u32(&mut out, WVH_MAGIC);
    push_u32(&mut out, kind);
    push_u32(&mut out, 0);
    push_u32(&mut out, payload.len() as u32);
    out.extend_from_slice(payload);
    out
}

pub fn read_frame(socket: &mut Socket) -> Result<(u32, Vec<u8>)> {
    let mut header = [0u8; 16];
    socket.read_exact(&mut header)?;
    let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let kind = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let length = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
    if magic != WVH_MAGIC {
        return Err(CdmHostError::Protocol("bad frame magic"));
    }
    let mut payload = vec![0u8; length];
    if length > 0 {
        socket.read_exact(&mut payload)?;
    }
    Ok((kind, payload))
}
