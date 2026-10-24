//! Encrypted H.264 decoder. CDM calls execute on the media worker, serialized
//! with licence/session operations by CdmShared, outside the WebCore thread.
use super::*;
use cdm_host::{CdmHostError, DecryptRequest, STATUS_NO_KEY, Subsample};

pub(super) struct CdmVideoDecoder {
    cdm: AttachedCdm,
    owner: u64,
    configuration: Vec<u8>,
    color: ColorInfo,
    next_timestamp: i64,
    metadata: BTreeMap<i64, (i64, u64)>,
    cancellation: Arc<AtomicBool>,
    frames: u64,
}

impl CdmVideoDecoder {
    pub(super) fn new(
        cdm: AttachedCdm,
        track: &Track,
        cancellation: Arc<AtomicBool>,
    ) -> Result<Self, MediaError> {
        let configuration = scarlet_codecs::media::AvcConfiguration::new(track)?;
        let profile = match track.extradata.get(1) {
            Some(66) => 2,
            Some(77) => 3,
            Some(88) => 4,
            Some(100) => 5,
            _ => return Err(MediaError::Unsupported("unsupported CDM H.264 profile")),
        };
        let owner = cdm
            .configure_video(profile, configuration.width(), configuration.height())
            .map_err(|e| MediaError::Decode(format!("CDM configure: {e:?}")))?;
        eprintln!(
            "[myrica:media] video-decoder=widevine-cdm output=nv12 width={} height={} profile={profile}",
            configuration.width(),
            configuration.height()
        );
        Ok(Self {
            cdm,
            owner,
            configuration: track.extradata.clone(),
            color: configuration.color(),
            next_timestamp: 1,
            metadata: BTreeMap::new(),
            cancellation,
            frames: 0,
        })
    }

    pub(super) fn uses_cdm(&self, cdm: &AttachedCdm) -> bool {
        Arc::ptr_eq(&self.cdm, cdm)
    }

    pub(super) fn pending_frames(&self) -> usize {
        // Drain one output per worker turn so the three-frame display queue
        // remains bounded even when the CDM retains multiple reference frames.
        1
    }

    pub(super) fn push(&mut self, sample: Option<&Sample>) -> Result<DecodeOutput, MediaError> {
        if self.cancellation.load(Ordering::Acquire) {
            return Ok(DecodeOutput::Video(Vec::new()));
        }
        let mut prepared = None;
        let mut subsamples = Vec::new();
        if let Some(sample) = sample {
            if self.metadata.len() >= 64 {
                return Err(MediaError::InvalidData(
                    "CDM video timestamp queue exceeded capacity",
                ));
            }
            let map: Vec<_> = sample
                .encryption
                .as_ref()
                .map(|e| {
                    e.subsamples
                        .iter()
                        .map(|s| (s.clear_bytes, s.encrypted_bytes))
                        .collect()
                })
                .unwrap_or_else(|| vec![(sample.data.len() as u32, 0)]);
            let unit = crate::cdm_avc::AvcCdmSample::new(
                &self.configuration,
                &sample.data,
                &map,
                sample.keyframe,
            )
            .map_err(MediaError::InvalidData)?;
            subsamples = unit
                .subsamples
                .iter()
                .map(|&(clear_bytes, cipher_bytes)| Subsample {
                    clear_bytes,
                    cipher_bytes,
                })
                .collect();
            prepared = Some(unit);
            self.metadata
                .insert(self.next_timestamp, (sample.pts_us, sample.duration_us));
        }
        let encryption = sample.and_then(|s| s.encryption.as_ref());
        let request = DecryptRequest {
            sample: prepared.as_ref().map(|u| u.data.as_slice()).unwrap_or(&[]),
            key_id: encryption.map(|e| e.key_id.as_slice()).unwrap_or(&[]),
            iv: encryption.map(|e| e.iv.as_slice()).unwrap_or(&[]),
            subsamples: &subsamples,
            scheme: if encryption.is_some() {
                cdm_host::SCHEME_CENC
            } else {
                cdm_host::SCHEME_UNENCRYPTED
            },
            timestamp: self.next_timestamp,
        };
        let picture = match self.cdm.decode_video(self.owner, &request) {
            Ok(picture) => picture,
            Err(CdmHostError::Status(STATUS_NO_KEY)) if sample.is_some() => {
                self.metadata.remove(&self.next_timestamp);
                return Ok(DecodeOutput::WaitingForKey);
            }
            Err(error) => return Err(MediaError::Decode(format!("CDM video: {error:?}"))),
        };
        if sample.is_some() {
            self.next_timestamp += 1;
        }
        let Some((info, buffer)) = picture else {
            if sample.is_none() {
                self.metadata.clear();
            }
            return Ok(DecodeOutput::Video(Vec::new()));
        };
        // Both planar formats identify U/V by plane index; physical order is
        // represented by offsets. Ten-bit formats require a different upload.
        if info.format != 1 && info.format != 2 {
            return Err(MediaError::Unsupported(
                "CDM output is not 8-bit planar video",
            ));
        }
        let (pts_us, duration_us) = self
            .metadata
            .remove(&info.timestamp)
            .ok_or(MediaError::InvalidData("CDM video timestamp not submitted"))?;
        let pixels = super::cdm_pixels::to_nv12(
            &buffer,
            info.width as usize,
            info.height as usize,
            [
                info.stride_y as usize,
                info.stride_u as usize,
                info.stride_v as usize,
            ],
            [0, info.offset_u as usize, info.offset_v as usize],
        )
        .map_err(MediaError::InvalidData)?;
        self.frames += 1;
        if crate::cdm_bridge::trace_enabled() && (self.frames == 1 || self.frames % 128 == 0) {
            eprintln!(
                "SCARLET_CDM_VIDEO frames={} width={} height={} pts_us={pts_us}",
                self.frames, info.width, info.height
            );
        }
        Ok(DecodeOutput::Video(vec![VideoFrame {
            width: info.width,
            height: info.height,
            pts_us,
            duration_us,
            color: self.color,
            pixels: VideoPixels::Cdm(pixels),
        }]))
    }
}

impl Drop for CdmVideoDecoder {
    fn drop(&mut self) {
        self.cdm.release_video(self.owner);
    }
}
