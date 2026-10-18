//! C ABI the WebKit side uses to reach the Widevine CDM host process.
//!
//! The CDM itself runs inside a Linux ABI view; the fork-side CDM
//! implementation (CDMFactoryScarlet and friends) calls these functions from
//! the Scarlet-native browser process.
//! Control calls run on the CDM WorkQueue. Shared state serializes them with
//! media-worker decryption; the media session retains that state independently
//! of the C handle. Returned buffers may move to the Page owner for delivery.
//!
//! Conventions:
//! - Status is 0 on success. Negative values are bridge-level errors:
//!   -1 generic, -2 no key, -3 protocol, -4 spawn failure.
//! - Buffers written through CdmBuffer pointers are owned by Rust until the
//!   caller releases them with cdm_host_buffer_free.
//! - Packed payloads use the layouts from tools/widevine-host/protocol.h.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cdm_host::{CdmHost, CdmHostError, CdmHostOptions, DecryptRequest, HostEvent, KeyStatus, Subsample};

use crate::Bytes;

const STATUS_OK: i32 = 0;
const STATUS_ERROR: i32 = -1;
const STATUS_NO_KEY: i32 = -2;
const STATUS_PROTOCOL: i32 = -3;
const STATUS_SPAWN: i32 = -4;

/// Event kinds mirrored from the host protocol.
const EV_MESSAGE: u32 = 103;
const EV_KEYS: u32 = 104;
const EV_EXPIRATION: u32 = 105;
const EV_SESSION_CLOSED: u32 = 106;

pub(crate) fn trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("SCARLET_EME_TRACE").is_ok_and(|value| value == "1"))
}

fn trace_key_statuses(keys: &[KeyStatus]) {
    if !trace_enabled() {
        return;
    }
    let mut counts = [0usize; 8];
    for key in keys {
        counts[(key.status as usize).min(7)] += 1;
    }
    eprintln!(
        "SCARLET_CDM_KEYS total={} usable={} internal_error={} expired={} output_restricted={} output_downscaled={} pending={} released={} other={}",
        keys.len(), counts[0], counts[1], counts[2], counts[3], counts[4], counts[5], counts[6], counts[7]
    );
}

/// Owned byte buffer that crosses the C ABI boundary.
#[repr(C)]
pub struct CdmBuffer {
    pub data: *mut u8,
    pub length: usize,
}

impl CdmBuffer {
    fn from_vec(bytes: Vec<u8>) -> Self {
        let mut boxed = bytes.into_boxed_slice();
        let buffer = Self {
            data: boxed.as_mut_ptr(),
            length: boxed.len(),
        };
        std::mem::forget(boxed);
        buffer
    }
}

/// One widevine-host process plus the events it produced.
pub struct CdmHandle {
    pub(crate) shared: Arc<CdmShared>,
}

pub(crate) struct CdmShared {
    state: Mutex<CdmState>,
    revision: AtomicU64,
    poll_failed: AtomicBool,
}
struct CdmState {
    host: CdmHost,
    events: VecDeque<(u32, Vec<u8>)>,
    decrypted_samples: u64,
    decrypted_bytes: u64,
}
impl CdmShared {
    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
    pub(crate) fn decrypt(&self, request: &DecryptRequest<'_>) -> Result<Vec<u8>, CdmHostError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let clear = state.host.decrypt(request).inspect_err(|error| {
            if trace_enabled() {
                eprintln!(
                    "SCARLET_CDM_DECRYPT_ERROR error={error:?} bytes={} key_id_bytes={} iv_bytes={} subsamples={} clear_bytes={}",
                    request.sample.len(), request.key_id.len(), request.iv.len(), request.subsamples.len(),
                    request.subsamples.iter().map(|region| u64::from(region.clear_bytes)).sum::<u64>()
                );
            }
        })?;
        state.decrypted_samples += 1;
        state.decrypted_bytes += clear.len() as u64;
        static PROFILE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if (*PROFILE.get_or_init(|| std::env::var_os("SCARLET_WEBVIEW_PROFILE").is_some()) || trace_enabled())
            && (state.decrypted_samples == 1 || state.decrypted_samples % 128 == 0)
        {
            eprintln!(
                "SCARLET_CDM_DECRYPT samples={} compressed_bytes={} decoder=scarlet-native",
                state.decrypted_samples, state.decrypted_bytes
            );
        }
        Ok(clear)
    }
}

fn status_of(error: CdmHostError) -> i32 {
    match error {
        CdmHostError::Status(value) => {
            if value == cdm_host::STATUS_NO_KEY {
                STATUS_NO_KEY
            } else {
                STATUS_ERROR
            }
        }
        CdmHostError::Protocol(_) => STATUS_PROTOCOL,
        CdmHostError::Spawn(_) => STATUS_SPAWN,
        CdmHostError::Handle(_) | CdmHostError::Io(_) => STATUS_ERROR,
    }
}

unsafe fn view(bytes: Bytes) -> Vec<u8> {
    if bytes.data.is_null() || bytes.length == 0 {
        return Vec::new();
    }
    // SAFETY: native callers guarantee readable provenance for the call.
    unsafe { std::slice::from_raw_parts(bytes.data, bytes.length) }.to_vec()
}

fn pack_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn unpack_u32(data: &[u8], offset: &mut usize) -> Option<u32> {
    if *offset + 4 > data.len() {
        return None;
    }
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&data[*offset..*offset + 4]);
    *offset += 4;
    Some(u32::from_le_bytes(bytes))
}

fn encode_event(event: &HostEvent) -> (u32, Vec<u8>) {
    match event {
        HostEvent::Message {
            session_id,
            message_type,
            data,
        } => {
            let mut payload = Vec::new();
            pack_u32(&mut payload, session_id.len() as u32);
            payload.extend_from_slice(session_id.as_bytes());
            pack_u32(&mut payload, *message_type);
            pack_u32(&mut payload, data.len() as u32);
            payload.extend_from_slice(data);
            (EV_MESSAGE, payload)
        }
        HostEvent::Keys { session_id, keys } => {
            trace_key_statuses(keys);
            let mut payload = Vec::new();
            pack_u32(&mut payload, session_id.len() as u32);
            payload.extend_from_slice(session_id.as_bytes());
            pack_u32(&mut payload, keys.len() as u32);
            for key in keys {
                pack_u32(&mut payload, key.key_id.len() as u32);
                payload.extend_from_slice(&key.key_id);
                pack_u32(&mut payload, key.status);
            }
            (EV_KEYS, payload)
        }
        HostEvent::Expiration { session_id, expiry } => {
            let mut payload = Vec::new();
            pack_u32(&mut payload, session_id.len() as u32);
            payload.extend_from_slice(session_id.as_bytes());
            payload.extend_from_slice(&expiry.to_bits().to_le_bytes());
            (EV_EXPIRATION, payload)
        }
        HostEvent::SessionClosed { session_id } => {
            let mut payload = Vec::new();
            pack_u32(&mut payload, session_id.len() as u32);
            payload.extend_from_slice(session_id.as_bytes());
            (EV_SESSION_CLOSED, payload)
        }
        _ => (0, Vec::new()),
    }
}

/// Start a widevine-host process. Returns null when the host cannot start.
#[unsafe(no_mangle)]
pub extern "C" fn cdm_host_create(
    host_path: Bytes,
    cdm_path: Bytes,
    ld_path: Bytes,
    slot_count: u32,
    slot_size: u32,
) -> *mut CdmHandle {
    // SAFETY: native callers pass readable path buffers for the call.
    let host_path = String::from_utf8_lossy(&unsafe { view(host_path) }).into_owned();
    // SAFETY: same contract as above.
    let cdm_path = String::from_utf8_lossy(&unsafe { view(cdm_path) }).into_owned();
    // SAFETY: same contract as above.
    let ld_path = String::from_utf8_lossy(&unsafe { view(ld_path) }).into_owned();

    let mut options = CdmHostOptions::new(host_path, cdm_path);
    if !ld_path.is_empty() {
        options.host_ld_path = Some(ld_path);
    }
    if slot_count > 0 {
        options.slot_count = slot_count;
    }
    if slot_size > 0 {
        options.slot_size = slot_size;
    }

    match CdmHost::spawn(&options) {
        Ok(host) => Box::into_raw(Box::new(CdmHandle {
            shared: Arc::new(CdmShared {
                state: Mutex::new(CdmState {
                    host,
                    events: VecDeque::new(),
                    decrypted_samples: 0,
                    decrypted_bytes: 0,
                }),
                revision: AtomicU64::new(1),
                poll_failed: AtomicBool::new(false),
            }),
        })),
        Err(error) => {
            eprintln!("cdm_host_create failed: {error:?}");
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cdm_host_destroy(handle: *mut CdmHandle) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the pointer came from cdm_host_create and is consumed once.
    drop(unsafe { Box::from_raw(handle) });
}

#[unsafe(no_mangle)]
pub extern "C" fn cdm_host_initialize(
    handle: *mut CdmHandle,
    allow_distinctive_identifier: u32,
    allow_persistent_state: u32,
    use_hw_secure_codecs: u32,
) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return STATUS_ERROR;
    };
    let mut state = handle
        .shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match state.host.initialize(
        allow_distinctive_identifier != 0,
        allow_persistent_state != 0,
        use_hw_secure_codecs != 0,
    ) {
        Ok(()) => STATUS_OK,
        Err(error) => status_of(error),
    }
}

/// Install a user-provided Widevine service certificate on the serial host.
#[unsafe(no_mangle)]
pub extern "C" fn cdm_host_set_server_certificate(
    handle: *mut CdmHandle,
    certificate: Bytes,
) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return STATUS_ERROR;
    };
    if certificate.data.is_null() || certificate.length == 0 || certificate.length > 1024 * 1024 {
        return STATUS_PROTOCOL;
    }
    let certificate = unsafe { view(certificate) };
    let mut state = handle
        .shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match state.host.set_server_certificate(&certificate) {
        Ok(()) => STATUS_OK,
        Err(error) => status_of(error),
    }
}

/// Create a session. out_session_id receives the MediaKeySession id, and
/// out_message receives the first message the CDM produced (a licence request,
/// or an individualization request during provisioning). out_message_type
/// receives its CDM message type; that message is consumed from the event queue.
#[unsafe(no_mangle)]
pub extern "C" fn cdm_host_create_session(
    handle: *mut CdmHandle,
    init_data: Bytes,
    session_type: u32,
    init_data_type: u32,
    out_message_type: *mut u32,
    out_session_id: *mut CdmBuffer,
    out_message: *mut CdmBuffer,
) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return STATUS_ERROR;
    };
    let mut state = handle
        .shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // SAFETY: native callers pass readable init data for the call.
    let init_data = unsafe { view(init_data) };
    let mut events = Vec::new();
    match state
        .host
        .create_session(&init_data, session_type, init_data_type, &mut events)
    {
        Ok((session_id, message)) => {
            // The first message is returned through the requestLicense callback;
            // queueing it as well would send the licence request twice.
            let mut returned_message = false;
            for event in &events {
                if !returned_message {
                    if let HostEvent::Message {
                        session_id: id,
                        message_type,
                        data,
                    } = event
                    {
                        if id == &session_id && message.as_ref() == Some(data) {
                            if !out_message_type.is_null() {
                                unsafe { *out_message_type = *message_type };
                            }
                            returned_message = true;
                            continue;
                        }
                    }
                }
                let encoded = encode_event(event);
                if encoded.0 != 0 {
                    state.events.push_back(encoded);
                }
            }
            if !returned_message {
                return STATUS_PROTOCOL;
            }
            if !out_session_id.is_null() {
                // SAFETY: the out pointer is valid for one write.
                unsafe { *out_session_id = CdmBuffer::from_vec(session_id.into_bytes()) };
            }
            if !out_message.is_null() {
                // SAFETY: the out pointer is valid for one write.
                unsafe { *out_message = CdmBuffer::from_vec(message.unwrap_or_default()) };
            }
            STATUS_OK
        }
        Err(error) => status_of(error),
    }
}

/// Apply a licence or provisioning response; out_keys receives the packed key
/// statuses (count, then per key: size, id, status).
#[unsafe(no_mangle)]
pub extern "C" fn cdm_host_update_session(
    handle: *mut CdmHandle,
    session_id: Bytes,
    response: Bytes,
    out_keys: *mut CdmBuffer,
) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return STATUS_ERROR;
    };
    let mut state = handle
        .shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // SAFETY: native callers pass readable buffers for the call.
    let session_id = String::from_utf8_lossy(&unsafe { view(session_id) }).into_owned();
    // SAFETY: same contract as above.
    let response = unsafe { view(response) };
    let mut events = Vec::new();
    match state
        .host
        .update_session(&session_id, &response, &mut events)
    {
        Ok(keys) => {
            trace_key_statuses(&keys);
            handle.shared.revision.fetch_add(1, Ordering::Release);
            let mut packed = Vec::new();
            pack_u32(&mut packed, keys.len() as u32);
            for key in &keys {
                pack_u32(&mut packed, key.key_id.len() as u32);
                packed.extend_from_slice(&key.key_id);
                pack_u32(&mut packed, key.status);
            }
            for event in &events {
                if matches!(event, HostEvent::Keys { session_id: id, .. } if id == &session_id) {
                    continue;
                }
                let encoded = encode_event(event);
                if encoded.0 != 0 {
                    state.events.push_back(encoded);
                }
            }
            if !out_keys.is_null() {
                // SAFETY: the out pointer is valid for one write.
                unsafe { *out_keys = CdmBuffer::from_vec(packed) };
            }
            STATUS_OK
        }
        Err(error) => status_of(error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cdm_host_close_session(handle: *mut CdmHandle, session_id: Bytes) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return STATUS_ERROR;
    };
    let mut state = handle
        .shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // SAFETY: native callers pass a readable session id for the call.
    let session_id = String::from_utf8_lossy(&unsafe { view(session_id) }).into_owned();
    match state.host.close_session(&session_id) {
        Ok(()) => {
            handle.shared.revision.fetch_add(1, Ordering::Release);
            STATUS_OK
        }
        Err(error) => status_of(error),
    }
}

/// Decrypt one sample. subsamples is packed as count followed by clear and
/// cipher pairs; out_plaintext receives the decrypted sample.
#[unsafe(no_mangle)]
pub extern "C" fn cdm_host_decrypt(
    handle: *mut CdmHandle,
    sample: Bytes,
    key_id: Bytes,
    iv: Bytes,
    subsamples: Bytes,
    scheme: u32,
    timestamp: i64,
    out_plaintext: *mut CdmBuffer,
) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return STATUS_ERROR;
    };
    let mut state = handle
        .shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // SAFETY: native callers pass readable buffers for the call.
    let sample = unsafe { view(sample) };
    // SAFETY: same contract as above.
    let key_id = unsafe { view(key_id) };
    // SAFETY: same contract as above.
    let iv = unsafe { view(iv) };
    // SAFETY: same contract as above.
    let packed_subsamples = unsafe { view(subsamples) };

    let mut regions = Vec::new();
    let mut offset = 0usize;
    if let Some(count) = unpack_u32(&packed_subsamples, &mut offset) {
        for _ in 0..count {
            let clear = unpack_u32(&packed_subsamples, &mut offset).unwrap_or(0);
            let cipher = unpack_u32(&packed_subsamples, &mut offset).unwrap_or(0);
            regions.push(Subsample {
                clear_bytes: clear,
                cipher_bytes: cipher,
            });
        }
    }

    let request = DecryptRequest {
        sample: &sample,
        key_id: &key_id,
        iv: &iv,
        subsamples: &regions,
        scheme,
        timestamp,
    };
    match state.host.decrypt(&request) {
        Ok(plaintext) => {
            if !out_plaintext.is_null() {
                // SAFETY: the out pointer is valid for one write.
                unsafe { *out_plaintext = CdmBuffer::from_vec(plaintext) };
            }
            STATUS_OK
        }
        Err(error) => status_of(error),
    }
}

/// Take one queued asynchronous event. Returns 1 when out_event was filled,
/// 0 when the queue is empty, and a negative status on failure. The packed
/// event is kind, then the payload described in cdm_bridge's module comment.
#[unsafe(no_mangle)]
pub extern "C" fn cdm_host_take_event(handle: *mut CdmHandle, out_event: *mut CdmBuffer) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return STATUS_ERROR;
    };
    let mut state = handle
        .shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if state.events.is_empty() {
        if handle.shared.poll_failed.load(Ordering::Acquire) {
            return STATUS_ERROR;
        }
        let events = match state.host.poll_events() {
            Ok(events) => events,
            Err(error) => {
                handle.shared.poll_failed.store(true, Ordering::Release);
                eprintln!("SCARLET_CDM_EVENT_ERROR error={error:?}");
                return status_of(error);
            }
        };
        for event in events {
            if matches!(
                &event,
                HostEvent::Keys { .. } | HostEvent::SessionClosed { .. }
            ) {
                handle.shared.revision.fetch_add(1, Ordering::Release);
            }
            let encoded = encode_event(&event);
            if encoded.0 != 0 {
                state.events.push_back(encoded);
            }
        }
    }
    let Some((kind, payload)) = state.events.pop_front() else {
        return 0;
    };
    if !out_event.is_null() {
        let mut packed = Vec::with_capacity(4 + payload.len());
        pack_u32(&mut packed, kind);
        packed.extend_from_slice(&payload);
        // SAFETY: the out pointer is valid for one write.
        unsafe { *out_event = CdmBuffer::from_vec(packed) };
    }
    1
}

/// Release a buffer produced by any cdm_host_* function.
#[unsafe(no_mangle)]
pub extern "C" fn cdm_host_buffer_free(buffer: CdmBuffer) {
    if buffer.data.is_null() || buffer.length == 0 {
        return;
    }
    // SAFETY: the buffer was allocated by CdmBuffer::from_vec and is freed once.
    unsafe {
        let slice = std::ptr::slice_from_raw_parts_mut(buffer.data, buffer.length);
        drop(Box::from_raw(slice));
    }
}
