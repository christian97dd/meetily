//! System audio on Linux: records the default output's monitor with `parec`, which talks to
//! PulseAudio and to PipeWire (through pipewire-pulse) alike, so whatever calls play is captured.

use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, Result};

pub const SAMPLE_RATE: u32 = 48_000;

/// Starts recording the monitor of the default output as mono 32-bit float samples on stdout.
pub fn spawn() -> Result<Child> {
    Command::new("parec")
        .args([
            "--device=@DEFAULT_MONITOR@",
            "--format=float32le",
            &format!("--rate={}", SAMPLE_RATE),
            "--channels=1",
            "--latency-msec=40",
            "--raw",
            "--client-name=meetily",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("parec unavailable ({}): install pulseaudio-utils to capture system audio", e))
}

/// Decodes little-endian f32 samples. `pending` keeps the bytes of a sample split across reads.
pub fn decode(pending: &mut Vec<u8>, bytes: &[u8]) -> Vec<f32> {
    pending.extend_from_slice(bytes);
    let whole = pending.len() / 4 * 4;
    let samples = pending[..whole]
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    pending.drain(..whole);
    samples
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn samples_split_across_reads_are_kept_whole() {
        let bytes: Vec<u8> = [0.5f32, -0.25].iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut pending = Vec::new();
        assert_eq!(decode(&mut pending, &bytes[..6]), vec![0.5]);
        assert_eq!(decode(&mut pending, &bytes[6..]), vec![-0.25]);
        assert!(pending.is_empty());
    }
}
