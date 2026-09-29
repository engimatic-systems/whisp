use anyhow::{Context, Result, bail};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub const MAX_SECONDS: u64 = 10 * 60;

pub struct Recorder {
    child: Option<Child>,
    pub started: Instant,
    path: PathBuf,
    peak: Arc<AtomicU32>,
    meter_reader: Option<thread::JoinHandle<()>>,
}

impl Recorder {
    pub fn start(path: PathBuf) -> Result<Self> {
        Self::start_input(path, &["-f", "pulse", "-i", "default"], MAX_SECONDS)
    }

    fn start_input(path: PathBuf, input: &[&str], seconds: u64) -> Result<Self> {
        // Never truncate a recording waiting for retry/recovery.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .context("Cannot create recording; retry or discard the previous recording first")?;
        let mut command = Command::new("ffmpeg");
        command
            .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
            .args(input)
            .args([
                "-map",
                "0:a",
                "-ac",
                "1",
                "-ar",
                "16000",
                "-c:a",
                "pcm_s16le",
                "-t",
                &seconds.to_string(),
                "-flush_packets",
                "1",
                "-y",
            ])
            .arg(&path)
            // A second output from the same microphone stream feeds the meter.
            .args([
                "-map",
                "0:a",
                "-ac",
                "1",
                "-ar",
                "16000",
                "-t",
                &seconds.to_string(),
                "-f",
                "s16le",
                "-flush_packets",
                "1",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = fs::remove_file(&path);
                return Err(e)
                    .context("Could not start ffmpeg. Install ffmpeg with PulseAudio support.");
            }
        };
        let mut output = child.stdout.take().expect("piped audio output");
        let peak = Arc::new(AtomicU32::new(0));
        let meter_peak = Arc::clone(&peak);
        let meter_reader = thread::spawn(move || {
            let mut bytes = [0; 3200]; // 100 ms of mono 16 kHz PCM.
            let mut pending = None;
            while let Ok(count) = output.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                let mut peak = 0;
                for &byte in &bytes[..count] {
                    if let Some(low) = pending.take() {
                        peak = peak.max(i16::from_le_bytes([low, byte]).unsigned_abs() as u32);
                    } else {
                        pending = Some(byte);
                    }
                }
                meter_peak.fetch_max(peak, Ordering::Relaxed);
            }
        });
        Ok(Self {
            child: Some(child),
            started: Instant::now(),
            path,
            peak,
            meter_reader: Some(meter_reader),
        })
    }

    /// Peak since the last UI update, mapped from -60..0 dBFS to 0..1.
    pub fn level(&self) -> f64 {
        let peak = self.peak.swap(0, Ordering::Relaxed) as f64 / 32768.0;
        if peak == 0.0 {
            0.0
        } else {
            ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
        }
    }

    pub fn finished(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|c| !matches!(c.try_wait(), Ok(None)))
    }

    // Run on the worker thread: SIGINT lets ffmpeg finalize the WAV header.
    pub fn stop(mut self) -> Result<PathBuf> {
        let child = self.child.as_mut().context("Recorder already stopped")?;
        let interrupted = child.try_wait()?.is_none();
        if interrupted {
            // SAFETY: the PID belongs to the unreaped child owned by this Recorder.
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGINT);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while child.try_wait()?.is_none() {
                if Instant::now() >= deadline {
                    child.kill()?;
                    child.wait()?;
                    bail!("The recorder did not stop cleanly. Audio remains saved locally.");
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        let output = self.child.take().unwrap().wait_with_output()?;
        if !output.status.success() && !(interrupted && output.status.code() == Some(255)) {
            let error: String = String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(300)
                .collect();
            bail!("Microphone recording failed: {}", error.trim());
        }
        if fs::metadata(&self.path)?.len() < 1024 {
            bail!("No usable audio was captured. Check the default PipeWire microphone.");
        }
        Ok(self.path.clone())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(reader) = self.meter_reader.take() {
            let _ = reader.join();
        }
    }
}

/// Inspect the finalized WAV's actual PCM data, rather than wall-clock time
/// (opening the microphone can take a substantial part of a short recording).
pub fn discard_short_recording(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut header = [0; 12];
    file.read_exact(&mut header)?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        bail!("Invalid saved WAV recording");
    }
    let length = file.metadata()?.len();
    let mut bytes_per_second = None;
    while file.stream_position()? + 8 <= length {
        let mut chunk = [0; 8];
        file.read_exact(&mut chunk)?;
        let size = u32::from_le_bytes(chunk[4..].try_into().unwrap()) as u64;
        let begin = file.stream_position()?;
        if begin + size > length {
            bail!("Incomplete saved WAV recording");
        }
        if &chunk[..4] == b"fmt " && size >= 16 {
            let mut fmt = [0; 16];
            file.read_exact(&mut fmt)?;
            if u16::from_le_bytes(fmt[..2].try_into().unwrap()) != 1 {
                bail!("Expected PCM recording");
            }
            bytes_per_second = Some(u32::from_le_bytes(fmt[8..12].try_into().unwrap()) as u64);
        }
        if &chunk[..4] == b"data" {
            let rate = bytes_per_second
                .filter(|&rate| rate > 0)
                .context("Missing WAV audio format")?;
            if size < rate {
                drop(file);
                fs::remove_file(path)?;
                return Ok(true);
            }
            return Ok(false);
        }
        file.seek(SeekFrom::Start(begin + size + size % 2))?;
    }
    bail!("Missing WAV audio data")
}

#[derive(Clone, Copy)]
pub enum Sound {
    Start,
    Stop,
    Ready,
    Error,
}

/// Short, quiet local feedback through PipeWire's PulseAudio interface.
/// Playback failures never interrupt dictation.
pub fn ping(sound: Sound) {
    thread::spawn(move || {
        use std::io::Write;
        let Ok(mut child) = Command::new("paplay")
            .args([
                "--raw",
                "--format=s16le",
                "--rate=16000",
                "--channels=1",
                "--client-name=Whisp",
                "--stream-name=Recording feedback",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        let double = matches!(sound, Sound::Ready | Sound::Error);
        let note_samples = if matches!(sound, Sound::Error) {
            1920
        } else {
            1280
        };
        let samples = if double { note_samples * 2 } else { 1120 };
        // Ready rises; errors descend in a lower register.
        let frequency = match sound {
            Sound::Start => 1046.5,
            Sound::Stop => 698.5,
            Sound::Ready => 1318.5,
            Sound::Error => 659.25,
        };
        let mut pcm = Vec::with_capacity(samples * 2);
        for i in 0..samples {
            let (phase, length, pitch) = if double {
                let second = if matches!(sound, Sound::Error) {
                    440.0
                } else {
                    1760.0
                };
                (
                    i % note_samples,
                    note_samples,
                    if i < note_samples { frequency } else { second },
                )
            } else {
                (i, samples, frequency)
            };
            let envelope = (std::f64::consts::PI * phase as f64 / length as f64)
                .sin()
                .powi(2);
            let sample = ((2.0 * std::f64::consts::PI * pitch * phase as f64 / 16000.0).sin()
                * envelope
                * 1800.0) as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        if let Some(mut input) = child.stdin.take() {
            let _ = input.write_all(&pcm);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discard_uses_audio_duration_and_preserves_invalid_or_longer_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audio.wav");
        for (samples, expected) in [(15999usize, true), (16000, false), (48000, false)] {
            let mut wav = Vec::new();
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + samples as u32 * 2).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&16000u32.to_le_bytes());
            wav.extend_from_slice(&32000u32.to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&(samples as u32 * 2).to_le_bytes());
            wav.resize(44 + samples * 2, 0);
            fs::write(&path, wav).unwrap();
            assert_eq!(discard_short_recording(&path).unwrap(), expected);
            assert_eq!(path.exists(), !expected);
        }
        fs::write(&path, b"incomplete audio").unwrap();
        assert!(discard_short_recording(&path).is_err());
        assert!(path.exists());
    }

    #[test]
    fn pending_audio_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.wav");
        fs::write(&path, b"precious recording").unwrap();
        assert!(Recorder::start(path.clone()).is_err());
        assert_eq!(fs::read(path).unwrap(), b"precious recording");
    }

    #[test]
    fn tone_feeds_meter_and_stopping_finalizes_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        let recorder = Recorder::start_input(
            path.clone(),
            &[
                "-re",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=16000",
            ],
            5,
        )
        .unwrap();
        let mut level = 0.0;
        while level == 0.0 && recorder.started.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(100));
            level = recorder.level();
        }
        assert!(
            level > 0.5 && level < 0.9,
            "Tone should register on the dB meter: {level}"
        );
        recorder.stop().unwrap();
        let wav = fs::read(path).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize,
            wav.len() - 8
        );
    }

    #[test]
    fn both_outputs_stop_at_capture_limit_without_a_stop_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        let mut recorder = Recorder::start_input(
            path.clone(),
            &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=16000"],
            1,
        )
        .unwrap();
        while !recorder.finished() && recorder.started.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            recorder.finished(),
            "Both FFmpeg outputs must stop at the limit"
        );
        recorder.stop().unwrap();
        let size = fs::metadata(path).unwrap().len();
        assert!((32_000..33_000).contains(&size));
    }
}
