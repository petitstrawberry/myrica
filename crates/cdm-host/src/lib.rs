//! Scarlet-native client for the widevine-host CDM service process.
//!
//! The library owns the host lifecycle: it creates the shared memory region
//! and the socket pair, spawns widevine-host inside the linux-aarch64 view
//! with the handles mapped to fixed descriptor numbers, and speaks the control
//! protocol defined in tools/widevine-host/protocol.h.
//!
//! myrica's WebKit EME integration uses this module from the Scarlet side: the
//! browser never loads the CDM itself.

pub mod setup;

use std::collections::VecDeque;
use std::io::{Read, Write};

use scarlet_os::environment::{Environment, HandleMapping};
use scarlet_os::handle::capability::memory_mapping::{flags as map_flags, munmap, prot};
use scarlet_os::handle::{Handle, HandleError, HandleResult};
use scarlet_os::ipc::{SharedMemory, permissions};
use scarlet_os::socket::Socket;
use scarlet_sys::{Syscall, syscall1};

pub const WVH_MAGIC: u32 = 0x3156_4857;
pub const WVH_VERSION: u32 = 2;

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
pub const CMD_DEINITIALIZE_VIDEO: u32 = 13;
pub const CMD_SET_SERVER_CERTIFICATE: u32 = 12;

pub const EV_READY: u32 = 100;
pub const EV_RESULT: u32 = 101;
pub const EV_SESSION: u32 = 102;
pub const EV_MESSAGE: u32 = 103;
pub const EV_KEYS: u32 = 104;
pub const EV_EXPIRATION: u32 = 105;
pub const EV_SESSION_CLOSED: u32 = 106;
pub const EV_FRAME: u32 = 107;

pub const STATUS_OK: i32 = 0;
pub const STATUS_NEED_MORE_DATA: i32 = 1;
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
    pub request_id: u32,
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
struct MappingGuard {
    base: usize,
    size: usize,
}
impl Drop for MappingGuard {
    fn drop(&mut self) {
        // The region is private to this client and no sample borrow escapes IPC.
        let _ = unsafe { munmap(self.base, self.size) };
    }
}

pub struct CdmHost {
    incoming: Vec<u8>,
    deferred: VecDeque<HostEvent>,
    socket: Socket,
    _mapping: MappingGuard,
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
        if options.slot_count < 2
            || options.slot_count > 64
            || options.slot_size == 0
            || region_size > 64 * 1024 * 1024
        {
            return Err(CdmHostError::Protocol("invalid shared region layout"));
        }
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

        let mapping = MappingGuard {
            base,
            size: region_size,
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
            incoming: Vec::new(),
            deferred: VecDeque::new(),
            socket: client_socket,
            _mapping: mapping,
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
        let (kind, payload) = self
            .next_frame(true)?
            .ok_or(CdmHostError::Protocol("missing control frame"))?;
        if kind != EV_READY || payload.len() != 16 {
            return Err(CdmHostError::Protocol("unexpected READY frame"));
        }
        let mut offset = 0;
        let version = take_u32(&payload, &mut offset).unwrap();
        let header = take_u32(&payload, &mut offset).unwrap();
        let slots = take_u32(&payload, &mut offset).unwrap();
        let slot_size = take_u32(&payload, &mut offset).unwrap();
        if version != WVH_VERSION
            || header != SHM_HEADER_BYTES as u32
            || slot_size != self.slot_size
            || SHM_HEADER_BYTES + slots as usize * slot_size as usize != self.region_size
        {
            return Err(CdmHostError::Protocol("host shared region layout mismatch"));
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

    /// Install a service certificate for Widevine privacy-mode requests.
    pub fn set_server_certificate(&mut self, certificate: &[u8]) -> Result<()> {
        if certificate.is_empty() || certificate.len() > 1024 * 1024 {
            return Err(CdmHostError::Protocol("invalid server certificate length"));
        }
        let request_id = self.take_request_id();
        let mut payload = Vec::new();
        push_u32(&mut payload, request_id);
        push_u32(&mut payload, certificate.len() as u32);
        payload.extend_from_slice(certificate);
        self.socket
            .write_all(&frame(CMD_SET_SERVER_CERTIFICATE, &payload))?;
        let (status, _, _) = self.wait_result(request_id, None)?;
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
        self.socket
            .write_all(&frame(CMD_CREATE_SESSION, &payload))?;

        let mut session_id = String::new();
        let (status, _, _) = self.wait_result(
            request_id,
            Some(&mut |event: HostEvent| {
                match &event {
                    HostEvent::Session {
                        request_id: id,
                        session_id: value,
                    } if *id == request_id => session_id = value.clone(),
                    _ => {}
                }
                events.push(event);
            }),
        )?;
        if status != STATUS_OK {
            return Err(CdmHostError::Status(status));
        }
        let message = events.iter().find_map(|event| match event {
            HostEvent::Message {
                session_id: id,
                data,
                ..
            } if id == &session_id => Some(data.clone()),
            _ => None,
        });
        if session_id.is_empty() || message.is_none() {
            return Err(CdmHostError::Protocol(
                "session result omitted its initial message",
            ));
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
        self.socket
            .write_all(&frame(CMD_UPDATE_SESSION, &payload))?;

        let mut keys = Vec::new();
        let (status, _, _) = self.wait_result(
            request_id,
            Some(&mut |event: HostEvent| {
                if let HostEvent::Keys {
                    session_id: id,
                    keys: found,
                } = &event
                    && id == session_id
                {
                    keys = found.clone();
                }
                events.push(event);
            }),
        )?;
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

    /// Preserve the encoded access unit and its clear/cipher boundaries. The
    /// CDM needs the clear codec headers as well as the encrypted payload.
    pub fn decrypt(&mut self, request: &DecryptRequest<'_>) -> Result<Vec<u8>> {
        if request.scheme != SCHEME_CENC {
            return Err(CdmHostError::Status(STATUS_UNSUPPORTED));
        }
        let regions = if request.subsamples.is_empty() {
            vec![Subsample {
                clear_bytes: 0,
                cipher_bytes: u32::try_from(request.sample.len())
                    .map_err(|_| CdmHostError::Protocol("sample too large"))?,
            }]
        } else {
            request.subsamples.to_vec()
        };
        let mut position = 0usize;
        let mut cipher_bytes = 0usize;
        for region in &regions {
            let size = region.clear_bytes as usize + region.cipher_bytes as usize;
            if size > request.sample.len().saturating_sub(position) {
                return Err(CdmHostError::Protocol("subsample map exceeds sample"));
            }
            cipher_bytes += region.cipher_bytes as usize;
            position += size;
        }
        if position != request.sample.len() {
            return Err(CdmHostError::Protocol(
                "subsample map does not cover sample",
            ));
        }
        if cipher_bytes == 0 {
            return Ok(request.sample.to_vec());
        }
        self.decrypt_region(request.sample, &regions, request)
    }

    fn decrypt_region(
        &mut self,
        chunk: &[u8],
        regions: &[Subsample],
        request: &DecryptRequest<'_>,
    ) -> Result<Vec<u8>> {
        let request_id = self.take_request_id();
        let payload = Self::input_payload(request_id, request, regions);

        self.write_slot(0, chunk)?;
        self.socket.write_all(&frame(CMD_DECRYPT, &payload))?;
        let (status, slot, size) = self.wait_result(request_id, None)?;
        if status != STATUS_OK {
            return Err(CdmHostError::Status(status));
        }
        let out = self.read_slot_vec(slot, size as usize);
        self.socket
            .write_all(&frame(CMD_RELEASE, &slot.to_le_bytes()))?;
        let out = out?;
        if out.len() != chunk.len() {
            return Err(CdmHostError::Protocol("decrypted sample size changed"));
        }
        Ok(out)
    }

    fn input_payload(
        request_id: u32,
        request: &DecryptRequest<'_>,
        regions: &[Subsample],
    ) -> Vec<u8> {
        let mut payload = Vec::new();
        push_u32(&mut payload, request_id);
        push_u32(&mut payload, 0);
        push_u32(&mut payload, 0);
        push_u32(&mut payload, request.sample.len() as u32);
        push_u32(&mut payload, request.key_id.len() as u32);
        payload.extend_from_slice(request.key_id);
        push_u32(&mut payload, request.iv.len() as u32);
        payload.extend_from_slice(request.iv);
        push_u32(&mut payload, regions.len() as u32);
        for region in regions {
            push_u32(&mut payload, region.clear_bytes);
            push_u32(&mut payload, region.cipher_bytes);
        }
        push_u32(&mut payload, request.scheme);
        payload.extend_from_slice(&request.timestamp.to_le_bytes());

        payload
    }

    /// Configure the CDM's H.264 decoder; parameter sets are sent in Annex B input.
    pub fn configure_video(&mut self, profile: u32, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 || width > 1920 || height > 1080 {
            return Err(CdmHostError::Protocol("unsupported CDM video dimensions"));
        }
        let mut payload = Vec::new();
        for value in [2, profile, 2, width, height, 0] {
            push_u32(&mut payload, value);
        }
        self.socket
            .write_all(&frame(CMD_CONFIGURE_VIDEO, &payload))?;
        let (status, _, _) = self.wait_result(0, None)?;
        if status == STATUS_OK {
            Ok(())
        } else {
            Err(CdmHostError::Status(status))
        }
    }

    pub fn deinitialize_video(&mut self) -> Result<()> {
        let id = self.take_request_id();
        self.socket
            .write_all(&frame(CMD_DEINITIALIZE_VIDEO, &id.to_le_bytes()))?;
        let (status, _, _) = self.wait_result(id, None)?;
        if status == STATUS_OK {
            Ok(())
        } else {
            Err(CdmHostError::Status(status))
        }
    }

    /// A consumed access unit can produce no picture until the decoder has
    /// enough reference pictures. Empty unencrypted input drains at EOS.
    pub fn decode_video(
        &mut self,
        request: &DecryptRequest<'_>,
    ) -> Result<Option<(FrameInfo, Vec<u8>)>> {
        let id = self.take_request_id();
        self.write_slot(0, request.sample)?;
        let payload = Self::input_payload(id, request, request.subsamples);
        self.socket.write_all(&frame(CMD_DECODE, &payload))?;
        let (status, slot, size) = self.wait_result(id, None)?;
        let position = self
            .deferred
            .iter()
            .position(|event| matches!(event, HostEvent::Frame(info) if info.request_id == id));
        let picture = position.and_then(|pos| self.deferred.remove(pos));
        if let Some(HostEvent::Frame(info)) = picture {
            let output = self.read_slot_vec(info.slot, info.size as usize);
            self.socket
                .write_all(&frame(CMD_RELEASE, &info.slot.to_le_bytes()))?;
            if status != STATUS_OK || info.slot != slot || info.size != size {
                return Err(CdmHostError::Protocol("CDM frame/result mismatch"));
            }
            return Ok(Some((info, output?)));
        }
        if status == STATUS_NEED_MORE_DATA {
            return Ok(None);
        }
        if status == STATUS_OK {
            return Err(CdmHostError::Protocol("CDM decoded without a frame event"));
        }
        Err(CdmHostError::Status(status))
    }

    /// Stop the host process.
    pub fn shutdown(self) {
        drop(self);
    }

    fn take_request_id(&mut self) -> u32 {
        self.next_request_id += 1;
        self.next_request_id
    }

    fn slot_start(&self, slot: u32, size: usize) -> Result<usize> {
        let count = (self.region_size - SHM_HEADER_BYTES) / self.slot_size as usize;
        if slot as usize >= count || size > self.slot_size as usize {
            return Err(CdmHostError::Protocol("shared slot range exceeds region"));
        }
        Ok(self.base + SHM_HEADER_BYTES + slot as usize * self.slot_size as usize)
    }

    fn write_slot(&self, slot: u32, data: &[u8]) -> Result<()> {
        let start = self.slot_start(slot, data.len())?;
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), start as *mut u8, data.len());
        }
        Ok(())
    }

    fn read_slot_vec(&self, slot: u32, size: usize) -> Result<Vec<u8>> {
        let start = self.slot_start(slot, size)?;
        let mut out = vec![0u8; size];
        unsafe {
            std::ptr::copy_nonoverlapping(start as *const u8, out.as_mut_ptr(), size);
        }
        Ok(out)
    }

    /// Read frames until the result for the request arrives, handing every
    /// other event to the observer.
    fn wait_result(
        &mut self,
        request_id: u32,
        mut observer: Option<&mut dyn FnMut(HostEvent)>,
    ) -> Result<(i32, u32, u32)> {
        loop {
            let (kind, payload) = self
                .next_frame(true)?
                .ok_or(CdmHostError::Protocol("missing control frame"))?;
            if kind == EV_RESULT {
                let mut offset = 0usize;
                let id = take_u32(&payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated result"))?;
                let status = take_u32(&payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated result"))?
                    as i32;
                let value0 = take_u32(&payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated result"))?;
                let value1 = take_u32(&payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated result"))?;
                if id == request_id {
                    return Ok((status, value0, value1));
                }
                continue;
            }
            let event = self.decode_event(kind, &payload)?;
            if let Some(callback) = observer.as_deref_mut() {
                callback(event);
            } else {
                if self.deferred.len() >= 64 {
                    return Err(CdmHostError::Protocol("CDM event queue full"));
                }
                self.deferred.push_back(event);
            }
        }
    }

    /// Drain spontaneous CDM messages/key changes without waiting for another
    /// request. Partial stream frames stay buffered for the next poll/command.
    pub fn poll_events(&mut self) -> Result<Vec<HostEvent>> {
        let mut events: Vec<_> = self.deferred.drain(..).collect();
        while events.len() < 64 {
            let Some((kind, payload)) = self.next_frame(false)? else {
                break;
            };
            if kind == EV_RESULT {
                return Err(CdmHostError::Protocol("unsolicited CDM result"));
            }
            events.push(self.decode_event(kind, &payload)?);
        }
        Ok(events)
    }

    fn next_frame(&mut self, wait: bool) -> Result<Option<(u32, Vec<u8>)>> {
        if !wait {
            self.socket
                .set_nonblocking(true)
                .map_err(|_| CdmHostError::Protocol("cannot poll CDM socket"))?;
        }
        let result = (|| {
            loop {
                if self.incoming.len() >= 16 {
                    let header = &self.incoming[..16];
                    if u32::from_le_bytes(header[..4].try_into().unwrap()) != WVH_MAGIC {
                        return Err(CdmHostError::Protocol("bad frame magic"));
                    }
                    let kind = u32::from_le_bytes(header[4..8].try_into().unwrap());
                    let size = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
                    if size > 16 * 1024 * 1024 {
                        return Err(CdmHostError::Protocol("control frame too large"));
                    }
                    if self.incoming.len() >= 16 + size {
                        let payload = self.incoming[16..16 + size].to_vec();
                        self.incoming.drain(..16 + size);
                        return Ok(Some((kind, payload)));
                    }
                }
                let mut bytes = [0; 8192];
                match self.socket.read(&mut bytes) {
                    Ok(0) => return Err(CdmHostError::Protocol("CDM control socket closed")),
                    Ok(count) => self.incoming.extend_from_slice(&bytes[..count]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && !wait => {
                        return Ok(None);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                }
            }
        })();
        if !wait {
            self.socket
                .set_nonblocking(false)
                .map_err(|_| CdmHostError::Protocol("cannot restore CDM socket mode"))?;
        }
        result
    }

    fn decode_event(&mut self, kind: u32, payload: &[u8]) -> Result<HostEvent> {
        let mut offset = 0usize;
        Ok(match kind {
            EV_SESSION => {
                let request_id = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                let session_id = take_string(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event string"))?;
                HostEvent::Session {
                    request_id,
                    session_id,
                }
            }
            EV_MESSAGE => {
                let session_id = take_string(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event string"))?;
                let message_type = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                let slot = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                let message_offset = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                let size = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                let data = self.read_slot_vec(slot, message_offset as usize + size as usize)?;
                let data = data[message_offset as usize..].to_vec();
                self.socket
                    .write_all(&frame(CMD_RELEASE, &slot.to_le_bytes()))?;
                HostEvent::Message {
                    session_id,
                    message_type,
                    data,
                }
            }
            EV_KEYS => {
                let session_id = take_string(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event string"))?;
                let count = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                if count as usize > (payload.len() - offset) / 8 {
                    return Err(CdmHostError::Protocol("invalid key status count"));
                }
                let mut keys = Vec::new();
                for _ in 0..count {
                    let size = take_u32(payload, &mut offset)
                        .ok_or(CdmHostError::Protocol("truncated event integer"))?
                        as usize;
                    if offset + size > payload.len() {
                        return Err(CdmHostError::Protocol("truncated key id"));
                    }
                    let key_id = payload[offset..offset + size].to_vec();
                    offset += size;
                    let status = take_u32(payload, &mut offset)
                        .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                    keys.push(KeyStatus { key_id, status });
                }
                HostEvent::Keys { session_id, keys }
            }
            EV_EXPIRATION => {
                let session_id = take_string(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event string"))?;
                let mut raw = [0u8; 8];
                if offset + 8 > payload.len() {
                    return Err(CdmHostError::Protocol("truncated event timestamp"));
                }
                raw.copy_from_slice(&payload[offset..offset + 8]);
                HostEvent::Expiration {
                    session_id,
                    expiry: f64::from_bits(u64::from_le_bytes(raw)),
                }
            }
            EV_SESSION_CLOSED => HostEvent::SessionClosed {
                session_id: take_string(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event string"))?,
            },
            EV_FRAME => {
                let mut info = FrameInfo::default();
                info.request_id = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated frame request id"))?;
                info.format = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                info.width = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                info.height = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                info.slot = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                info.size = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                info.stride_y = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                info.stride_u = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                info.stride_v = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                info.offset_u = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                info.offset_v = take_u32(payload, &mut offset)
                    .ok_or(CdmHostError::Protocol("truncated event integer"))?;
                let mut raw = [0u8; 8];
                if offset + 8 > payload.len() {
                    return Err(CdmHostError::Protocol("truncated event timestamp"));
                }
                raw.copy_from_slice(&payload[offset..offset + 8]);
                info.timestamp = i64::from_le_bytes(raw);
                HostEvent::Frame(info)
            }
            other => HostEvent::Unknown(other),
        })
    }

    pub fn slot_size(&self) -> u32 {
        self.slot_size
    }

    pub fn region_size(&self) -> usize {
        self.region_size
    }
}

impl Drop for CdmHost {
    fn drop(&mut self) {
        let _ = self.socket.write_all(&frame(CMD_SHUTDOWN, &[]));
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
    if length > 16 * 1024 * 1024 {
        return Err(CdmHostError::Protocol("control frame too large"));
    }
    if magic != WVH_MAGIC {
        return Err(CdmHostError::Protocol("bad frame magic"));
    }
    let mut payload = vec![0u8; length];
    if length > 0 {
        socket.read_exact(&mut payload)?;
    }
    Ok((kind, payload))
}
