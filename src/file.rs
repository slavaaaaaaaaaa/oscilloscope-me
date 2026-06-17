//! MP3 file playback for oscilloscope music (L = X, R = Y).

use crate::app::AppEvent;
use crate::audio::StereoResampler;
use crate::demod::peak_dbfs;
use crate::display::decimate_scope_pair;
use crate::sdr::SharedRing;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

const CHUNK_FRAMES: usize = 2_048;

pub struct DecodedTrack {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    pub sample_rate: u32,
}

pub fn decode_mp3(path: &Path) -> Result<DecodedTrack, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Unsupported or corrupt audio file: {e}"))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "No audio track found".to_string())?;

    let track_id = track.id;
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| "Missing sample rate in file".to_string())?;
    let channels = track
        .codec_params
        .channels
        .ok_or_else(|| "Missing channel layout in file".to_string())?
        .count();

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Cannot create decoder: {e}"))?;

    let mut left = Vec::new();
    let mut right = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(Error::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("Decode error: {e}")),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = decoder
            .decode(&packet)
            .map_err(|e| format!("Decode error: {e}"))?;

        let mut sample_buf =
            SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        sample_buf.copy_interleaved_ref(decoded);

        let frames = sample_buf.samples().chunks(channels);
        for frame in frames {
            let l = frame.first().copied().unwrap_or(0.0);
            let r = if channels >= 2 {
                frame[1]
            } else {
                l
            };
            left.push(l);
            right.push(r);
        }
    }

    if left.is_empty() {
        return Err("File contains no audio samples".to_string());
    }

    Ok(DecodedTrack {
        left,
        right,
        sample_rate,
    })
}

enum FileCommand {
    SetLoop(bool),
    SetPaused(bool),
    SeekSeconds(f64),
    Restart,
}

pub struct FileHandle {
    stop: Arc<AtomicBool>,
    cmd_tx: crossbeam_channel::Sender<FileCommand>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FileHandle {
    pub fn set_loop(&self, loop_playback: bool) {
        let _ = self.cmd_tx.send(FileCommand::SetLoop(loop_playback));
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.cmd_tx.send(FileCommand::SetPaused(paused));
    }

    pub fn seek_seconds(&self, delta: f64) {
        let _ = self.cmd_tx.send(FileCommand::SeekSeconds(delta));
    }

    pub fn restart(&self) {
        let _ = self.cmd_tx.send(FileCommand::Restart);
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn start_playback(
    path: PathBuf,
    track: DecodedTrack,
    device_rate: u32,
    loop_playback: bool,
    ring: SharedRing,
    event_tx: crossbeam_channel::Sender<AppEvent>,
    app_shutdown: Arc<AtomicBool>,
) -> Result<FileHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let stop_thread = stop.clone();
    let app_shutdown_thread = app_shutdown.clone();
    let path_display = path.clone();
    let source_rate = track.sample_rate;

    let thread = thread::Builder::new()
        .name("file-playback".into())
        .spawn(move || {
            playback_loop(
                track.left,
                track.right,
                source_rate,
                device_rate,
                loop_playback,
                ring,
                event_tx,
                cmd_rx,
                stop_thread,
                app_shutdown_thread,
                path_display,
            );
        })
        .map_err(|e| e.to_string())?;

    Ok(FileHandle {
        stop,
        cmd_tx,
        thread: Some(thread),
    })
}

fn playback_loop(
    left: Vec<f32>,
    right: Vec<f32>,
    source_rate: u32,
    device_rate: u32,
    mut loop_playback: bool,
    ring: SharedRing,
    event_tx: crossbeam_channel::Sender<AppEvent>,
    cmd_rx: crossbeam_channel::Receiver<FileCommand>,
    stop: Arc<AtomicBool>,
    app_shutdown: Arc<AtomicBool>,
    path: PathBuf,
) {
    let mut resampler = StereoResampler::new(source_rate, device_rate);

    let _ = event_tx.send(AppEvent::FilePlaying {
        path: path.display().to_string(),
        sample_rate: device_rate,
        loop_playback,
    });

    let mut pos = 0usize;
    let total = left.len();
    let mut paused = false;

    while !stop.load(Ordering::Relaxed) && !app_shutdown.load(Ordering::Relaxed) {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                FileCommand::SetLoop(v) => loop_playback = v,
                FileCommand::SetPaused(v) => paused = v,
                FileCommand::SeekSeconds(delta) => {
                    let current = pos as f64 / source_rate as f64;
                    let dur = total as f64 / source_rate as f64;
                    let new_pos = if loop_playback {
                        (current + delta).rem_euclid(dur.max(1e-9))
                    } else {
                        (current + delta).clamp(0.0, dur)
                    };
                    pos = (new_pos * source_rate as f64).round() as usize;
                    pos = pos.min(total.saturating_sub(1));
                    resampler.reset(source_rate, device_rate);
                    if let Ok(mut r) = ring.lock() {
                        r.clear();
                    }
                    paused = false;
                }
                FileCommand::Restart => {
                    pos = 0;
                    resampler.reset(source_rate, device_rate);
                    if let Ok(mut r) = ring.lock() {
                        r.clear();
                    }
                    paused = false;
                }
            }
        }

        if paused {
            thread::sleep(Duration::from_millis(16));
            continue;
        }

        let end = (pos + CHUNK_FRAMES).min(total);
        let chunk_l = &left[pos..end];
        let chunk_r = &right[pos..end];
        let input_frames = chunk_l.len();

        let (audio_l, audio_r) = resampler.process(chunk_l, chunk_r);

        if !audio_l.is_empty() {
            let mut r = ring.lock().unwrap();
            r.push_frame(&audio_l, &audio_r);
        }

        let (scope_l, scope_r) = decimate_scope_pair(chunk_l, chunk_r, 192);
        let _ = event_tx.send(AppEvent::StereoData {
            scope_left: scope_l,
            scope_right: scope_r,
            peak_l: peak_dbfs(chunk_l),
            peak_r: peak_dbfs(chunk_r),
        });

        let chunk_duration =
            Duration::from_secs_f64(input_frames as f64 / source_rate as f64);
        let deadline = Instant::now() + chunk_duration;
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) || app_shutdown.load(Ordering::Relaxed) {
                break;
            }
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    FileCommand::SetLoop(v) => loop_playback = v,
                    FileCommand::SetPaused(v) => paused = v,
                    FileCommand::SeekSeconds(delta) => {
                        let current = pos as f64 / source_rate as f64;
                        let dur = total as f64 / source_rate as f64;
                        let new_pos = if loop_playback {
                            (current + delta).rem_euclid(dur.max(1e-9))
                        } else {
                            (current + delta).clamp(0.0, dur)
                        };
                        pos = (new_pos * source_rate as f64).round() as usize;
                        pos = pos.min(total.saturating_sub(1));
                        resampler.reset(source_rate, device_rate);
                        if let Ok(mut r) = ring.lock() {
                            r.clear();
                        }
                        paused = false;
                    }
                    FileCommand::Restart => {
                        pos = 0;
                        resampler.reset(source_rate, device_rate);
                        if let Ok(mut r) = ring.lock() {
                            r.clear();
                        }
                        paused = false;
                    }
                }
            }
            thread::sleep(Duration::from_millis(1));
        }

        pos = end;
        if pos >= total {
            let (tail_l, tail_r) = resampler.flush();
            if !tail_l.is_empty() {
                let mut r = ring.lock().unwrap();
                r.push_frame(&tail_l, &tail_r);
            }

            if loop_playback {
                pos = 0;
                resampler.reset(source_rate, device_rate);
                if let Ok(mut r) = ring.lock() {
                    r.clear();
                }
            } else {
                let _ = event_tx.send(AppEvent::FileFinished);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn interleaved_stereo_split() {
        let samples = [1.0f32, 2.0, 3.0, 4.0];
        let mut left = Vec::new();
        let mut right = Vec::new();
        for frame in samples.chunks(2) {
            left.push(frame[0]);
            right.push(frame[1]);
        }
        assert_eq!(left, vec![1.0, 3.0]);
        assert_eq!(right, vec![2.0, 4.0]);
    }
}
