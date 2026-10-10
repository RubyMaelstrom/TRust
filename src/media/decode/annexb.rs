//! ISO/IEC 14496-15 length-prefixed H.264/HEVC samples to the Annex B byte
//! stream (start codes) that decoders such as Media Foundation's take, with
//! the configuration record's parameter sets (avcC §5.3.3.1, hvcC
//! §8.3.3.1) before every random access point, as Chrome and Firefox feed
//! them.

const START_CODE: [u8; 4] = [0, 0, 0, 1];

pub struct AnnexB {
    length_size: usize,
    /// The parameter sets, each behind a start code.
    parameter_sets: Vec<u8>,
}

fn push_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nal);
}

/// Read `count` u16-length-prefixed NAL units at `at`.
fn read_nals(config: &[u8], mut at: usize, count: usize, out: &mut Vec<u8>) -> Option<usize> {
    for _ in 0..count {
        let len = usize::from(u16::from_be_bytes([*config.get(at)?, *config.get(at + 1)?]));
        let nal = config.get(at + 2..at + 2 + len)?;
        push_nal(out, nal);
        at += 2 + len;
    }
    Some(at)
}

impl AnnexB {
    /// From an AVCDecoderConfigurationRecord.
    pub fn from_avcc(config: &[u8]) -> Option<Self> {
        if config.len() < 7 || config[0] != 1 {
            return None;
        }
        let length_size = usize::from(config[4] & 3) + 1;
        let mut parameter_sets = Vec::new();
        let at = read_nals(
            config,
            6,
            usize::from(config[5] & 0x1F),
            &mut parameter_sets,
        )?;
        let pps = usize::from(*config.get(at)?);
        read_nals(config, at + 1, pps, &mut parameter_sets)?;
        Some(Self {
            length_size,
            parameter_sets,
        })
    }

    /// From an HEVCDecoderConfigurationRecord.
    pub fn from_hvcc(config: &[u8]) -> Option<Self> {
        if config.len() < 23 {
            return None;
        }
        let length_size = usize::from(config[21] & 3) + 1;
        let mut parameter_sets = Vec::new();
        let mut at = 23;
        for _ in 0..config[22] {
            let count = usize::from(u16::from_be_bytes([
                *config.get(at + 1)?,
                *config.get(at + 2)?,
            ]));
            at = read_nals(config, at + 3, count, &mut parameter_sets)?;
        }
        Some(Self {
            length_size,
            parameter_sets,
        })
    }

    /// The parameter sets as Annex B (a decoder's sequence header).
    pub fn parameter_sets(&self) -> &[u8] {
        &self.parameter_sets
    }

    /// One sample in Annex B, the parameter sets first at a random access
    /// point; `None` when its NAL unit lengths overrun it.
    pub fn convert(&self, sample: &[u8], keyframe: bool) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(
            sample.len()
                + 16
                + if keyframe {
                    self.parameter_sets.len()
                } else {
                    0
                },
        );
        if keyframe {
            out.extend_from_slice(&self.parameter_sets);
        }
        let mut at = 0;
        while at < sample.len() {
            let header = sample.get(at..at + self.length_size)?;
            let len = header
                .iter()
                .fold(0usize, |acc, &byte| (acc << 8) | usize::from(byte));
            let nal = sample.get(at + self.length_size..at + self.length_size + len)?;
            push_nal(&mut out, nal);
            at += self.length_size + len;
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_samples_become_annex_b() {
        // avcC: 4-byte lengths, one SPS (3 bytes), one PPS (2 bytes).
        let avcc = [
            1, 0x64, 0, 0x1F, 0xFF, 0xE1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 9,
        ];
        let annexb = AnnexB::from_avcc(&avcc).unwrap();
        assert_eq!(
            annexb.parameter_sets(),
            [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 9]
        );
        let sample = [0, 0, 0, 2, 0x65, 7, 0, 0, 0, 1, 0x06];
        assert_eq!(
            annexb.convert(&sample, true).unwrap(),
            [
                0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 9, 0, 0, 0, 1, 0x65, 7, 0, 0, 0, 1, 0x06
            ]
        );
        assert_eq!(
            annexb.convert(&sample, false).unwrap(),
            [0, 0, 0, 1, 0x65, 7, 0, 0, 0, 1, 0x06]
        );
        assert!(annexb.convert(&[0, 0, 0, 9, 1], false).is_none());
        assert!(AnnexB::from_avcc(&avcc[..10]).is_none());
    }

    #[test]
    fn hvcc_arrays_become_parameter_sets() {
        let mut hvcc = vec![1u8; 21];
        hvcc.push(0x03); // lengthSizeMinusOne 3
        hvcc.push(2); // two arrays
        hvcc.extend_from_slice(&[0x20, 0, 1, 0, 2, 0x40, 1]); // VPS
        hvcc.extend_from_slice(&[0x21, 0, 1, 0, 1, 0x42]); // SPS
        let annexb = AnnexB::from_hvcc(&hvcc).unwrap();
        assert_eq!(
            annexb.parameter_sets(),
            [0, 0, 0, 1, 0x40, 1, 0, 0, 0, 1, 0x42]
        );
    }
}
