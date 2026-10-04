use std::{
    io::{BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use rodio::{ChannelCount, DeviceSinkBuilder, Player, SampleRate, Source};

const CHUNK_SAMPLES: usize = 2048;
const QUEUED_CHUNKS: usize = 8;
const ERROR_BYTES: usize = 65536;
static PLAYBACK_ACTIVE: AtomicBool = AtomicBool::new(false);

struct SessionPermit;

impl Drop for SessionPermit {
    fn drop(&mut self) {
        PLAYBACK_ACTIVE.store(false, Ordering::Release);
    }
}

enum Control {
    Play,
    Pause,
    Stop,
}

pub enum Status {
    Playing,
    Paused,
    Ended,
    Failed(String),
}

pub struct Playback {
    control: Sender<Control>,
    status: Receiver<Status>,
    finished: bool,
    _worker: JoinHandle<()>,
}

impl Playback {
    pub fn start(path: PathBuf, seconds: u64) -> Result<Self> {
        anyhow::ensure!(
            PLAYBACK_ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "Another media preview is playing. Stop it before playing this file."
        );
        let permit = SessionPermit;
        let (control_sender, control_receiver) = mpsc::channel();
        let (status_sender, status_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("file-viewer-playback".into())
            .spawn(move || {
                let _permit = permit;
                let status = match run(&path, seconds, control_receiver, &status_sender) {
                    Ok(()) => Status::Ended,
                    Err(error) => Status::Failed(format!("{error:#}").chars().take(4096).collect()),
                };
                if let Err(error) = status_sender.send(status) {
                    log::debug!("Playback viewer closed before the final status: {error}");
                }
            })?;
        Ok(Self {
            control: control_sender,
            status: status_receiver,
            finished: false,
            _worker: worker,
        })
    }

    pub fn play(&self) -> Result<()> {
        self.control
            .send(Control::Play)
            .context("Playback has ended")
    }

    pub fn pause(&self) -> Result<()> {
        self.control
            .send(Control::Pause)
            .context("Playback has ended")
    }

    pub fn poll(&mut self) -> Option<Status> {
        match self.status.try_recv() {
            Ok(status) => {
                if matches!(status, Status::Ended | Status::Failed(_)) {
                    self.finished = true;
                }
                Some(status)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) if !self.finished => {
                self.finished = true;
                Some(Status::Ended)
            }
            Err(TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        // The worker owns all OS resources and reaps the decoder after this nonblocking stop.
        if !self.finished
            && let Err(error) = self.control.send(Control::Stop)
        {
            log::debug!("Playback already stopped: {error}");
        }
    }
}

struct Decoder(
    Child,
    Option<file_preview::decoder_budget::ProcessContainment>,
);

impl Decoder {
    fn terminate(&mut self) -> Result<ExitStatus> {
        drop(self.1.take());
        Ok(file_preview::decoder_budget::terminate(&mut self.0)?)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            log::warn!("Failed to stop the playback decoder: {error:#}");
        }
    }
}

struct PcmSource {
    receiver: Receiver<Vec<f32>>,
    chunk: Vec<f32>,
    position: usize,
    exhausted: bool,
    consumed: Arc<AtomicU64>,
}

impl Iterator for PcmSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if let Some(sample) = self.chunk.get(self.position) {
            self.position += 1;
            self.consumed.fetch_add(1, Ordering::Relaxed);
            return Some(*sample);
        }
        match self.receiver.try_recv() {
            Ok(chunk) => {
                self.chunk = chunk;
                self.position = 0;
                self.next()
            }
            // Decoder stalls must never block the audio callback or the GPUI thread.
            Err(TryRecvError::Empty) => Some(0.0),
            Err(TryRecvError::Disconnected) => {
                self.exhausted = true;
                None
            }
        }
    }
}

impl Source for PcmSource {
    fn current_span_len(&self) -> Option<usize> {
        self.exhausted.then_some(0)
    }

    fn channels(&self) -> ChannelCount {
        ChannelCount::new(2).unwrap_or(ChannelCount::MIN)
    }

    fn sample_rate(&self) -> SampleRate {
        SampleRate::new(44100).unwrap_or(SampleRate::MIN)
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

fn decode_pcm(mut reader: impl Read, sender: SyncSender<Vec<f32>>) -> Result<()> {
    let mut bytes = [0_u8; CHUNK_SAMPLES * 4];
    loop {
        let mut filled = 0;
        while filled < bytes.len() {
            let count = reader.read(&mut bytes[filled..])?;
            if count == 0 {
                break;
            }
            filled += count;
        }
        if filled == 0 {
            return Ok(());
        }
        anyhow::ensure!(
            filled % 8 == 0,
            "Decoder produced an incomplete audio frame"
        );
        let samples = bytes[..filled]
            .chunks_exact(4)
            .map(|sample| {
                let value = f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
                if value.is_finite() { value } else { 0.0 }
            })
            .collect();
        if sender.send(samples).is_err() {
            return Ok(());
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "The decoder is spawned only on the dedicated playback worker; stdio remains synchronous."
)]
fn spawn_decoder(path: &Path, seconds: u64) -> Result<Decoder> {
    let mut command = Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-nostdin",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-max_alloc",
        "67108864",
        "-protocol_whitelist",
        "file,pipe,data",
        "-ss",
    ]);
    command.arg(seconds.to_string()).arg("-i").arg(path);
    command.args([
        "-vn", "-sn", "-dn", "-map", "0:a:0", "-ac", "2", "-ar", "44100", "-f", "f32le", "pipe:1",
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    file_preview::decoder_budget::configure(&mut command);
    let mut child = command
        .spawn()
        .context("FFmpeg is required for media playback")?;
    let containment = match file_preview::decoder_budget::ProcessContainment::new(&child) {
        Ok(containment) => containment,
        Err(error) => {
            file_preview::decoder_budget::terminate(&mut child)?;
            return Err(error).context("Cannot contain the playback decoder");
        }
    };
    Ok(Decoder(child, Some(containment)))
}

fn run(
    path: &Path,
    seconds: u64,
    controls: Receiver<Control>,
    statuses: &Sender<Status>,
) -> Result<()> {
    let mut device =
        DeviceSinkBuilder::open_default_sink().context("Cannot open the audio output device")?;
    device.log_on_drop(false);
    let player = Player::connect_new(device.mixer());
    let mut decoder = spawn_decoder(path, seconds)?;
    let stdout = decoder
        .0
        .stdout
        .take()
        .context("Missing decoder audio pipe")?;
    let stderr = decoder
        .0
        .stderr
        .take()
        .context("Missing decoder error pipe")?;
    let (sample_sender, sample_receiver) = mpsc::sync_channel(QUEUED_CHUNKS);
    let consumed = Arc::new(AtomicU64::new(0));
    player.append(PcmSource {
        receiver: sample_receiver,
        chunk: Vec::new(),
        position: 0,
        exhausted: false,
        consumed: consumed.clone(),
    });
    let audio_reader = thread::Builder::new()
        .name("file-viewer-pcm".into())
        .spawn(move || decode_pcm(BufReader::new(stdout), sample_sender))?;
    let error_reader = thread::Builder::new()
        .name("file-viewer-audio-errors".into())
        .spawn(move || {
            let mut error = Vec::new();
            stderr
                .take(ERROR_BYTES as u64 + 1)
                .read_to_end(&mut error)?;
            Ok::<_, std::io::Error>(error)
        })?;
    let mut stopped = false;
    let result = (|| -> Result<()> {
        statuses.send(Status::Playing)?;
        let mut last_progress = Instant::now();
        let mut last_consumed = 0;
        loop {
            let process_status = decoder.0.try_wait()?;
            if process_status.is_none()
                && file_preview::decoder_budget::memory_exceeded(&decoder.0)?
            {
                anyhow::bail!("Playback decoder exceeded the 256 MiB resident memory budget");
            }
            match controls.recv_timeout(Duration::from_millis(20)) {
                Ok(Control::Play) => {
                    player.play();
                    last_progress = Instant::now();
                    statuses.send(Status::Playing)?;
                }
                Ok(Control::Pause) => {
                    player.pause();
                    statuses.send(Status::Paused)?;
                }
                Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    stopped = true;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let current_consumed = consumed.load(Ordering::Relaxed);
            if current_consumed != last_consumed {
                last_consumed = current_consumed;
                last_progress = Instant::now();
            }
            if !player.is_paused() && last_progress.elapsed() > Duration::from_secs(10) {
                anyhow::bail!("Playback decoder stopped producing audio for 10 seconds");
            }
            if let Some(status) = process_status
                && (!status.success() || player.empty())
            {
                break;
            }
        }
        Ok(())
    })();
    player.stop();
    drop(player);
    drop(device);
    let status = decoder.terminate()?;
    let audio_result = audio_reader
        .join()
        .map_err(|_| anyhow::anyhow!("Audio reader failed"))?;
    let error = error_reader
        .join()
        .map_err(|_| anyhow::anyhow!("Audio error reader failed"))??;
    result?;
    if !stopped {
        audio_result?;
        anyhow::ensure!(
            status.success(),
            "Playback decoder failed: {}",
            String::from_utf8_lossy(&error)
                .chars()
                .take(4096)
                .collect::<String>()
        );
    } else if let Err(error) = audio_result {
        log::debug!("Audio reader stopped with playback: {error:#}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_source_never_blocks_waiting_for_decoder() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut source = PcmSource {
            receiver,
            chunk: Vec::new(),
            position: 0,
            exhausted: false,
            consumed: Arc::new(AtomicU64::new(0)),
        };
        assert_eq!(source.next(), Some(0.0));
        sender.send(vec![0.25, -0.25]).expect("PCM test channel");
        assert_eq!(source.next(), Some(0.25));
        assert_eq!(source.next(), Some(-0.25));
        drop(sender);
        assert_eq!(source.next(), None);
        assert_eq!(source.current_span_len(), Some(0));
    }

    #[test]
    fn pcm_decoder_rejects_incomplete_stereo_frames() {
        let (sender, _) = mpsc::sync_channel(1);
        assert!(decode_pcm([1_u8, 2, 3, 4].as_slice(), sender).is_err());
    }

    #[test]
    fn decoder_streams_audio_and_is_reaped_when_stopped() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        let frames = 44100_u32;
        let data_bytes = frames * 4;
        let mut bytes = Vec::with_capacity(44 + data_bytes as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(data_bytes + 36).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&44100_u32.to_le_bytes());
        bytes.extend_from_slice(&176400_u32.to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        bytes.resize(44 + data_bytes as usize, 0);
        std::fs::write(file.path(), bytes)?;
        let mut decoder = match spawn_decoder(file.path(), 0) {
            Ok(decoder) => decoder,
            Err(error)
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                }) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let mut stdout = decoder.0.stdout.take().context("Test decoder stdout")?;
        let mut sample_frame = [0_u8; 8];
        stdout.read_exact(&mut sample_frame)?;
        assert_eq!(sample_frame, [0; 8]);
        assert!(!file_preview::decoder_budget::memory_exceeded(&decoder.0)?);
        decoder.terminate()?;
        assert!(decoder.0.try_wait()?.is_some());
        Ok(())
    }
}
