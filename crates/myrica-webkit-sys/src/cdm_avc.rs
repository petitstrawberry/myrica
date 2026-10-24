//! H.264 framing at the CDM boundary. CDMs receive Annex B access units with
//! their clear parameter sets and the adjusted CENC subsample boundaries.
use std::ops::Range;

pub(crate) struct AvcCdmSample {
    pub data: Vec<u8>,
    pub subsamples: Vec<(u32, u32)>,
}

#[derive(Clone)]
struct Span {
    range: Range<usize>,
    encrypted: bool,
}

impl AvcCdmSample {
    pub fn new(
        configuration: &[u8],
        sample: &[u8],
        subsamples: &[(u32, u32)],
        keyframe: bool,
    ) -> Result<Self, &'static str> {
        if configuration.len() < 7 || configuration[0] != 1 {
            return Err("invalid CDM AVC configuration");
        }
        let length_size = usize::from((configuration[4] & 3) + 1);
        if length_size == 3 {
            return Err("invalid CDM AVC length size");
        }
        let mut spans = Vec::new();
        let mut position = 0usize;
        for &(clear, cipher) in subsamples {
            for (size, encrypted) in [(clear, false), (cipher, true)] {
                let end = position
                    .checked_add(size as usize)
                    .ok_or("AVC map overflow")?;
                if end > sample.len() {
                    return Err("AVC map exceeds sample");
                }
                if end != position {
                    spans.push(Span {
                        range: position..end,
                        encrypted,
                    });
                }
                position = end;
            }
        }
        if position != sample.len() || sample.is_empty() {
            return Err("AVC framing requires a complete subsample map");
        }
        let mut result = Self {
            data: Vec::new(),
            subsamples: Vec::new(),
        };
        let mut parameters = Vec::new();
        if keyframe {
            let mut offset = 6usize;
            let sps_count = configuration[5] & 31;
            if sps_count == 0 {
                return Err("CDM AVC configuration has no SPS");
            }
            for _ in 0..sps_count {
                parameters.push(read_parameter(configuration, &mut offset, 7)?);
            }
            let pps_count = *configuration
                .get(offset)
                .ok_or("missing CDM AVC PPS count")?;
            offset += 1;
            if pps_count == 0 {
                return Err("CDM AVC configuration has no PPS");
            }
            for _ in 0..pps_count {
                parameters.push(read_parameter(configuration, &mut offset, 8)?);
            }
        }
        let mut offset = 0usize;
        let mut span_index = 0usize;
        let mut inserted_parameters = false;
        while offset < sample.len() {
            let prefix_end = offset
                .checked_add(length_size)
                .ok_or("AVC length overflow")?;
            let prefix = sample
                .get(offset..prefix_end)
                .ok_or("truncated AVC length")?;
            let header_end = prefix_end.checked_add(1).ok_or("AVC header overflow")?;
            if !range_is_clear(&spans, offset..header_end) {
                return Err("encrypted AVC length or NAL header");
            }
            let size = prefix
                .iter()
                .fold(0usize, |n, byte| (n << 8) | usize::from(*byte));
            let end = prefix_end.checked_add(size).ok_or("AVC NAL overflow")?;
            if size == 0 || end > sample.len() {
                return Err("invalid AVC NAL length");
            }
            // The access-unit delimiter, when present, must remain first.
            let first_aud = offset == 0 && sample[prefix_end] & 31 == 9;
            if !inserted_parameters && !first_aud {
                result.append_parameters(&parameters)?;
                inserted_parameters = true;
            }
            result.append(&[0, 0, 0, 1], false)?;
            let mut source = prefix_end;
            while source < end {
                while span_index < spans.len() && spans[span_index].range.end <= source {
                    span_index += 1;
                }
                let span = spans.get(span_index).ok_or("incomplete AVC map")?;
                let next = end.min(span.range.end);
                result.append(&sample[source..next], span.encrypted)?;
                source = next;
            }
            offset = end;
        }
        Ok(result)
    }

    fn append_parameters(&mut self, parameters: &[&[u8]]) -> Result<(), &'static str> {
        for parameter in parameters {
            self.append(&[0, 0, 0, 1], false)?;
            self.append(parameter, false)?;
        }
        Ok(())
    }

    fn append(&mut self, bytes: &[u8], encrypted: bool) -> Result<(), &'static str> {
        if bytes.is_empty() {
            return Ok(());
        }
        let size = u32::try_from(bytes.len()).map_err(|_| "oversized CDM AVC data")?;
        if self.subsamples.is_empty() || (!encrypted && self.subsamples.last().unwrap().1 != 0) {
            self.subsamples.push((0, 0));
        }
        let last = self.subsamples.last_mut().unwrap();
        let count = if encrypted { &mut last.1 } else { &mut last.0 };
        *count = count.checked_add(size).ok_or("CDM AVC map overflow")?;
        self.data.extend_from_slice(bytes);
        Ok(())
    }
}

fn read_parameter<'a>(
    data: &'a [u8],
    offset: &mut usize,
    kind: u8,
) -> Result<&'a [u8], &'static str> {
    let size_bytes = data
        .get(*offset..*offset + 2)
        .ok_or("truncated AVC parameter length")?;
    let size = usize::from(u16::from_be_bytes([size_bytes[0], size_bytes[1]]));
    *offset += 2;
    let parameter = data
        .get(*offset..*offset + size)
        .ok_or("truncated AVC parameter")?;
    if parameter.first().is_none_or(|byte| byte & 31 != kind) {
        return Err("invalid AVC parameter type");
    }
    *offset += size;
    Ok(parameter)
}

fn range_is_clear(spans: &[Span], range: Range<usize>) -> bool {
    let first = spans.partition_point(|span| span.range.end <= range.start);
    let mut covered = range.start;
    for span in &spans[first..] {
        if span.range.start > covered || span.encrypted {
            return false;
        }
        covered = covered.max(span.range.end);
        if covered >= range.end {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(length: u8) -> Vec<u8> {
        vec![
            1,
            66,
            0,
            30,
            0xfc | (length - 1),
            0xe1,
            0,
            2,
            0x67,
            0x80,
            1,
            0,
            2,
            0x68,
            0x80,
        ]
    }

    fn cipher_bytes(bytes: &[u8], map: &[(u32, u32)]) -> Vec<u8> {
        let mut offset = 0;
        let mut result = Vec::new();
        for &(clear, cipher) in map {
            offset += clear as usize;
            result.extend_from_slice(&bytes[offset..offset + cipher as usize]);
            offset += cipher as usize;
        }
        assert_eq!(offset, bytes.len());
        result
    }

    #[test]
    fn length_expansion_preserves_cipher_order() {
        for length in [1, 2, 4] {
            let mut sample = Vec::new();
            for nal in [[0x65, 0x80, 0xa1, 0xa2], [0x41, 0x80, 0xb1, 0xb2]] {
                sample.extend_from_slice(&4u32.to_be_bytes()[4 - length as usize..]);
                sample.extend_from_slice(&nal);
            }
            let map = [(u32::from(length) + 2, 2), (u32::from(length) + 2, 2)];
            let prepared = AvcCdmSample::new(&config(length), &sample, &map, true).unwrap();
            assert_eq!(
                &prepared.data[..12],
                &[0, 0, 0, 1, 0x67, 0x80, 0, 0, 0, 1, 0x68, 0x80]
            );
            assert_eq!(
                cipher_bytes(&prepared.data, &prepared.subsamples),
                cipher_bytes(&sample, &map)
            );
            assert_eq!(prepared.subsamples, [(18, 2), (6, 2)]);
        }
    }

    #[test]
    fn keeps_access_unit_delimiter_before_parameter_sets() {
        let sample = [2, 0x09, 0xf0, 3, 0x65, 0x80, 0xaa];
        let prepared = AvcCdmSample::new(&config(1), &sample, &[(6, 1)], true).unwrap();
        assert_eq!(
            &prepared.data[..12],
            &[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 0x80]
        );
        assert_eq!(prepared.subsamples, [(24, 1)]);
    }

    #[test]
    fn rejects_encrypted_framing_and_invalid_maps_without_touching_input() {
        let sample = [3, 0x65, 0x80, 0xaa];
        for map in [vec![(0, 4)], vec![(1, 3)], vec![(3, 0)], vec![(5, 0)]] {
            assert!(AvcCdmSample::new(&config(1), &sample, &map, true).is_err());
        }
        for bad in [&[8, 0x65, 0x80, 0xaa][..], &[0][..], &[][..]] {
            assert!(AvcCdmSample::new(&config(1), bad, &[(bad.len() as u32, 0)], true).is_err());
        }
        let prepared = AvcCdmSample::new(&config(1), &sample, &[(3, 1)], false).unwrap();
        assert_eq!(prepared.data, [0, 0, 0, 1, 0x65, 0x80, 0xaa]);
    }
}
