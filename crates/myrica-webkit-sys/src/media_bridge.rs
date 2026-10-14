//! ScarletMedia.h implementation. WebCore stays on its owner thread; only
//! owned encoded bytes and decoded planes cross the bounded worker queues.
//!
//! Acquired event/frame pointers are pinned by a lease until explicitly
//! released. Reset and seek invalidate queued work, never an acquired lease.
use scarlet_codecs::media::{
    AudioDecoder, AudioFrame, Codec, ColorInfo, MediaError, Mp4Parser, ParserEvent, Sample, Track,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_INPUT_CHUNK: usize = 512 * 1024;
const MAX_EVENTS: usize = 64;
const MAX_LEASES: usize = 8;
const MAX_VIDEO_FRAMES: usize = 3;
const MAX_AUDIO_BYTES: usize = 1024 * 1024;
const OK: i32 = 0;
const AGAIN: i32 = 1;
const ERROR: i32 = -1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MediaTrack {
    pub identifier: u64,
    pub kind: u32,
    pub codec: u32,
    pub width: u32,
    pub height: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub duration: f64,
    pub configuration: *const u8,
    pub configuration_size: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MediaSample {
    pub track: u64,
    pub presentation_us: i64,
    pub decode_us: i64,
    pub duration_us: i64,
    pub flags: u32,
    pub reserved: u32,
    pub data: *const u8,
    pub size: usize,
}

#[repr(C)]
pub struct MediaParserEvent {
    pub lease: u64,
    pub append_identifier: u64,
    pub kind: u32,
    pub error: i32,
    pub track: MediaTrack,
    pub sample: MediaSample,
}

#[repr(C)]
pub struct MediaFrame {
    pub lease: u64,
    pub presentation_us: i64,
    pub duration_us: i64,
    pub format: u32,
    pub matrix: u32,
    pub full_range: u32,
    pub width: u32,
    pub height: u32,
    pub reserved: u32,
    pub planes: [*const u8; 3],
    pub strides: [usize; 3],
    pub plane_sizes: [usize; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MediaStatus {
    pub current_time: f64,
    pub duration: f64,
    pub decoded_frames: u64,
    pub dropped_frames: u64,
    pub has_audio: u32,
    pub has_video: u32,
    pub paused: u32,
    pub ended: u32,
    pub ready_state: u32,
    pub error: i32,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Poison cannot unwind across the native ABI. A failed worker publishes a
    // media error, and destroy can still wake/join it and release its leases.
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// Native callers guarantee readable input provenance for the duration of a
/// call. Bounds are checked before creating the borrowed view or copying it.
unsafe fn copy_bytes(data: *const u8, size: usize, limit: usize) -> Result<Vec<u8>, ()> {
    if size > limit || size > isize::MAX as usize || (data.is_null() && size != 0) {
        return Err(());
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    Ok(unsafe { std::slice::from_raw_parts(data, size) }.to_vec())
}

#[derive(Debug)]
struct PlaybackClock {
    seconds: f64,
    anchor: Instant,
    playing: bool,
    audio_master: bool,
    ready: bool,
}

impl PlaybackClock {
    fn new() -> Self {
        Self {
            seconds: 0.0,
            anchor: Instant::now(),
            playing: false,
            audio_master: false,
            ready: false,
        }
    }
    fn now(&self) -> f64 {
        self.at(Instant::now())
    }
    fn at(&self, instant: Instant) -> f64 {
        if self.playing && self.ready && !self.audio_master {
            self.seconds + instant.saturating_duration_since(self.anchor).as_secs_f64()
        } else {
            self.seconds
        }
    }
    fn set_playing(&mut self, playing: bool) {
        self.seconds = self.now();
        self.anchor = Instant::now();
        self.playing = playing;
    }
    fn seek(&mut self, seconds: f64) {
        self.seconds = seconds;
        self.anchor = Instant::now();
        self.ready = false;
    }
    fn mark_ready(&mut self) {
        if !self.ready {
            self.anchor = Instant::now();
            self.ready = true;
        }
    }
    fn audio_position(&mut self, seconds: f64) {
        if self.playing {
            self.seconds = self.seconds.max(seconds);
            self.anchor = Instant::now();
        }
    }
    fn finish_audio(&mut self) {
        // Video can legitimately outlast the audio track. Once all audio has
        // actually been consumed, continue from that position with wall time.
        self.audio_master = false;
        self.anchor = Instant::now();
        self.ready = true;
    }
}

enum ParseInput {
    Bytes {
        generation: u64,
        append: u64,
        bytes: Vec<u8>,
    },
    End {
        generation: u64,
        append: u64,
    },
}

struct ParseOutput {
    append: u64,
    result: Result<Option<ParserEvent>, ()>,
}

impl ParseOutput {
    fn size(&self) -> usize {
        match &self.result {
            Ok(Some(ParserEvent::Track(track))) => track.extradata.len(),
            Ok(Some(ParserEvent::Sample(sample))) => sample.data.len(),
            _ => 0,
        }
    }
}

struct ParserState {
    generation: u64,
    stopping: bool,
    input: VecDeque<ParseInput>,
    input_bytes: usize,
    output: VecDeque<Box<ParseOutput>>,
    output_bytes: usize,
    leases: BTreeMap<u64, Box<ParseOutput>>,
    next_lease: u64,
}

struct ParserShared {
    state: Mutex<ParserState>,
    wake: Condvar,
}

pub struct MediaParser {
    shared: Arc<ParserShared>,
    worker: Option<JoinHandle<()>>,
}

fn publish_parser(shared: &ParserShared, generation: u64, output: ParseOutput) -> bool {
    let size = output.size();
    let mut state = lock(&shared.state);
    while !state.stopping
        && state.generation == generation
        && (state.output.len() >= MAX_EVENTS || state.output_bytes + size > MAX_INPUT_BYTES)
    {
        state = shared
            .wake
            .wait(state)
            .unwrap_or_else(|error| error.into_inner());
    }
    if state.stopping || state.generation != generation {
        return false;
    }
    state.output_bytes += size;
    state.output.push_back(Box::new(output));
    true
}

fn parser_worker(shared: &ParserShared) {
    let mut parser = Mp4Parser::new();
    let mut active_generation = 0;
    loop {
        let input = {
            let mut state = lock(&shared.state);
            while state.input.is_empty() && !state.stopping {
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
            if state.stopping {
                return;
            }
            let input = state.input.pop_front().expect("nonempty parser input");
            if let ParseInput::Bytes { bytes, .. } = &input {
                state.input_bytes -= bytes.len();
            }
            input
        };
        let (generation, append) = match &input {
            ParseInput::Bytes {
                generation, append, ..
            }
            | ParseInput::End { generation, append } => (*generation, *append),
        };
        if generation != active_generation {
            parser.reset();
            active_generation = generation;
        }
        if lock(&shared.state).generation != generation {
            continue;
        }
        let success = match input {
            ParseInput::Bytes { bytes, .. } => bytes.chunks(MAX_INPUT_CHUNK).all(|chunk| {
                parser.push_bytes(chunk).is_ok()
                    && drain_parser(&mut parser, shared, generation, append)
            }),
            ParseInput::End { .. } => {
                parser.finish().is_ok() && drain_parser(&mut parser, shared, generation, append)
            }
        };
        publish_parser(
            shared,
            generation,
            ParseOutput {
                append,
                result: if success { Ok(None) } else { Err(()) },
            },
        );
    }
}

fn drain_parser(
    parser: &mut Mp4Parser,
    shared: &ParserShared,
    generation: u64,
    append: u64,
) -> bool {
    loop {
        match parser.next_event() {
            Ok(Some(event)) => {
                if !publish_parser(
                    shared,
                    generation,
                    ParseOutput {
                        append,
                        result: Ok(Some(event)),
                    },
                ) {
                    return false;
                }
            }
            Ok(None) => return true,
            Err(_) => return false,
        }
    }
}

fn track_view(track: &Track) -> MediaTrack {
    MediaTrack {
        identifier: u64::from(track.id),
        kind: if track.codec == Codec::H264 { 1 } else { 2 },
        codec: if track.codec == Codec::H264 { 1 } else { 2 },
        width: track.width,
        height: track.height,
        sample_rate: track.sample_rate,
        channels: u32::from(track.channels),
        duration: track
            .duration_us
            .map(|duration| duration as f64 / 1_000_000.0)
            .unwrap_or(f64::INFINITY),
        configuration: track.extradata.as_ptr(),
        configuration_size: track.extradata.len(),
    }
}

fn sample_view(sample: &Sample) -> MediaSample {
    MediaSample {
        track: u64::from(sample.track_id),
        presentation_us: sample.pts_us,
        decode_us: sample.dts_us,
        duration_us: sample.duration_us.min(i64::MAX as u64) as i64,
        flags: u32::from(sample.keyframe),
        reserved: 0,
        data: sample.data.as_ptr(),
        size: sample.data.len(),
    }
}

fn empty_track() -> MediaTrack {
    MediaTrack {
        identifier: 0,
        kind: 0,
        codec: 0,
        width: 0,
        height: 0,
        sample_rate: 0,
        channels: 0,
        duration: 0.0,
        configuration: std::ptr::null(),
        configuration_size: 0,
    }
}
fn empty_sample() -> MediaSample {
    MediaSample {
        track: 0,
        presentation_us: 0,
        decode_us: 0,
        duration_us: 0,
        flags: 0,
        reserved: 0,
        data: std::ptr::null(),
        size: 0,
    }
}

unsafe fn text<'a>(data: *const c_char, size: usize) -> Option<&'a str> {
    if size > 4096 || (data.is_null() && size != 0) {
        return None;
    }
    if size == 0 {
        return Some("");
    }
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(data.cast(), size) }).ok()
}

fn supported(mime: &str, codecs: &str) -> bool {
    let mime = mime.trim().to_ascii_lowercase();
    if mime != "video/mp4" && mime != "audio/mp4" && mime != "application/mp4" {
        return false;
    }
    codecs.trim().trim_matches('"').split(',').all(|codec| {
        let codec = codec.trim().to_ascii_lowercase();
        codec.is_empty() || codec == "mp4a.40.2" || (mime != "audio/mp4" && avc_codec(&codec))
    })
}

fn avc_codec(codec: &str) -> bool {
    let Some((kind, profile)) = codec.split_once('.') else {
        return false;
    };
    if kind != "avc1" || profile.len() != 6 || !profile.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }
    let profile_idc = u8::from_str_radix(&profile[..2], 16).unwrap();
    let constraints = u8::from_str_radix(&profile[2..4], 16).unwrap();
    let level = u8::from_str_radix(&profile[4..], 16).unwrap();
    matches!(profile_idc, 0x42 | 0x4d | 0x64)
        && constraints & 3 == 0
        && matches!(
            level,
            10 | 11 | 12 | 13 | 20 | 21 | 22 | 30 | 31 | 32 | 40 | 41 | 42 | 50 | 51 | 52
        )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_supports_type(
    mime: *const c_char,
    mime_size: usize,
    codecs: *const c_char,
    codecs_size: usize,
    is_media_source: u32,
) -> i32 {
    let (Some(mime), Some(codecs)) = (unsafe { text(mime, mime_size) }, unsafe {
        text(codecs, codecs_size)
    }) else {
        return ERROR;
    };
    if is_media_source > 1 {
        return ERROR;
    }
    if !supported(mime, codecs) {
        return 0;
    }
    let codecs = codecs.trim().trim_matches('"');
    let requires_h264 = codecs
        .split(',')
        .any(|codec| codec.trim().to_ascii_lowercase().starts_with("avc1."))
        || (codecs.is_empty() && !mime.trim().eq_ignore_ascii_case("audio/mp4"));
    i32::from(!requires_h264 || native_h264_available())
}

fn native_h264_available() -> bool {
    #[cfg(target_os = "scarlet")]
    {
        scarlet_video_client::ScarletVideoDecoder::available_capabilities()
            .ok()
            .is_some_and(|caps| {
                caps.supports(scarlet_video_client::VideoFormat::H264)
                    && caps.output_pixel_format()
                        == scarlet_video_client::NV12_VIDEO_RANGE_PIXEL_FORMAT
            })
    }
    #[cfg(not(target_os = "scarlet"))]
    {
        false
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_parser_create(
    mime: *const c_char,
    mime_size: usize,
    is_media_source: u32,
) -> *mut MediaParser {
    let Some(mime) = (unsafe { text(mime, mime_size) }) else {
        return std::ptr::null_mut();
    };
    if is_media_source > 1 || !supported(mime, "") {
        return std::ptr::null_mut();
    }
    let shared = Arc::new(ParserShared {
        state: Mutex::new(ParserState {
            generation: 1,
            stopping: false,
            input: VecDeque::new(),
            input_bytes: 0,
            output: VecDeque::new(),
            output_bytes: 0,
            leases: BTreeMap::new(),
            next_lease: 1,
        }),
        wake: Condvar::new(),
    });
    let worker_shared = shared.clone();
    let Ok(worker) = thread::Builder::new()
        .name("webkit-media-demux".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || parser_worker(&worker_shared))
    else {
        return std::ptr::null_mut();
    };
    Box::into_raw(Box::new(MediaParser {
        shared,
        worker: Some(worker),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_parser_destroy(parser: *mut MediaParser) {
    if parser.is_null() {
        return;
    }
    let mut parser = unsafe { Box::from_raw(parser) };
    lock(&parser.shared.state).stopping = true;
    parser.shared.wake.notify_all();
    if let Some(worker) = parser.worker.take() {
        let _ = worker.join();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_parser_append(
    parser: *mut MediaParser,
    bytes: *const u8,
    size: usize,
    append_identifier: u64,
) -> i32 {
    let Some(parser) = (unsafe { parser.as_ref() }) else {
        return ERROR;
    };
    if size > MAX_INPUT_CHUNK {
        return ERROR;
    }
    let mut state = lock(&parser.shared.state);
    if state.stopping {
        return ERROR;
    }
    if state.input.len() >= MAX_EVENTS || state.input_bytes + size > MAX_INPUT_BYTES {
        return AGAIN;
    }
    let Ok(bytes) = (unsafe { copy_bytes(bytes, size, MAX_INPUT_CHUNK) }) else {
        return ERROR;
    };
    let generation = state.generation;
    state.input_bytes += bytes.len();
    state.input.push_back(ParseInput::Bytes {
        generation,
        append: append_identifier,
        bytes,
    });
    parser.shared.wake.notify_all();
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_parser_end(
    parser: *mut MediaParser,
    append_identifier: u64,
) -> i32 {
    let Some(parser) = (unsafe { parser.as_ref() }) else {
        return ERROR;
    };
    let mut state = lock(&parser.shared.state);
    if state.stopping {
        return ERROR;
    }
    if state.input.len() >= MAX_EVENTS {
        return AGAIN;
    }
    let generation = state.generation;
    state.input.push_back(ParseInput::End {
        generation,
        append: append_identifier,
    });
    parser.shared.wake.notify_all();
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_parser_reset(parser: *mut MediaParser) {
    let Some(parser) = (unsafe { parser.as_ref() }) else {
        return;
    };
    let mut state = lock(&parser.shared.state);
    state.generation = state.generation.wrapping_add(1);
    state.input.clear();
    state.input_bytes = 0;
    state.output.clear();
    state.output_bytes = state.leases.values().map(|event| event.size()).sum();
    parser.shared.wake.notify_all();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_parser_poll(
    parser: *mut MediaParser,
    output: *mut MediaParserEvent,
) -> i32 {
    let (Some(parser), Some(output)) = (unsafe { parser.as_ref() }, unsafe { output.as_mut() })
    else {
        return ERROR;
    };
    let mut state = lock(&parser.shared.state);
    if state.leases.len() >= MAX_LEASES {
        return OK;
    }
    let Some(event) = state.output.pop_front() else {
        return OK;
    };
    let lease = state.next_lease;
    state.next_lease = lease.wrapping_add(1).max(1);
    *output = MediaParserEvent {
        lease,
        append_identifier: event.append,
        kind: 0,
        error: 0,
        track: empty_track(),
        sample: empty_sample(),
    };
    match &event.result {
        Ok(Some(ParserEvent::Track(track))) => {
            output.kind = 1;
            output.track = track_view(track);
        }
        Ok(Some(ParserEvent::Sample(sample))) => {
            output.kind = 2;
            output.sample = sample_view(sample);
        }
        Ok(None) => output.kind = 3,
        Err(()) => {
            output.kind = 4;
            output.error = ERROR;
        }
    }
    state.leases.insert(lease, event);
    parser.shared.wake.notify_all();
    1
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_parser_release(parser: *mut MediaParser, lease: u64) {
    let Some(parser) = (unsafe { parser.as_ref() }) else {
        return;
    };
    let mut state = lock(&parser.shared.state);
    if let Some(event) = state.leases.remove(&lease) {
        state.output_bytes -= event.size();
    }
    parser.shared.wake.notify_all();
}

struct SessionTrack {
    track: Track,
    generation: u64,
    samples: VecDeque<QueuedSample>,
    ended: bool,
    drained: bool,
    media_end_seconds: f64,
    cancellation: Arc<AtomicBool>,
}

struct QueuedSample {
    sample: Sample,
    non_displaying: bool,
}

struct SessionState {
    generation: u64,
    next_track_generation: u64,
    audio_generation: u64,
    stopping: bool,
    tracks: BTreeMap<u32, SessionTrack>,
    input_bytes: usize,
    video: VecDeque<Box<VideoFrame>>,
    leases: BTreeMap<u64, Box<VideoFrame>>,
    next_lease: u64,
    audio_bytes: usize,
    audio_seconds: f64,
    audio_drained: bool,
    media_end_seconds: f64,
    seek_target: Option<f64>,
    clock: PlaybackClock,
    volume: f64,
    muted: bool,
    status: MediaStatus,
}

struct SessionShared {
    state: Mutex<SessionState>,
    wake: Condvar,
}

pub struct MediaSession {
    shared: Arc<SessionShared>,
    worker: Option<JoinHandle<()>>,
}

enum Decoder {
    Video(HardwareVideoDecoder),
    Audio(AudioDecoder),
}

struct TrackDecoder {
    generation: u64,
    decoder: Decoder,
    non_displaying: BTreeSet<i64>,
}

enum DecodeOutput {
    Video(Vec<VideoFrame>),
    Audio(AudioFrame),
}

struct VideoFrame {
    width: u32,
    height: u32,
    pts_us: i64,
    duration_us: u64,
    color: ColorInfo,
    #[cfg(target_os = "scarlet")]
    pixels: scarlet_video_client::OwnedDecodedFrame,
}

impl VideoFrame {
    fn payload(&self) -> &[u8] {
        #[cfg(target_os = "scarlet")]
        {
            self.pixels.payload()
        }
        #[cfg(not(target_os = "scarlet"))]
        {
            &[]
        }
    }
}

/// The device returns pictures in decode order. Retain at most the SPS
/// reorder depth and release by the sample's presentation timestamp, including
/// the final held pictures at EOS. Pixels are never synthesized here.
struct PresentationReorder<T> {
    depth: usize,
    pending: Vec<(i64, T)>,
}

impl<T> PresentationReorder<T> {
    fn new(depth: usize) -> Result<Self, MediaError> {
        if depth > 2 {
            return Err(MediaError::Unsupported(
                "native AVC reorder depth exceeds two",
            ));
        }
        Ok(Self {
            depth,
            pending: Vec::with_capacity(depth + 1),
        })
    }
    fn push(&mut self, pts: i64, frame: T) -> Option<T> {
        self.pending.push((pts, frame));
        if self.pending.len() > self.depth {
            let index = self
                .pending
                .iter()
                .enumerate()
                .min_by_key(|(_, (pts, _))| *pts)
                .unwrap()
                .0;
            Some(self.pending.remove(index).1)
        } else {
            None
        }
    }
    fn finish(&mut self) -> Vec<T> {
        self.pending.sort_by_key(|(pts, _)| *pts);
        self.pending.drain(..).map(|(_, frame)| frame).collect()
    }
}

struct HardwareVideoDecoder {
    #[cfg(target_os = "scarlet")]
    decoder: scarlet_video_client::ScarletVideoDecoder,
    #[cfg(target_os = "scarlet")]
    configuration: scarlet_codecs::media::AvcConfiguration,
    #[cfg(target_os = "scarlet")]
    reorder: PresentationReorder<VideoFrame>,
    #[cfg(target_os = "scarlet")]
    next_token: u64,
    #[cfg(target_os = "scarlet")]
    metadata: BTreeMap<u64, (i64, u64, ColorInfo)>,
    #[cfg(target_os = "scarlet")]
    dimensions: (u32, u32),
    cancellation: Arc<AtomicBool>,
}

impl HardwareVideoDecoder {
    #[cfg(target_os = "scarlet")]
    fn new(track: &Track, cancellation: Arc<AtomicBool>) -> Result<Self, MediaError> {
        use scarlet_video_client::{
            DecoderOptions, ScarletVideoDecoder, VideoBufferRequest, VideoFormat,
        };
        let configuration = scarlet_codecs::media::AvcConfiguration::new(track)?;
        let output_len = scarlet_video_client::recommended_nv12_output_buffer_len(
            configuration.width(),
            configuration.height(),
        )
        .ok_or(MediaError::InvalidData(
            "native AVC output capacity overflow",
        ))?;
        let input_len =
            scarlet_video_client::recommended_input_buffer_len(4 * 1024 * 1024 + 65536 + 1024)
                .ok_or(MediaError::InvalidData(
                    "native AVC input capacity overflow",
                ))?;
        let options = DecoderOptions::new()
            .with_shared_images(false)
            .with_buffer_request(VideoBufferRequest::new(input_len, output_len))
            .with_cancellation(cancellation.clone());
        let mut decoder =
            ScarletVideoDecoder::open_with_options(options).map_err(MediaError::Decode)?;
        if !decoder.capabilities().is_some_and(|caps| {
            caps.supports(VideoFormat::H264)
                && caps.output_pixel_format() == scarlet_video_client::NV12_VIDEO_RANGE_PIXEL_FORMAT
        }) {
            return Err(MediaError::Unsupported(
                "/dev/video0 does not advertise AVC decode",
            ));
        }
        decoder
            .configure(VideoFormat::H264)
            .map_err(MediaError::Decode)?;
        let reorder = PresentationReorder::new(configuration.max_reorder_frames())?;
        let dimensions = (configuration.width(), configuration.height());
        eprintln!(
            "[myrica:media] video-decoder=scarlet-native device=/dev/video0 output=nv12 cpu-h264-fallback=none"
        );
        Ok(Self {
            decoder,
            configuration,
            reorder,
            next_token: 1,
            metadata: BTreeMap::new(),
            dimensions,
            cancellation,
        })
    }

    #[cfg(not(target_os = "scarlet"))]
    fn new(_: &Track, _: Arc<AtomicBool>) -> Result<Self, MediaError> {
        Err(MediaError::Unsupported(
            "native Scarlet video device is unavailable on this host",
        ))
    }

    #[cfg(target_os = "scarlet")]
    fn push(&mut self, sample: &Sample) -> Result<Vec<VideoFrame>, MediaError> {
        use scarlet_video_client::{DecodedOutput, VideoFormat};
        if self.cancellation.load(Ordering::Acquire) {
            return Ok(Vec::new());
        }
        let unit = self.configuration.annex_b(sample)?;
        if (self.configuration.width(), self.configuration.height()) != self.dimensions {
            return Err(MediaError::Unsupported(
                "AVC dimensions changed without a track reconfiguration",
            ));
        }
        if self.configuration.max_reorder_frames() != self.reorder.depth {
            if !self.reorder.pending.is_empty() {
                return Err(MediaError::Unsupported(
                    "AVC reorder depth changed without a track reconfiguration",
                ));
            }
            self.reorder = PresentationReorder::new(self.configuration.max_reorder_frames())?;
        }
        if !annex_b_has_picture(&unit) {
            return Err(MediaError::InvalidData(
                "AVC access unit has no coded picture",
            ));
        }
        let token = self.next_token;
        self.next_token = token.checked_add(1).ok_or(MediaError::InvalidData(
            "native AVC timestamp token overflow",
        ))?;
        let mut color = self.configuration.color();
        // The mapped device ABI explicitly returns NV12 video-range pixels;
        // the native producer converts full-range sources to that output.
        color.full_range = false;
        self.metadata
            .insert(token, (sample.pts_us, sample.duration_us, color));
        let output = self
            .decoder
            .decode_output(VideoFormat::H264, &unit, token)
            .map_err(MediaError::Decode)?;
        let Some(output) = output else {
            self.metadata.remove(&token);
            return if self.cancellation.load(Ordering::Acquire) {
                Ok(Vec::new())
            } else {
                Err(MediaError::InvalidData(
                    "native AVC access unit produced no frame",
                ))
            };
        };
        let DecodedOutput::Nv12(frame) = output else {
            return Err(MediaError::Unsupported(
                "native image decode requires an explicit GPU image ABI",
            ));
        };
        let (pts_us, duration_us, color) =
            self.metadata
                .remove(&frame.timestamp())
                .ok_or(MediaError::InvalidData(
                    "native AVC frame has an unknown timestamp token",
                ))?;
        let pixels = frame.try_into_owned().map_err(MediaError::Decode)?;
        let frame = VideoFrame {
            width: pixels.width(),
            height: pixels.height(),
            pts_us,
            duration_us,
            color,
            pixels,
        };
        if !decoded_video_is_valid(&frame) {
            return Err(MediaError::InvalidData(
                "native AVC NV12 dimensions or payload are invalid",
            ));
        }
        if self.metadata.len() > 3 {
            return Err(MediaError::InvalidData(
                "native AVC timestamp queue exceeded capacity",
            ));
        }
        Ok(self.reorder.push(pts_us, frame).into_iter().collect())
    }

    #[cfg(not(target_os = "scarlet"))]
    fn push(&mut self, _: &Sample) -> Result<Vec<VideoFrame>, MediaError> {
        Err(MediaError::Unsupported(
            "native Scarlet video device is unavailable on this host",
        ))
    }

    fn flush(&mut self) -> Result<Vec<VideoFrame>, MediaError> {
        if self.cancellation.load(Ordering::Acquire) {
            return Ok(Vec::new());
        }
        #[cfg(target_os = "scarlet")]
        {
            Ok(self.reorder.finish())
        }
        #[cfg(not(target_os = "scarlet"))]
        {
            Err(MediaError::Unsupported(
                "native Scarlet video device is unavailable on this host",
            ))
        }
    }
}

fn annex_b_has_picture(unit: &[u8]) -> bool {
    // The shared AVC facade emits a four-byte prefix for every EBSP NAL. Its
    // parameter sets/SEI alone do not satisfy the complete access-unit contract.
    unit.windows(5)
        .any(|bytes| bytes[..4] == [0, 0, 0, 1] && matches!(bytes[4] & 31, 1 | 5))
}

fn decoded_video_is_valid(frame: &VideoFrame) -> bool {
    let width = frame.width as usize;
    let height = frame.height as usize;
    if width == 0 || height == 0 || width > 3840 || height > 2160 {
        return false;
    }
    width.is_multiple_of(2)
        && height.is_multiple_of(2)
        && width
            .checked_mul(height + height / 2)
            .is_some_and(|size| frame.payload().len() == size)
}

fn update_duration(state: &mut SessionState) {
    let declared = state
        .tracks
        .values()
        .filter_map(|track| track.track.duration_us)
        .filter(|duration| *duration != 0)
        .max();
    state.status.duration = declared.map(|value| value as f64 / 1_000_000.0).unwrap_or(
        if state.media_end_seconds > 0.0 {
            state.media_end_seconds
        } else {
            f64::INFINITY
        },
    );
}

fn session_worker(shared: &SessionShared) {
    let mut decoders: BTreeMap<u32, TrackDecoder> = BTreeMap::new();
    let mut audio = AudioOutput::new();
    let mut last_generation = 0;
    let mut last_audio_generation = 0;
    loop {
        let mut state = lock(&shared.state);
        if state.stopping {
            return;
        }
        if state.status.error != 0 {
            state.clock.set_playing(false);
            state.status.paused = 1;
            drop(
                shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner()),
            );
            continue;
        }
        let reset_all = last_generation != state.generation;
        let retired_ids: Vec<_> = decoders
            .iter()
            .filter_map(|(&id, decoder)| {
                (reset_all
                    || state
                        .tracks
                        .get(&id)
                        .is_none_or(|track| track.generation != decoder.generation))
                .then_some(id)
            })
            .collect();
        let retired: Vec<_> = retired_ids
            .into_iter()
            .filter_map(|id| decoders.remove(&id))
            .collect();
        let retired_audio = if reset_all || last_audio_generation != state.audio_generation {
            let previous = std::mem::replace(&mut audio, AudioOutput::new());
            audio.minimum_seconds = state.clock.seconds;
            Some(previous)
        } else {
            None
        };
        last_generation = state.generation;
        last_audio_generation = state.audio_generation;
        // SAS consumed frames, not the number of submitted samples, own the
        // audio clock. Ring writes are nonblocking and never stall WebCore.
        let playing = state.clock.playing;
        let volume = if state.muted { 0.0 } else { state.volume };
        let audio_generation = state.audio_generation;
        drop(state);
        // Hardware session destruction may wait for the producer. Keep it
        // outside the state lock so WebCore controls/status remain responsive.
        drop(retired);
        drop(retired_audio);
        let audio_result = audio.pump(playing, volume, shared, audio_generation);
        let mut state = lock(&shared.state);
        if state.stopping {
            return;
        }
        if state.audio_generation != audio_generation {
            continue;
        }
        if audio_result.is_err() {
            state.status.error = ERROR;
            state.clock.set_playing(false);
        }
        if let Some(seconds) = audio.position() {
            state.clock.audio_position(seconds);
        }
        if state.clock.audio_master
            && audio.is_empty()
            && state
                .tracks
                .values()
                .filter(|track| track.track.codec == Codec::Aac)
                .all(|track| track.ended && track.drained && track.samples.is_empty())
        {
            state.clock.finish_audio();
        }
        state.audio_bytes = audio.bytes();
        state.audio_seconds = audio.seconds();
        state.audio_drained = audio.is_empty();
        state.status.current_time = state.clock.now();
        let video_empty = state.video.is_empty();
        let audio_capacity = state.audio_bytes < MAX_AUDIO_BYTES && state.audio_seconds < 0.25;
        let state_audio_seconds = state.audio_seconds;
        let work = state.tracks.iter_mut().find_map(|(&id, track)| {
            let capacity = match track.track.codec {
                Codec::H264 => video_empty,
                Codec::Aac => {
                    audio_capacity
                        && (state_audio_seconds + 1024.0 / f64::from(track.track.sample_rate)
                            <= 0.25)
                }
            };
            if !capacity {
                return None;
            }
            if let Some(sample) = track.samples.pop_front() {
                Some((
                    id,
                    track.generation,
                    track.track.clone(),
                    track.cancellation.clone(),
                    Some(sample),
                ))
            } else if track.ended && !track.drained {
                track.drained = true;
                Some((
                    id,
                    track.generation,
                    track.track.clone(),
                    track.cancellation.clone(),
                    None,
                ))
            } else {
                None
            }
        });
        if let Some((id, generation, track, cancellation, sample)) = work {
            if let Some(sample) = &sample {
                state.input_bytes -= sample.sample.data.len();
            }
            drop(state);
            if decoders
                .get(&id)
                .is_none_or(|current| current.generation != generation)
            {
                let decoder = match track.codec {
                    Codec::H264 => {
                        HardwareVideoDecoder::new(&track, cancellation).map(Decoder::Video)
                    }
                    Codec::Aac => AudioDecoder::new(&track).map(Decoder::Audio),
                };
                match decoder {
                    Ok(decoder) => {
                        decoders.insert(
                            id,
                            TrackDecoder {
                                generation,
                                decoder,
                                non_displaying: BTreeSet::new(),
                            },
                        );
                    }
                    Err(error) => {
                        let mut state = lock(&shared.state);
                        if state
                            .tracks
                            .get(&id)
                            .is_some_and(|track| track.generation == generation)
                        {
                            state.status.error = ERROR;
                            eprintln!("SCARLET_MEDIA_ERROR stage=decoder-open message={error}");
                        }
                        continue;
                    }
                }
            }
            let decoder = decoders.get_mut(&id).expect("configured media decoder");
            if let Some(sample) = &sample {
                if sample.non_displaying {
                    if decoder.non_displaying.len() >= MAX_EVENTS {
                        let mut state = lock(&shared.state);
                        if state
                            .tracks
                            .get(&id)
                            .is_some_and(|track| track.generation == generation)
                        {
                            state.status.error = ERROR;
                        }
                        continue;
                    }
                    decoder.non_displaying.insert(sample.sample.pts_us);
                }
            }
            let result = match (&mut decoder.decoder, sample.as_ref()) {
                (Decoder::Video(decoder), Some(sample)) => {
                    decoder.push(&sample.sample).map(DecodeOutput::Video)
                }
                (Decoder::Video(decoder), None) => decoder.flush().map(DecodeOutput::Video),
                (Decoder::Audio(decoder), Some(sample)) => {
                    decoder.decode(&sample.sample).map(DecodeOutput::Audio)
                }
                (Decoder::Audio(_), None) => {
                    let state = lock(&shared.state);
                    if state
                        .tracks
                        .get(&id)
                        .is_some_and(|track| track.generation == generation)
                    {
                        audio.begin_drain();
                    }
                    continue;
                }
            };
            let mut state = lock(&shared.state);
            if state.stopping {
                return;
            }
            if state
                .tracks
                .get(&id)
                .is_none_or(|track| track.generation != generation)
            {
                continue;
            }
            match result {
                Ok(DecodeOutput::Video(mut frames)) => {
                    if frames.len() > MAX_VIDEO_FRAMES || !frames.iter().all(decoded_video_is_valid)
                    {
                        state.status.error = ERROR;
                        continue;
                    }
                    state.status.decoded_frames += frames.len() as u64;
                    frames.retain(|frame| !decoder.non_displaying.remove(&frame.pts_us));
                    for frame in &frames {
                        let end = (frame.pts_us as f64 + frame.duration_us as f64) / 1_000_000.0;
                        state.media_end_seconds = state.media_end_seconds.max(end);
                        if let Some(track) = state.tracks.get_mut(&id) {
                            track.media_end_seconds = track.media_end_seconds.max(end);
                        }
                    }
                    update_duration(&mut state);
                    if frames.iter().any(|frame| {
                        state.seek_target.is_none_or(|target| {
                            (frame.pts_us as f64 + frame.duration_us as f64) / 1_000_000.0 > target
                        })
                    }) {
                        state.clock.mark_ready();
                    }
                    // Each access unit is decoded only when the presentation
                    // queue is empty. Reordered outputs fit as a complete batch.
                    state.video.extend(frames.into_iter().map(Box::new));
                }
                Ok(DecodeOutput::Audio(frame)) => {
                    if decoder.non_displaying.remove(&frame.pts_us) {
                        continue;
                    }
                    if frame.channels == 0 || frame.rate == 0 {
                        state.status.error = ERROR;
                    } else {
                        if !state.clock.audio_master {
                            state.clock.seconds = state.clock.now();
                            state.clock.audio_master = true;
                        }
                        let end = frame.pts_us as f64 / 1_000_000.0
                            + frame.samples.len() as f64
                                / f64::from(frame.channels)
                                / f64::from(frame.rate);
                        state.media_end_seconds = state.media_end_seconds.max(end);
                        if let Some(track) = state.tracks.get_mut(&id) {
                            track.media_end_seconds = track.media_end_seconds.max(end);
                        }
                        update_duration(&mut state);
                        if audio.push(frame).is_err() {
                            state.status.error = ERROR;
                        }
                    }
                }
                Err(error) => {
                    state.status.error = ERROR;
                    eprintln!("SCARLET_MEDIA_ERROR stage=decode message={error}");
                }
            }
            shared.wake.notify_all();
            continue;
        }
        let all_ended = !state.tracks.is_empty()
            && state
                .tracks
                .values()
                .all(|track| track.ended && track.drained);
        let media_end = state.media_end_seconds;
        if all_ended && video_empty && audio.is_empty() && state.clock.now() >= media_end {
            state.status.ended = 1;
            state.clock.set_playing(false);
        }
        state.status.paused = u32::from(!state.clock.playing);
        let have_current_video = state.video.iter().any(|frame| {
            state.seek_target.is_none_or(|target| {
                (frame.pts_us as f64 + frame.duration_us as f64) / 1_000_000.0 > target
            })
        });
        state.status.ready_state = if !state.tracks.is_empty() {
            if have_current_video || state.audio_bytes > 0 {
                3
            } else if state.clock.ready || audio.position().is_some() {
                2
            } else {
                1
            }
        } else {
            0
        };
        // A paused idle player sleeps until a command. Playing clocks and SAS
        // consumption are sampled at a bounded 5ms interval without busy spin.
        if state.clock.playing || !audio.is_empty() {
            let _ = shared.wake.wait_timeout(state, Duration::from_millis(5));
        } else {
            drop(
                shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner()),
            );
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn scarlet_media_session_create() -> *mut MediaSession {
    let shared = Arc::new(SessionShared {
        state: Mutex::new(SessionState {
            generation: 1,
            next_track_generation: 1,
            audio_generation: 1,
            stopping: false,
            tracks: BTreeMap::new(),
            input_bytes: 0,
            video: VecDeque::new(),
            leases: BTreeMap::new(),
            next_lease: 1,
            audio_bytes: 0,
            audio_seconds: 0.0,
            audio_drained: true,
            media_end_seconds: 0.0,
            seek_target: None,
            clock: PlaybackClock::new(),
            volume: 1.0,
            muted: false,
            status: MediaStatus {
                paused: 1,
                ..MediaStatus::default()
            },
        }),
        wake: Condvar::new(),
    });
    let worker_shared = shared.clone();
    let Ok(worker) = thread::Builder::new()
        .name("webkit-media-decode".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || session_worker(&worker_shared))
    else {
        return std::ptr::null_mut();
    };
    Box::into_raw(Box::new(MediaSession {
        shared,
        worker: Some(worker),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_destroy(session: *mut MediaSession) {
    if session.is_null() {
        return;
    }
    let mut session = unsafe { Box::from_raw(session) };
    {
        let mut state = lock(&session.shared.state);
        state.stopping = true;
        for track in state.tracks.values() {
            track.cancellation.store(true, Ordering::Release);
        }
    }
    session.shared.wake.notify_all();
    if let Some(worker) = session.worker.take() {
        let _ = worker.join();
    }
}

unsafe fn owned_track(track: &MediaTrack) -> Result<Track, ()> {
    let codec = match (track.kind, track.codec) {
        (1, 1) => Codec::H264,
        (2, 2) => Codec::Aac,
        _ => return Err(()),
    };
    if track.identifier == 0
        || track.identifier > u32::MAX as u64
        || track.duration.is_nan()
        || track.duration < 0.0
        || (codec == Codec::H264 && (track.width > 3840 || track.height > 2160))
        || (codec == Codec::Aac
            && (track.sample_rate < 8000
                || track.sample_rate > 96000
                || !(1..=2).contains(&track.channels)))
    {
        return Err(());
    }
    let extradata =
        unsafe { copy_bytes(track.configuration, track.configuration_size, 1024 * 1024) }?;
    Ok(Track {
        id: track.identifier as u32,
        codec,
        extradata,
        width: track.width,
        height: track.height,
        sample_rate: track.sample_rate,
        channels: track.channels as u16,
        timescale: 1_000_000,
        duration_us: if track.duration.is_finite() {
            Some((track.duration * 1_000_000.0) as u64)
        } else {
            None
        },
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_configure(
    session: *mut MediaSession,
    track: *const MediaTrack,
) -> i32 {
    let (Some(session), Some(track)) = (unsafe { session.as_ref() }, unsafe { track.as_ref() })
    else {
        return ERROR;
    };
    let Ok(track) = (unsafe { owned_track(track) }) else {
        return ERROR;
    };
    let mut state = lock(&session.shared.state);
    if state.stopping {
        return ERROR;
    }
    // The player has one selected video and one selected audio track. Track
    // selection is WebCore's responsibility; silently mixing tracks is wrong.
    if state
        .tracks
        .values()
        .any(|other| other.track.codec == track.codec && other.track.id != track.id)
    {
        return ERROR;
    }
    let generation = state.next_track_generation;
    state.next_track_generation = generation.wrapping_add(1);
    if let Some(old) = state.tracks.remove(&track.id) {
        old.cancellation.store(true, Ordering::Release);
        state.input_bytes -= old
            .samples
            .iter()
            .map(|sample| sample.sample.data.len())
            .sum::<usize>();
    }
    if track.codec == Codec::H264 {
        state.status.has_video = 1;
        state.video.clear();
    } else {
        state.status.has_audio = 1;
        state.clock.audio_master = true;
        state.audio_generation = state.audio_generation.wrapping_add(1);
    }
    state.status.ended = 0;
    state.tracks.insert(
        track.id,
        SessionTrack {
            track,
            generation,
            samples: VecDeque::new(),
            ended: false,
            drained: false,
            media_end_seconds: 0.0,
            cancellation: Arc::new(AtomicBool::new(false)),
        },
    );
    state.media_end_seconds = state
        .tracks
        .values()
        .map(|track| track.media_end_seconds)
        .fold(0.0, f64::max);
    update_duration(&mut state);
    session.shared.wake.notify_all();
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_enqueue(
    session: *mut MediaSession,
    sample: *const MediaSample,
) -> i32 {
    let (Some(session), Some(sample)) = (unsafe { session.as_ref() }, unsafe { sample.as_ref() })
    else {
        return ERROR;
    };
    if sample.track > u32::MAX as u64
        || sample.duration_us < 0
        || sample.reserved != 0
        || sample.flags & !3 != 0
    {
        return ERROR;
    }
    let mut state = lock(&session.shared.state);
    if state.stopping {
        return ERROR;
    }
    let Some(track) = state.tracks.get(&(sample.track as u32)) else {
        return ERROR;
    };
    if sample.size > 4 * 1024 * 1024 {
        return ERROR;
    }
    if state.input_bytes + sample.size > MAX_INPUT_BYTES || track.samples.len() >= MAX_EVENTS {
        return AGAIN;
    }
    let Ok(data) = (unsafe { copy_bytes(sample.data, sample.size, 4 * 1024 * 1024) }) else {
        return ERROR;
    };
    state.input_bytes += data.len();
    let track = state
        .tracks
        .get_mut(&(sample.track as u32))
        .expect("validated track");
    track.ended = false;
    track.drained = false;
    track.samples.push_back(QueuedSample {
        sample: Sample {
            track_id: sample.track as u32,
            data,
            pts_us: sample.presentation_us,
            dts_us: sample.decode_us,
            duration_us: sample.duration_us as u64,
            keyframe: sample.flags & 1 != 0,
        },
        non_displaying: sample.flags & 2 != 0,
    });
    state.status.ended = 0;
    session.shared.wake.notify_all();
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_has_capacity(
    session: *mut MediaSession,
    track: u64,
) -> u32 {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return 0;
    };
    let state = lock(&session.shared.state);
    u32::from(
        !state.stopping
            && track <= u32::MAX as u64
            && state.input_bytes < MAX_INPUT_BYTES
            && state
                .tracks
                .get(&(track as u32))
                .is_some_and(|track| track.samples.len() < MAX_EVENTS),
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_flush(session: *mut MediaSession, track: u64) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    let mut state = lock(&session.shared.state);
    if track > u32::MAX as u64 {
        return;
    }
    let generation = state.next_track_generation;
    state.next_track_generation = generation.wrapping_add(1);
    if let Some(track) = state.tracks.get_mut(&(track as u32)) {
        let bytes = track
            .samples
            .iter()
            .map(|sample| sample.sample.data.len())
            .sum::<usize>();
        track.samples.clear();
        track.ended = false;
        track.drained = false;
        track.cancellation.store(true, Ordering::Release);
        track.cancellation = Arc::new(AtomicBool::new(false));
        track.generation = generation;
        let video = track.track.codec == Codec::H264;
        state.input_bytes -= bytes;
        if video {
            state.video.clear();
        } else {
            state.audio_generation = state.audio_generation.wrapping_add(1);
        }
        state.status.ended = 0;
        session.shared.wake.notify_all();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_remove_track(
    session: *mut MediaSession,
    track: u64,
) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    if track > u32::MAX as u64 {
        return;
    }
    let mut state = lock(&session.shared.state);
    let Some(removed) = state.tracks.remove(&(track as u32)) else {
        return;
    };
    removed.cancellation.store(true, Ordering::Release);
    state.input_bytes -= removed
        .samples
        .iter()
        .map(|sample| sample.sample.data.len())
        .sum::<usize>();
    if removed.track.codec == Codec::H264 {
        state.video.clear();
    } else {
        state.audio_generation = state.audio_generation.wrapping_add(1);
        state.audio_bytes = 0;
        state.audio_seconds = 0.0;
        state.audio_drained = true;
        if state.clock.audio_master {
            state.clock.finish_audio();
        }
    }
    state.status.has_video = u32::from(
        state
            .tracks
            .values()
            .any(|track| track.track.codec == Codec::H264),
    );
    state.status.has_audio = u32::from(
        state
            .tracks
            .values()
            .any(|track| track.track.codec == Codec::Aac),
    );
    state.media_end_seconds = state
        .tracks
        .values()
        .map(|track| track.media_end_seconds)
        .fold(0.0, f64::max);
    state.status.ready_state = if state.tracks.is_empty() { 0 } else { 1 };
    state.status.ended = 0;
    if state.tracks.is_empty() {
        state.clock.set_playing(false);
        state.status.duration = 0.0;
    } else {
        update_duration(&mut state);
    }
    session.shared.wake.notify_all();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_end(session: *mut MediaSession, track: u64) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    if track > u32::MAX as u64 {
        return;
    }
    if let Some(track) = lock(&session.shared.state).tracks.get_mut(&(track as u32)) {
        track.ended = true;
    }
    session.shared.wake.notify_all();
}

unsafe fn control(session: *mut MediaSession, action: impl FnOnce(&mut SessionState)) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    action(&mut lock(&session.shared.state));
    session.shared.wake.notify_all();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_play(session: *mut MediaSession) {
    unsafe {
        control(session, |state| {
            state.clock.set_playing(true);
            state.status.paused = 0;
        })
    };
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_pause(session: *mut MediaSession) {
    unsafe {
        control(session, |state| {
            state.clock.set_playing(false);
            state.status.paused = 1;
        })
    };
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_set_rate(session: *mut MediaSession, rate: f64) {
    unsafe {
        control(session, |state| {
            if rate == 0.0 {
                state.clock.set_playing(false);
            } else if rate != 1.0 {
                state.status.error = ERROR;
                state.clock.set_playing(false);
            }
        })
    };
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_set_volume(session: *mut MediaSession, volume: f64) {
    unsafe {
        control(session, |state| {
            if volume.is_finite() {
                state.volume = volume.clamp(0.0, 1.0);
            }
        })
    };
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_set_muted(session: *mut MediaSession, muted: u32) {
    unsafe { control(session, |state| state.muted = muted != 0) };
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_seek(session: *mut MediaSession, seconds: f64) {
    unsafe {
        control(session, |state| {
            if !seconds.is_finite() || seconds < 0.0 {
                state.status.error = ERROR;
                return;
            }
            state.generation = state.generation.wrapping_add(1);
            state.audio_generation = state.audio_generation.wrapping_add(1);
            state.input_bytes = 0;
            state.video.clear();
            let mut generation = state.next_track_generation;
            for track in state.tracks.values_mut() {
                track.samples.clear();
                track.ended = false;
                track.drained = false;
                track.cancellation.store(true, Ordering::Release);
                track.cancellation = Arc::new(AtomicBool::new(false));
                track.generation = generation;
                track.media_end_seconds = seconds;
                generation = generation.wrapping_add(1);
            }
            state.next_track_generation = generation;
            state.clock.seek(seconds);
            state.seek_target = Some(seconds);
            state.media_end_seconds = seconds;
            state.status.current_time = seconds;
            state.status.ended = 0;
            state.status.ready_state = if state.tracks.is_empty() { 0 } else { 1 };
        })
    };
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_status(
    session: *mut MediaSession,
    output: *mut MediaStatus,
) -> i32 {
    let (Some(session), Some(output)) = (unsafe { session.as_ref() }, unsafe { output.as_mut() })
    else {
        return ERROR;
    };
    let state = lock(&session.shared.state);
    *output = state.status;
    output.current_time = state.clock.now();
    OK
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_acquire_frame(
    session: *mut MediaSession,
    output: *mut MediaFrame,
) -> i32 {
    let (Some(session), Some(output)) = (unsafe { session.as_ref() }, unsafe { output.as_mut() })
    else {
        return ERROR;
    };
    let mut state = lock(&session.shared.state);
    if state.leases.len() >= 2 {
        return OK;
    }
    let now_us = (state.clock.now() * 1_000_000.0) as i64;
    // Seeking reenqueues from a preceding random access point. Decode those
    // pictures for reference state, but never present the preroll pictures.
    if let Some(target) = state.seek_target {
        while state.video.front().is_some_and(|frame| {
            (frame.pts_us as f64 + frame.duration_us as f64) / 1_000_000.0 <= target
        }) {
            state.video.pop_front();
            state.status.dropped_frames += 1;
        }
        if !state.video.is_empty() {
            state.seek_target = None;
        }
        session.shared.wake.notify_all();
    }
    let Some(next) = state.video.front() else {
        return OK;
    };
    if next.pts_us > now_us + 5_000 {
        return OK;
    }
    let frame = state.video.pop_front().expect("due frame");
    let lease = state.next_lease;
    state.next_lease = lease.wrapping_add(1).max(1);
    // Matrix values in the decoder are ISO/IEC VUI identifiers; the C ABI
    // names the three matrices Skia actually converts. Unspecified VUI uses
    // the codec's limited-range BT.601 default, never full-range by accident.
    let matrix = match frame.color.matrix {
        1 => 2,
        9 | 10 => 3,
        _ => 1,
    };
    *output = MediaFrame {
        lease,
        presentation_us: frame.pts_us,
        duration_us: frame.duration_us.min(i64::MAX as u64) as i64,
        format: 2,
        matrix,
        full_range: u32::from(frame.color.full_range),
        width: frame.width,
        height: frame.height,
        reserved: 0,
        planes: [
            frame.payload().as_ptr(),
            unsafe {
                frame
                    .payload()
                    .as_ptr()
                    .add(frame.width as usize * frame.height as usize)
            },
            std::ptr::null(),
        ],
        strides: [frame.width as usize, frame.width as usize, 0],
        plane_sizes: [
            frame.width as usize * frame.height as usize,
            frame.width as usize * (frame.height as usize / 2),
            0,
        ],
    };
    state.leases.insert(lease, frame);
    session.shared.wake.notify_all();
    1
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scarlet_media_session_release_frame(
    session: *mut MediaSession,
    lease: u64,
) {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return;
    };
    lock(&session.shared.state).leases.remove(&lease);
    session.shared.wake.notify_all();
}

/// PCM is retained until SAS actually consumes it. On pause we reset the ring,
/// trim only consumed frames, and submit the remaining samples again on resume.
/// No global SAS volume control is used: gain is local to this media element.
struct AudioChunk {
    frame: AudioFrame,
    consumed: usize,
    submitted: usize,
    ring_start: Option<u64>,
}

struct AudioOutput {
    chunks: VecDeque<AudioChunk>,
    ring: Option<AudioRing>,
    position: Option<f64>,
    playing: bool,
    draining: bool,
    failed: bool,
    minimum_seconds: f64,
    resampler: Option<PcmResampler>,
}

impl AudioOutput {
    fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
            ring: None,
            position: None,
            playing: false,
            draining: false,
            failed: false,
            minimum_seconds: 0.0,
            resampler: None,
        }
    }
    fn push(&mut self, frame: AudioFrame) -> Result<(), ()> {
        if !(1..=2).contains(&frame.channels)
            || frame.rate < 8000
            || frame.rate > 96000
            || frame.samples.len() % usize::from(frame.channels) != 0
            || frame.samples.len() > 8192
        {
            return Err(());
        }
        if self.draining {
            self.observe_consumed();
            if let Some(ring) = &mut self.ring {
                ring.reset();
            }
            self.rewind_unconsumed();
            self.draining = false;
            self.resampler = None;
        }
        if self.resampler.is_none() {
            self.resampler = Some(PcmResampler::new(frame.rate, frame.channels, frame.pts_us));
        }
        let mut frame = self
            .resampler
            .as_mut()
            .expect("initialized PCM resampler")
            .push(frame)?;
        if self.bytes() + frame.samples.len() * 2 > MAX_AUDIO_BYTES {
            return Err(());
        }
        if let Some(existing) = self.chunks.front() {
            if existing.frame.rate != frame.rate || existing.frame.channels != frame.channels {
                return Err(());
            }
        }
        let trim_frames = (((self.minimum_seconds - frame.pts_us as f64 / 1_000_000.0).max(0.0)
            * f64::from(frame.rate))
        .ceil() as usize)
            .min(frame.samples.len() / usize::from(frame.channels));
        if trim_frames != 0 {
            frame
                .samples
                .drain(..trim_frames * usize::from(frame.channels));
            frame.pts_us +=
                (trim_frames as f64 * 1_000_000.0 / f64::from(frame.rate)).round() as i64;
        }
        if !frame.samples.is_empty() {
            self.chunks.push_back(AudioChunk {
                frame,
                consumed: 0,
                submitted: 0,
                ring_start: None,
            });
        }
        Ok(())
    }
    fn bytes(&self) -> usize {
        self.chunks
            .iter()
            .map(|chunk| {
                (chunk.frame.samples.len() - chunk.consumed * usize::from(chunk.frame.channels)) * 2
            })
            .sum()
    }
    fn seconds(&self) -> f64 {
        self.chunks
            .iter()
            .map(|chunk| {
                (chunk.frame.samples.len() / usize::from(chunk.frame.channels) - chunk.consumed)
                    as f64
                    / f64::from(chunk.frame.rate)
            })
            .sum()
    }
    fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
    fn position(&self) -> Option<f64> {
        self.position
    }
    fn begin_drain(&mut self) {
        if let Some(frame) = self.resampler.as_mut().and_then(PcmResampler::finish) {
            self.chunks.push_back(AudioChunk {
                frame,
                consumed: 0,
                submitted: 0,
                ring_start: None,
            });
        }
        self.draining = true;
    }

    fn observe_consumed(&mut self) {
        let Some(ring) = &self.ring else {
            return;
        };
        let read = ring.read_frames();
        self.observe_consumed_count(read);
    }
    fn observe_consumed_count(&mut self, read: u64) {
        for chunk in &mut self.chunks {
            let Some(start) = chunk.ring_start else {
                break;
            };
            if read < start {
                break;
            }
            let frames = chunk.frame.samples.len() / usize::from(chunk.frame.channels);
            chunk.consumed = read.saturating_sub(start).min(chunk.submitted as u64) as usize;
            self.position = Some(
                chunk.frame.pts_us as f64 / 1_000_000.0
                    + chunk.consumed as f64 / f64::from(chunk.frame.rate),
            );
            if chunk.consumed != frames {
                break;
            }
        }
        while self.chunks.front().is_some_and(|chunk| {
            chunk.consumed == chunk.frame.samples.len() / usize::from(chunk.frame.channels)
        }) {
            self.chunks.pop_front();
        }
    }

    fn rewind_unconsumed(&mut self) {
        for chunk in &mut self.chunks {
            let channels = usize::from(chunk.frame.channels);
            if chunk.consumed != 0 {
                chunk.frame.samples.drain(..chunk.consumed * channels);
                chunk.frame.pts_us += (chunk.consumed as f64 * 1_000_000.0
                    / f64::from(chunk.frame.rate))
                .round() as i64;
            }
            chunk.consumed = 0;
            chunk.submitted = 0;
            chunk.ring_start = None;
        }
    }

    fn pump(
        &mut self,
        playing: bool,
        volume: f64,
        shared: &SessionShared,
        generation: u64,
    ) -> Result<(), ()> {
        if self.failed {
            return Err(());
        }
        self.observe_consumed();
        if !playing {
            if self.playing {
                if let Some(ring) = &mut self.ring {
                    ring.reset();
                }
                self.rewind_unconsumed();
            }
            self.playing = false;
            return Ok(());
        }
        self.playing = true;
        if self.ring.is_none() {
            let Some(chunk) = self.chunks.front() else {
                return Ok(());
            };
            self.position = Some(chunk.frame.pts_us as f64 / 1_000_000.0);
            match AudioRing::new(chunk.frame.rate, chunk.frame.channels, shared, generation) {
                Ok(Some(ring)) => self.ring = Some(ring),
                Ok(None) => return Ok(()), // seek/cancel interrupted configure
                Err(()) => {
                    self.failed = true;
                    return Err(());
                }
            }
        }
        let ring = self.ring.as_mut().expect("configured PCM ring");
        if ring.is_closed() {
            self.failed = true;
            return Err(());
        }
        let mut writable = ring.writable_frames();
        for chunk in &mut self.chunks {
            if writable == 0 {
                break;
            }
            let channels = usize::from(chunk.frame.channels);
            let total = chunk.frame.samples.len() / channels;
            let count = writable.min(total - chunk.submitted);
            if count == 0 {
                continue;
            }
            let start = chunk.submitted * channels;
            let end = start + count * channels;
            let bytes = pcm_bytes(&chunk.frame.samples[start..end], volume);
            if chunk.ring_start.is_none() {
                chunk.ring_start = Some(ring.write_frames());
            }
            let written = ring.write(&bytes);
            chunk.submitted += written;
            writable -= written;
        }
        if self.draining
            && self.chunks.iter().all(|chunk| {
                chunk.submitted == chunk.frame.samples.len() / usize::from(chunk.frame.channels)
            })
        {
            ring.begin_drain();
        }
        Ok(())
    }
}

fn pcm_bytes(samples: &[i16], volume: f64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let scaled = (f64::from(*sample) * volume)
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        bytes.extend_from_slice(&scaled.to_le_bytes());
    }
    bytes
}

/// Rational phase and the last source frame survive packet boundaries. Each
/// decoded source sample participates once; independently resampling AAC
/// packets would repeatedly lose a fractional frame and drift against video.
struct PcmResampler {
    source_rate: u32,
    channels: u16,
    source_frames: u64,
    output_frames: u64,
    previous: [i16; 2],
    base_pts_us: i64,
    finished: bool,
}

impl PcmResampler {
    fn new(source_rate: u32, channels: u16, base_pts_us: i64) -> Self {
        Self {
            source_rate,
            channels,
            source_frames: 0,
            output_frames: 0,
            previous: [0; 2],
            base_pts_us,
            finished: false,
        }
    }
    fn push(&mut self, frame: AudioFrame) -> Result<AudioFrame, ()> {
        if frame.rate != self.source_rate || frame.channels != self.channels || self.finished {
            return Err(());
        }
        let expected_us = self.base_pts_us as f64
            + self.source_frames as f64 * 1_000_000.0 / f64::from(self.source_rate);
        // The parser rounds timestamps to microseconds. Larger discontinuities
        // require an explicit flush/seek rather than silently joining audio.
        if (frame.pts_us as f64 - expected_us).abs() > 2_000.0 {
            return Err(());
        }
        let channels = usize::from(self.channels);
        let frame_count = frame.samples.len() / channels;
        if frame_count == 0 {
            return Ok(AudioFrame {
                samples: Vec::new(),
                rate: 48000,
                channels: self.channels,
                pts_us: frame.pts_us,
            });
        }
        let end = self.source_frames + frame_count as u64;
        let output_start = self.output_frames;
        let mut samples = Vec::new();
        loop {
            let position = self
                .output_frames
                .checked_mul(u64::from(self.source_rate))
                .ok_or(())?;
            let index = position / 48000;
            let fraction = position % 48000;
            if index + u64::from(fraction != 0) >= end {
                break;
            }
            for channel in 0..channels {
                let left = if index < self.source_frames {
                    if index + 1 != self.source_frames {
                        return Err(());
                    }
                    self.previous[channel]
                } else {
                    frame.samples[(index - self.source_frames) as usize * channels + channel]
                };
                let right = if fraction == 0 {
                    left
                } else {
                    frame.samples[(index + 1 - self.source_frames) as usize * channels + channel]
                };
                let interpolated = (i64::from(left) * (48000 - fraction) as i64
                    + i64::from(right) * fraction as i64)
                    / 48000;
                samples.push(interpolated as i16);
            }
            self.output_frames += 1;
        }
        self.previous[..channels].copy_from_slice(&frame.samples[frame.samples.len() - channels..]);
        self.source_frames = end;
        Ok(AudioFrame {
            samples,
            rate: 48000,
            channels: self.channels,
            pts_us: self.base_pts_us + (output_start as f64 * 1_000_000.0 / 48000.0).round() as i64,
        })
    }
    fn finish(&mut self) -> Option<AudioFrame> {
        if self.finished || self.source_frames == 0 {
            return None;
        }
        self.finished = true;
        let start = self.output_frames;
        let end = (self.source_frames * 48000).div_ceil(u64::from(self.source_rate));
        let mut samples = Vec::new();
        while self.output_frames < end {
            samples.extend_from_slice(&self.previous[..usize::from(self.channels)]);
            self.output_frames += 1;
        }
        if samples.is_empty() {
            None
        } else {
            Some(AudioFrame {
                samples,
                rate: 48000,
                channels: self.channels,
                pts_us: self.base_pts_us + (start as f64 * 1_000_000.0 / 48000.0).round() as i64,
            })
        }
    }
}

#[cfg(target_os = "scarlet")]
struct AudioRing {
    _client: sas_client::SasClient,
    stream: sas_client::SasStream,
}

#[cfg(target_os = "scarlet")]
impl AudioRing {
    fn new(
        rate: u32,
        channels: u16,
        shared: &SessionShared,
        generation: u64,
    ) -> Result<Option<Self>, ()> {
        let mut client = sas_client::SasClient::connect().map_err(|_| ())?;
        let period_frames = (rate / 100).max(64);
        let config = sas_client::StreamConfig {
            format: 1, // Scarlet AUDIO_PCM_FORMAT_S16LE ABI
            rate,
            channels,
            period_frames,
            buffer_frames: period_frames * 5,
        };
        let deadline = Instant::now() + Duration::from_millis(500);
        let stream = client
            .configure_cancellable(&config, || {
                let state = lock(&shared.state);
                state.stopping || state.audio_generation != generation || Instant::now() >= deadline
            })
            .map_err(|_| ())?;
        match stream {
            Some(stream) => Ok(Some(Self {
                _client: client,
                stream,
            })),
            None => {
                let state = lock(&shared.state);
                if state.stopping || state.audio_generation != generation {
                    Ok(None)
                } else {
                    Err(())
                }
            }
        }
    }
    fn read_frames(&self) -> u64 {
        self.stream.read_frames()
    }
    fn write_frames(&self) -> u64 {
        self.stream.write_frames()
    }
    fn writable_frames(&self) -> usize {
        self.stream.writable_frames()
    }
    fn is_closed(&self) -> bool {
        self.stream.is_closed()
    }
    fn write(&mut self, bytes: &[u8]) -> usize {
        self.stream.write(bytes)
    }
    fn reset(&mut self) {
        self.stream.reset();
    }
    fn begin_drain(&mut self) {
        self.stream.begin_drain();
    }
}

// Host tests exercise demux/decode and ownership. SAS is a Scarlet device and
// cannot be reported as available on the host merely to make a test pass.
#[cfg(not(target_os = "scarlet"))]
struct AudioRing;
#[cfg(not(target_os = "scarlet"))]
impl AudioRing {
    fn new(_: u32, _: u16, _: &SessionShared, _: u64) -> Result<Option<Self>, ()> {
        Err(())
    }
    fn read_frames(&self) -> u64 {
        0
    }
    fn write_frames(&self) -> u64 {
        0
    }
    fn writable_frames(&self) -> usize {
        0
    }
    fn is_closed(&self) -> bool {
        true
    }
    fn write(&mut self, _: &[u8]) -> usize {
        0
    }
    fn reset(&mut self) {}
    fn begin_drain(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_rejects_codecs_we_do_not_decode() {
        for codec in [
            "avc1.42E01E",
            "avc1.4D401F",
            "avc1.640028",
            "avc1.640028,mp4a.40.2",
        ] {
            assert!(supported("video/mp4", codec), "{codec}");
        }
        for codec in [
            "vp09.00.10.08",
            "av01.0.08M.08",
            "mp4a.40.5",
            "avc1.nothex",
            "avc1.6e0028",
            "avc1.64ffff",
            "avc1.640009",
            "avc1.64003e",
            "avc3.64002a",
            "opus",
        ] {
            assert!(!supported("video/mp4", codec), "{codec}");
        }
        assert!(!supported("audio/mp4", "avc1.42E01E"));
        assert!(!supported("video/webm", "vp9"));
        assert!(supported("audio/mp4", "mp4a.40.2"));
    }

    #[test]
    fn native_decode_order_is_reordered_by_pts_and_drained_at_eof() {
        let mut reorder = PresentationReorder::new(2).unwrap();
        let mut presented = Vec::new();
        for pts in [0, 375_000, 125_000, 250_000, 750_000, 500_000, 625_000] {
            if let Some(frame) = reorder.push(pts, pts) {
                presented.push(frame);
            }
            assert!(reorder.pending.len() <= 2);
        }
        presented.extend(reorder.finish());
        assert_eq!(
            presented,
            [0, 125_000, 250_000, 375_000, 500_000, 625_000, 750_000]
        );
        assert!(reorder.finish().is_empty());
        assert!(PresentationReorder::<i64>::new(3).is_err());
        let mut no_reorder = PresentationReorder::new(0).unwrap();
        assert_eq!(no_reorder.push(0, 17), Some(17));
        assert!(no_reorder.finish().is_empty());
    }

    #[cfg(not(target_os = "scarlet"))]
    #[test]
    fn hardware_absence_is_not_hidden_by_a_software_video_decoder() {
        let video = b"video/mp4";
        let audio = b"audio/mp4";
        let avc = b"avc1.42C01E";
        let aac = b"mp4a.40.2";
        unsafe {
            assert_eq!(
                scarlet_media_supports_type(
                    video.as_ptr().cast(),
                    video.len(),
                    avc.as_ptr().cast(),
                    avc.len(),
                    1
                ),
                0
            );
            assert_eq!(
                scarlet_media_supports_type(
                    audio.as_ptr().cast(),
                    audio.len(),
                    aac.as_ptr().cast(),
                    aac.len(),
                    1
                ),
                1
            );
        }
        assert!(!native_h264_available());
    }

    #[test]
    fn flush_and_destroy_cancel_the_native_generation() {
        let session = scarlet_media_session_create();
        let mut track = empty_track();
        track.identifier = 1;
        track.kind = 1;
        track.codec = 1;
        track.width = 64;
        track.height = 48;
        unsafe {
            assert_eq!(scarlet_media_session_configure(session, &track), OK);
        }
        let shared = unsafe { session.as_ref().unwrap().shared.clone() };
        let first = lock(&shared.state).tracks[&1].cancellation.clone();
        unsafe {
            scarlet_media_session_flush(session, 1);
        }
        assert!(first.load(Ordering::Acquire));
        let second = lock(&shared.state).tracks[&1].cancellation.clone();
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(!second.load(Ordering::Acquire));
        unsafe {
            scarlet_media_session_destroy(session);
        }
        assert!(second.load(Ordering::Acquire));
    }

    #[test]
    fn playback_clock_waits_for_data_and_pause_seek_are_stable() {
        let mut clock = PlaybackClock::new();
        clock.set_playing(true);
        assert_eq!(clock.at(clock.anchor + Duration::from_secs(2)), 0.0);
        clock.mark_ready();
        assert_eq!(clock.at(clock.anchor + Duration::from_secs(2)), 2.0);
        clock.set_playing(false);
        let paused = clock.seconds;
        assert_eq!(clock.at(clock.anchor + Duration::from_secs(100)), paused);
        clock.seek(17.5);
        clock.set_playing(true);
        assert_eq!(clock.at(clock.anchor + Duration::from_secs(100)), 17.5);
        clock.mark_ready();
        assert_eq!(clock.at(clock.anchor + Duration::from_secs(1)), 18.5);
    }

    #[test]
    fn audio_clock_uses_consumed_timestamp_instead_of_wall_time() {
        let mut clock = PlaybackClock::new();
        clock.audio_master = true;
        clock.set_playing(true);
        clock.mark_ready();
        assert_eq!(clock.at(clock.anchor + Duration::from_secs(100)), 0.0);
        clock.audio_position(2.125);
        assert_eq!(clock.at(clock.anchor + Duration::from_secs(100)), 2.125);
        clock.set_playing(false);
        clock.audio_position(3.0);
        assert_eq!(clock.now(), 2.125);
    }

    #[test]
    fn video_outlasting_audio_hands_off_from_consumed_clock() {
        let mut clock = PlaybackClock::new();
        clock.audio_master = true;
        clock.set_playing(true);
        clock.audio_position(1.25);
        clock.finish_audio();
        assert_eq!(clock.at(clock.anchor + Duration::from_millis(750)), 2.0);
        clock.set_playing(false);
        let paused = clock.now();
        assert_eq!(clock.at(clock.anchor + Duration::from_secs(10)), paused);
    }

    #[test]
    fn removing_audio_allows_new_audio_track_and_preserves_video_metadata() {
        let session = scarlet_media_session_create();
        let mut video = empty_track();
        video.identifier = 1;
        video.kind = 1;
        video.codec = 1;
        video.width = 64;
        video.height = 48;
        video.duration = 4.0;
        let mut audio = empty_track();
        audio.identifier = 2;
        audio.kind = 2;
        audio.codec = 2;
        audio.sample_rate = 44100;
        audio.channels = 2;
        audio.duration = 3.0;
        unsafe {
            assert_eq!(scarlet_media_session_configure(session, &video), OK);
            assert_eq!(scarlet_media_session_configure(session, &audio), OK);
            let mut status = MediaStatus::default();
            scarlet_media_session_status(session, &mut status);
            assert_eq!(
                (status.duration, status.has_video, status.has_audio),
                (4.0, 1, 1)
            );
            scarlet_media_session_remove_track(session, 2);
            scarlet_media_session_status(session, &mut status);
            assert_eq!(
                (status.duration, status.has_video, status.has_audio),
                (4.0, 1, 0)
            );
            assert!(
                !lock(&session.as_ref().unwrap().shared.state)
                    .clock
                    .audio_master
            );
            audio.identifier = 99;
            assert_eq!(scarlet_media_session_configure(session, &audio), OK);
            scarlet_media_session_status(session, &mut status);
            assert_eq!((status.has_audio, status.error), (1, 0));
            scarlet_media_session_destroy(session);
        }
    }

    fn audio_frame(samples: Vec<i16>, rate: u32, channels: u16, pts_us: i64) -> AudioFrame {
        AudioFrame {
            samples,
            rate,
            channels,
            pts_us,
        }
    }

    #[test]
    fn resampling_is_identical_across_irregular_packet_boundaries() {
        let source: Vec<i16> = (0..44100)
            .flat_map(|index| {
                let value = ((index % 8192) as i32 - 4096) as i16;
                [value, -value]
            })
            .collect();
        let mut whole = PcmResampler::new(44100, 2, 0);
        let mut expected = whole
            .push(audio_frame(source.clone(), 44100, 2, 0))
            .unwrap()
            .samples;
        expected.extend(whole.finish().unwrap().samples);
        assert_eq!(expected.len(), 48000 * 2);
        let mut split = PcmResampler::new(44100, 2, 0);
        let mut actual = Vec::new();
        let mut position = 0;
        for count in [1, 1024, 17, 511, 4096, 3].into_iter().cycle() {
            if position == 44100 {
                break;
            }
            let count = count.min(44100 - position);
            let output = split
                .push(audio_frame(
                    source[position * 2..(position + count) * 2].to_vec(),
                    44100,
                    2,
                    (position as f64 / 44100.0 * 1_000_000.0).round() as i64,
                ))
                .unwrap();
            assert_eq!(
                output.pts_us,
                (actual.len() as f64 / 2.0 / 48000.0 * 1_000_000.0).round() as i64
            );
            actual.extend(output.samples);
            position += count;
        }
        actual.extend(split.finish().unwrap().samples);
        assert_eq!(actual, expected);
        assert!(split.finish().is_none());
    }

    #[test]
    fn native_48khz_pcm_is_bit_exact_and_volume_is_player_local() {
        let samples = vec![i16::MIN, i16::MAX, -1000, 1000];
        let mut resampler = PcmResampler::new(48000, 2, 1_000_000);
        let output = resampler
            .push(audio_frame(samples.clone(), 48000, 2, 1_000_000))
            .unwrap();
        assert_eq!(output.samples, samples);
        assert!(resampler.finish().is_none());
        assert_eq!(pcm_bytes(&samples, 0.0), vec![0; 8]);
        let expected: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        assert_eq!(pcm_bytes(&samples, 1.0), expected);
        assert_eq!(
            pcm_bytes(&[-1000, 1000], 0.5),
            [(-500i16).to_le_bytes(), 500i16.to_le_bytes()].concat()
        );
    }

    #[test]
    fn pause_retains_exactly_unconsumed_pcm_and_pts() {
        let mut output = AudioOutput::new();
        let samples: Vec<i16> = (0..2000).map(|sample| sample as i16).collect();
        output
            .push(audio_frame(samples.clone(), 48000, 2, 2_000_000))
            .unwrap();
        let chunk = output.chunks.front_mut().unwrap();
        chunk.ring_start = Some(100);
        chunk.submitted = 1000;
        output.observe_consumed_count(350);
        assert_eq!(output.bytes(), 750 * 2 * 2);
        assert!((output.position().unwrap() - (2.0 + 250.0 / 48000.0)).abs() < 1e-10);
        output.rewind_unconsumed();
        let chunk = output.chunks.front().unwrap();
        assert_eq!(chunk.frame.samples, samples[500..]);
        assert_eq!(chunk.frame.pts_us, 2_005_208);
        assert_eq!(chunk.submitted, 0);
        assert_eq!(chunk.consumed, 0);
        assert_eq!(chunk.ring_start, None);
    }

    #[test]
    fn consumed_audio_does_not_jump_to_a_future_queued_block() {
        let mut output = AudioOutput::new();
        for index in 0..2 {
            output
                .push(audio_frame(vec![0; 960], 48000, 2, index * 10_000))
                .unwrap();
            let chunk = output.chunks.back_mut().unwrap();
            chunk.ring_start = Some(index as u64 * 480);
            chunk.submitted = 480;
        }
        output.observe_consumed_count(200);
        assert!((output.position().unwrap() - 200.0 / 48000.0).abs() < 1e-10);
        assert_eq!(output.chunks.len(), 2);
        output.observe_consumed_count(960);
        assert!(output.is_empty());
        assert_eq!(output.position(), Some(0.02));
    }

    #[test]
    fn resampler_requires_explicit_flush_for_timestamp_discontinuity() {
        let mut resampler = PcmResampler::new(44100, 1, 0);
        resampler
            .push(audio_frame(vec![0; 1024], 44100, 1, 0))
            .unwrap();
        assert!(
            resampler
                .push(audio_frame(vec![0; 1024], 44100, 1, 900_000))
                .is_err()
        );
    }

    #[test]
    fn encoded_input_backpressure_preserves_caller_bytes_and_reset_capacity() {
        let shared = Arc::new(ParserShared {
            state: Mutex::new(ParserState {
                generation: 1,
                stopping: false,
                input: VecDeque::new(),
                input_bytes: 0,
                output: VecDeque::new(),
                output_bytes: 0,
                leases: BTreeMap::new(),
                next_lease: 1,
            }),
            wake: Condvar::new(),
        });
        let parser = Box::into_raw(Box::new(MediaParser {
            shared: shared.clone(),
            worker: None,
        }));
        let mut bytes = vec![23; MAX_INPUT_CHUNK];
        unsafe {
            for _ in 0..MAX_INPUT_BYTES / MAX_INPUT_CHUNK {
                assert_eq!(
                    scarlet_media_parser_append(parser, bytes.as_ptr(), bytes.len(), 17),
                    OK
                );
            }
            bytes[0] = 99;
            assert_eq!(
                scarlet_media_parser_append(parser, bytes.as_ptr(), 1, 18),
                AGAIN
            );
        }
        let state = lock(&shared.state);
        let ParseInput::Bytes { bytes, append, .. } = state.input.front().unwrap() else {
            panic!()
        };
        assert_eq!(bytes[0], 23);
        assert_eq!(*append, 17);
        drop(state);
        unsafe {
            scarlet_media_parser_reset(parser);
            assert_eq!(scarlet_media_parser_append(parser, [1].as_ptr(), 1, 19), OK);
            scarlet_media_parser_destroy(parser);
        }
    }

    #[test]
    fn abi_has_identical_native_64bit_field_layout() {
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<MediaTrack>(), 56);
        assert_eq!(offset_of!(MediaTrack, duration), 32);
        assert_eq!(size_of::<MediaSample>(), 56);
        assert_eq!(offset_of!(MediaSample, data), 40);
        assert_eq!(size_of::<MediaParserEvent>(), 136);
        assert_eq!(size_of::<MediaFrame>(), 120);
        assert_eq!(offset_of!(MediaFrame, planes), 48);
        assert_eq!(size_of::<MediaStatus>(), 56);
    }

    /// The harness generates this file with an independent encoder in an
    /// automatically removed temporary directory. This is a real worker/ABI
    /// test; it never substitutes fake decoded pictures or a fake device.
    #[test]
    #[ignore = "requires SCARLET_MEDIA_BRIDGE_FIXTURE generated by ffmpeg"]
    fn real_encoded_video_async_demux_preserves_packets_and_timestamps() {
        let data =
            std::fs::read(std::env::var("SCARLET_MEDIA_BRIDGE_FIXTURE").expect("fixture path"))
                .unwrap();
        let mime = b"video/mp4";
        let parser = unsafe { scarlet_media_parser_create(mime.as_ptr().cast(), mime.len(), 0) };
        assert!(!parser.is_null());
        let chunks = data.chunks(257).collect::<Vec<_>>();
        let mut input = 0;
        let mut done = 0;
        let mut ended = false;
        let mut track = None;
        let mut samples = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while done != chunks.len() + 1 {
            assert!(Instant::now() < deadline, "asynchronous parser stalled");
            if let Some(chunk) = chunks.get(input) {
                let result = unsafe {
                    scarlet_media_parser_append(parser, chunk.as_ptr(), chunk.len(), input as u64)
                };
                assert!(result == OK || result == AGAIN);
                if result == OK {
                    input += 1;
                }
            } else if !ended {
                let result = unsafe { scarlet_media_parser_end(parser, input as u64) };
                assert!(result == OK || result == AGAIN);
                ended = result == OK;
            }
            let mut event = MediaParserEvent {
                lease: 0,
                append_identifier: 0,
                kind: 0,
                error: 0,
                track: empty_track(),
                sample: empty_sample(),
            };
            if unsafe { scarlet_media_parser_poll(parser, &mut event) } == 1 {
                match event.kind {
                    1 => track = Some(unsafe { owned_track(&event.track) }.unwrap()),
                    2 => samples.push(Sample {
                        track_id: event.sample.track as u32,
                        data: unsafe {
                            copy_bytes(event.sample.data, event.sample.size, MAX_INPUT_BYTES)
                        }
                        .unwrap(),
                        pts_us: event.sample.presentation_us,
                        dts_us: event.sample.decode_us,
                        duration_us: event.sample.duration_us as u64,
                        keyframe: event.sample.flags & 1 != 0,
                    }),
                    3 => done += 1,
                    _ => panic!("media parse error {}", event.error),
                }
                unsafe {
                    scarlet_media_parser_release(parser, event.lease);
                }
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
        unsafe {
            scarlet_media_parser_destroy(parser);
        }
        let track = track.unwrap();
        assert_eq!(track.codec, Codec::H264);
        assert_eq!(samples.len(), 16);
        assert_eq!((track.width, track.height), (64, 48));
        assert!(
            samples
                .iter()
                .enumerate()
                .all(|(index, sample)| sample.pts_us == index as i64 * 125_000
                    && sample.duration_us == 125_000)
        );
        assert!(samples.iter().all(|sample| !sample.data.is_empty()));
        let mut configuration = scarlet_codecs::media::AvcConfiguration::new(&track).unwrap();
        for sample in &samples {
            let annex_b = configuration.annex_b(sample).unwrap();
            assert!(annex_b.starts_with(&[0, 0, 0, 1]));
            assert!(annex_b_has_picture(&annex_b));
        }
    }
}
