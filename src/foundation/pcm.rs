//! PCM decoding for the recognition path — turn the audio bytes a channel
//! already holds into the **16 kHz mono signed-16-bit** samples the voiceprint
//! capability embeds.
//!
//! This is deliberately narrow. The whole system speaks one audio contract: the
//! SPA records 16 kHz mono 16-bit PCM (wrapped in a canonical 44-byte WAV header
//! for a posted clip, raw for the live mic), and STT, the persisted minute files,
//! and voiceprint all assume it. So we don't pull in a general WAV/decoder — we
//! strip the known header (or take raw PCM as-is) and reinterpret little-endian
//! 16-bit samples. Anything that isn't that contract is an explicit error rather
//! than silently mis-decoded garbage, mirroring [`crate::foundation::vendors::volcengine_stt`].

use anyhow::bail;

/// Canonical RIFF/WAVE header length the SPA emits (16 kHz mono 16-bit PCM).
const WAV_HEADER_BYTES: usize = 44;

/// Decode `bytes` of the given `mime` into 16 kHz mono signed-16-bit-LE samples.
///
/// `audio/wav` (and its aliases) is assumed to be the SPA's canonical 44-byte
/// header followed by raw PCM; `audio/pcm` / `application/octet-stream` is taken
/// as raw PCM already. Any other mime, or a body too short / not RIFF, is an
/// error — the caller skips voiceprint rather than embedding noise.
pub fn to_i16_16k_mono(bytes: &[u8], mime: &str) -> anyhow::Result<Vec<i16>> {
    let kind = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let pcm: &[u8] = match kind.as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => {
            if bytes.len() < WAV_HEADER_BYTES + 2 || &bytes[0..4] != b"RIFF" {
                bail!("audio body is not a canonical 16 kHz mono 16-bit WAV");
            }
            &bytes[WAV_HEADER_BYTES..]
        }
        "audio/pcm" | "application/octet-stream" => bytes,
        other => bail!("unsupported audio mime {other:?} for voiceprint (need wav or pcm)"),
    };
    Ok(le_i16(pcm))
}

/// Reinterpret a little-endian 16-bit PCM byte slice as samples, dropping a
/// trailing odd byte (a torn sample) rather than failing.
pub fn le_i16(pcm: &[u8]) -> Vec<i16> {
    pcm.chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(pcm: &[u8]) -> Vec<u8> {
        let mut w = vec![0u8; WAV_HEADER_BYTES];
        w[0..4].copy_from_slice(b"RIFF");
        w.extend_from_slice(pcm);
        w
    }

    #[test]
    fn wav_strips_header_and_decodes_samples() {
        let pcm = [1i16, -2, 300, -4000];
        let body: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        let got = to_i16_16k_mono(&wav(&body), "audio/wav").unwrap();
        assert_eq!(got, pcm);
    }

    #[test]
    fn raw_pcm_is_taken_as_is() {
        let pcm = [7i16, 8, 9];
        let body: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(to_i16_16k_mono(&body, "audio/pcm").unwrap(), pcm);
    }

    #[test]
    fn odd_trailing_byte_is_dropped() {
        assert_eq!(le_i16(&[0x01, 0x00, 0x7f]), vec![1i16]);
    }

    #[test]
    fn non_riff_wav_is_rejected() {
        assert!(to_i16_16k_mono(&[0u8; 50], "audio/wav").is_err());
    }

    #[test]
    fn unknown_mime_is_rejected() {
        assert!(to_i16_16k_mono(&[0u8; 64], "audio/mpeg").is_err());
    }
}
