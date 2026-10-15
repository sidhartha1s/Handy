use anyhow::Result;
use hound::{WavReader, WavSpec, WavWriter};
use log::debug;
use std::path::Path;

/// Read a WAV file and return normalised f32 samples.
pub fn read_wav_samples<P: AsRef<Path>>(file_path: P) -> Result<Vec<f32>> {
    let reader = WavReader::open(file_path.as_ref())?;
    let samples = reader
        .into_samples::<i16>()
        .map(|s| s.map(|v| v as f32 / i16::MAX as f32))
        .collect::<Result<Vec<f32>, _>>()?;
    Ok(samples)
}

/// Verify a WAV file by reading it back and checking the sample count.
pub fn verify_wav_file<P: AsRef<Path>>(file_path: P, expected_samples: usize) -> Result<()> {
    let reader = WavReader::open(file_path.as_ref())?;
    let actual_samples = reader.len() as usize;
    if actual_samples != expected_samples {
        anyhow::bail!(
            "WAV sample count mismatch: expected {}, got {}",
            expected_samples,
            actual_samples
        );
    }
    Ok(())
}

/// Save audio samples as a WAV file
pub fn save_wav_file<P: AsRef<Path>>(file_path: P, samples: &[f32]) -> Result<()> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = WavWriter::create(file_path.as_ref(), spec)?;

    // Convert f32 samples to i16 for WAV
    for sample in samples {
        let sample_i16 = (sample * i16::MAX as f32) as i16;
        writer.write_sample(sample_i16)?;
    }

    writer.finalize()?;
    debug!("Saved WAV file: {:?}", file_path.as_ref());
    Ok(())
}

/// Where to cut `samples` so no piece is longer than `max_len`: each cut is the
/// quietest 30 ms frame (16 kHz) in the last `search_len` samples before the
/// limit, so cuts land in pauses between words rather than inside them. A cut
/// never leaves a last piece under 1 s: Moonshine can make up text on scraps.
pub fn quiet_cut_points(samples: &[f32], max_len: usize, search_len: usize) -> Vec<usize> {
    const FRAME: usize = 480;
    const MIN_LAST: usize = 16_000;
    debug_assert!(
        max_len > FRAME,
        "pieces must be longer than one 30 ms frame"
    );
    let mut cuts = Vec::new();
    let mut start = 0;
    while samples.len() - start > max_len {
        let from = start + max_len - search_len.min(max_len);
        let to = (start + max_len).min(samples.len().saturating_sub(MIN_LAST));
        let quietest = (from..to.saturating_sub(FRAME))
            .step_by(FRAME)
            .map(|at| (samples[at..at + FRAME].iter().map(|s| s * s).sum::<f32>(), at))
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map_or(from, |(_, at)| at);
        start = quietest + FRAME / 2;
        cuts.push(start);
    }
    cuts
}

#[cfg(test)]
mod tests {
    use super::quiet_cut_points;

    #[test]
    fn cuts_long_audio_in_its_pauses() {
        // 50 s of tone with silent gaps at 17 s and 36 s.
        let mut samples = vec![0.5f32; 50 * 16_000];
        for gap in [17 * 16_000, 36 * 16_000] {
            samples[gap..gap + 4_800].fill(0.0);
        }
        let cuts = quiet_cut_points(&samples, 20 * 16_000, 5 * 16_000);
        assert_eq!(cuts.len(), 2);
        for (cut, gap) in cuts.iter().zip([17 * 16_000, 36 * 16_000]) {
            assert!(
                (gap..gap + 4_800).contains(cut),
                "cut {cut} missed the gap at {gap}"
            );
        }
        assert!(quiet_cut_points(&samples[..20 * 16_000], 20 * 16_000, 5 * 16_000).is_empty());
    }

    #[test]
    fn last_piece_is_never_a_scrap() {
        // 20.05 s with its only silence right at the 20 s limit: cutting there
        // would leave a 0.05 s last piece.
        let mut samples = vec![0.5f32; 20 * 16_000 + 800];
        samples[20 * 16_000 - 960..20 * 16_000].fill(0.0);
        let cuts = quiet_cut_points(&samples, 20 * 16_000, 5 * 16_000);
        assert_eq!(cuts.len(), 1);
        assert!(samples.len() - cuts[0] >= 16_000, "last piece {} samples", samples.len() - cuts[0]);
    }
}
