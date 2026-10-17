//! Microphone capture delivering 16 kHz mono float samples.
//!
//! Native capture (`cpal`, CoreAudio) is compiled in with the `voice` feature on macOS. Every
//! platform can also read an installed recorder program that writes raw signed 16-bit
//! little-endian 16 kHz mono PCM to stdout; the first installed one is used.

use super::{
    CaptureChoice, SttConfig,
    process::{self, CommandSpec},
    wav,
};
use anyhow::{Result, bail};
use std::{
    io::Read,
    path::Path,
    process::{Child, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

/// One message from a running capture: samples, or a terminal error.
pub type Chunk = Result<Vec<f32>, String>;

/// A running microphone capture. Dropping it stops the capture.
pub struct Capture {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<thread::JoinHandle<()>>,
    source: String,
}

impl Capture {
    /// Start capturing; chunks of 16 kHz mono samples arrive on the returned receiver. `cancel`
    /// aborts a native start that is still waiting for the microphone.
    pub fn start(
        config: &SttConfig,
        cancel: &AtomicBool,
    ) -> Result<(Capture, mpsc::Receiver<Chunk>)> {
        match config.capture {
            CaptureChoice::Native => native::start(cancel),
            CaptureChoice::Command => Self::start_command(config),
            CaptureChoice::Auto => {
                if config.capture_command.is_empty() && native::AVAILABLE {
                    match native::start(cancel) {
                        Ok(started) => return Ok(started),
                        Err(native_error) => {
                            return Self::start_command(config).map_err(|command_error| {
                                anyhow::anyhow!(
                                    "native capture failed ({native_error:#}); {command_error:#}"
                                )
                            });
                        }
                    }
                }
                Self::start_command(config)
            }
        }
    }

    /// Human-readable name of the capture source, for status messages.
    pub fn source(&self) -> &str {
        &self.source
    }

    fn start_command(config: &SttConfig) -> Result<(Capture, mpsc::Receiver<Chunk>)> {
        let spec = recorder(config)?;
        let mut command = spec.command();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::platform::process::spawn_group(&mut command)
            .map_err(|e| anyhow::anyhow!("starting recorder {}: {e}", spec.program.display()))?;
        // Recorder diagnostics go to a bounded tail so a failure can report why it stopped.
        const STDERR_TAIL: usize = 2048;
        let stderr_tail = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (stderr_done_tx, stderr_done) = mpsc::channel::<()>();
        let mut stderr = child.stderr.take().expect("piped stderr");
        let tail_sink = stderr_tail.clone();
        thread::spawn(move || {
            let mut chunk = [0u8; 1024];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut tail) = tail_sink.lock() {
                            tail.extend_from_slice(&chunk[..n]);
                            let excess = tail.len().saturating_sub(STDERR_TAIL);
                            tail.drain(..excess);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = stderr_done_tx.send(());
        });
        let mut stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(Some(child)));
        let thread_stop = stop.clone();
        let thread_child = child.clone();
        let source = spec.program.display().to_string();
        let name = source.clone();
        let thread = thread::spawn(move || {
            // 100 ms of 16 kHz 16-bit audio per read.
            let mut buf = vec![0u8; 3200];
            let mut carry: Option<u8> = None;
            let mut samples = Vec::with_capacity(1600);
            let mut received = false;
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        received = true;
                        samples.clear();
                        let bytes = &buf[..n];
                        let rest = match carry.take() {
                            Some(first) => {
                                let mut joined = Vec::with_capacity(n + 1);
                                joined.push(first);
                                joined.extend_from_slice(bytes);
                                wav::pcm16le_into(&joined, &mut samples)
                            }
                            None => wav::pcm16le_into(bytes, &mut samples),
                        };
                        carry = rest;
                        if tx.send(Ok(samples.clone())).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let _ = tx.send(Err(format!("reading from {name}: {error}")));
                        break;
                    }
                }
            }
            if !thread_stop.load(Ordering::SeqCst) {
                // Give stderr a moment to drain so its tail can explain the failure.
                let _ = stderr_done.recv_timeout(Duration::from_millis(250));
                let status = thread_child
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.as_mut().and_then(|c| c.try_wait().ok().flatten()));
                let detail = status.map(|s| format!(" ({s})")).unwrap_or_default();
                let hint = if received {
                    ""
                } else {
                    "; check microphone permission and the input device"
                };
                let tail = stderr_tail
                    .lock()
                    .map(|t| String::from_utf8_lossy(&t).trim().to_owned())
                    .unwrap_or_default();
                let tail_note = if tail.is_empty() {
                    String::new()
                } else {
                    format!(": {tail}")
                };
                let _ = tx.send(Err(format!(
                    "recorder {name} stopped{detail}{hint}{tail_note}"
                )));
            }
        });
        Ok((
            Capture {
                stop,
                child,
                thread: Some(thread),
                source,
            },
            rx,
        ))
    }

    /// Stop capturing. Samples already sent stay in the receiver.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = self.child.lock()
            && let Some(mut child) = slot.take()
        {
            process::kill(&mut child);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The recorder command: the configured one, or the first installed candidate.
fn recorder(config: &SttConfig) -> Result<CommandSpec> {
    if let Some((program, args)) = config.capture_command.split_first() {
        let resolved = process::which(Path::new(program))
            .ok_or_else(|| anyhow::anyhow!("capture command {program:?} is not installed"))?;
        let mut spec = CommandSpec::new(resolved);
        for arg in args {
            spec = spec.arg(arg);
        }
        return Ok(spec);
    }
    for candidate in recorder_candidates(std::env::consts::OS) {
        if let Some(resolved) = process::which(&candidate.program) {
            return Ok(CommandSpec {
                program: resolved,
                args: candidate.args,
            });
        }
    }
    let hint = if !native::AVAILABLE && cfg!(target_os = "macos") {
        " (this build has no native capture: rebuild with `--features voice`)"
    } else {
        ""
    };
    bail!(
        "no microphone recorder found{hint}; install one of {} or set voice.stt.capture_command",
        recorder_candidates(std::env::consts::OS)
            .iter()
            .map(|c| c.program.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Recorder programs tried in order on `os`, each writing raw s16le 16 kHz mono PCM to stdout.
pub fn recorder_candidates(os: &str) -> Vec<CommandSpec> {
    let sox = CommandSpec::new("sox")
        .arg("-q")
        .arg("-d")
        .arg("-t")
        .arg("raw")
        .arg("-r")
        .arg("16000")
        .arg("-c")
        .arg("1")
        .arg("-b")
        .arg("16")
        .arg("-e")
        .arg("signed-integer")
        .arg("-L")
        .arg("-");
    let ffmpeg = |format: &str, input: &str| {
        CommandSpec::new("ffmpeg")
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-f")
            .arg(format)
            .arg("-i")
            .arg(input)
            .arg("-ac")
            .arg("1")
            .arg("-ar")
            .arg("16000")
            .arg("-f")
            .arg("s16le")
            .arg("-")
    };
    match os {
        "macos" => vec![sox, ffmpeg("avfoundation", ":0")],
        _ => vec![
            CommandSpec::new("arecord")
                .arg("-q")
                .arg("-t")
                .arg("raw")
                .arg("-f")
                .arg("S16_LE")
                .arg("-r")
                .arg("16000")
                .arg("-c")
                .arg("1"),
            CommandSpec::new("parecord")
                .arg("--raw")
                .arg("--rate=16000")
                .arg("--channels=1")
                .arg("--format=s16le"),
            sox,
            ffmpeg("pulse", "default"),
        ],
    }
}

#[cfg(all(feature = "voice", target_os = "macos"))]
mod native {
    use super::{Capture, Chunk};
    use crate::voice::wav::{self, Resampler, SAMPLE_RATE};
    use anyhow::{Context, Result};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::time::Duration;

    pub const AVAILABLE: bool = true;

    struct Converter {
        channels: usize,
        resampler: Resampler,
        mono: Vec<f32>,
        tx: mpsc::Sender<Chunk>,
    }

    impl Converter {
        fn push(&mut self, interleaved: &[f32]) {
            self.mono.clear();
            wav::downmix_into(interleaved, self.channels, &mut self.mono);
            let mut out = Vec::with_capacity(self.mono.len());
            self.resampler.process(&self.mono, &mut out);
            if !out.is_empty() {
                let _ = self.tx.send(Ok(out));
            }
        }
    }

    pub fn start(cancel: &AtomicBool) -> Result<(Capture, mpsc::Receiver<Chunk>)> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        // cpal streams are not Send on every platform: build, run and drop on one thread.
        let thread = std::thread::spawn(move || {
            let stream = (|| -> Result<(cpal::Stream, String)> {
                let host = cpal::default_host();
                let device = host
                    .default_input_device()
                    .context("no default microphone; check System Settings > Sound > Input")?;
                let name = device.name().unwrap_or_else(|_| "microphone".into());
                let supported = device
                    .default_input_config()
                    .context("reading the microphone's input format")?;
                let format = supported.sample_format();
                let config: cpal::StreamConfig = supported.into();
                let make = |tx: mpsc::Sender<Chunk>| Converter {
                    channels: config.channels.max(1) as usize,
                    resampler: Resampler::new(config.sample_rate.0, SAMPLE_RATE),
                    mono: Vec::new(),
                    tx,
                };
                let error_tx = tx.clone();
                let on_error = move |error: cpal::StreamError| {
                    let _ = error_tx.send(Err(format!("microphone stream failed: {error}")));
                };
                let stream = match format {
                    cpal::SampleFormat::F32 => {
                        let mut convert = make(tx.clone());
                        device.build_input_stream(
                            &config,
                            move |data: &[f32], _: &cpal::InputCallbackInfo| convert.push(data),
                            on_error,
                            None,
                        )
                    }
                    cpal::SampleFormat::I16 => {
                        let mut convert = make(tx.clone());
                        let mut scratch = Vec::new();
                        device.build_input_stream(
                            &config,
                            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                                scratch.clear();
                                scratch.extend(data.iter().map(|&s| s as f32 / 32768.0));
                                convert.push(&scratch);
                            },
                            on_error,
                            None,
                        )
                    }
                    cpal::SampleFormat::I32 => {
                        let mut convert = make(tx.clone());
                        let mut scratch = Vec::new();
                        device.build_input_stream(
                            &config,
                            move |data: &[i32], _: &cpal::InputCallbackInfo| {
                                scratch.clear();
                                scratch.extend(data.iter().map(|&s| s as f32 / 2_147_483_648.0));
                                convert.push(&scratch);
                            },
                            on_error,
                            None,
                        )
                    }
                    cpal::SampleFormat::U16 => {
                        let mut convert = make(tx.clone());
                        let mut scratch = Vec::new();
                        device.build_input_stream(
                            &config,
                            move |data: &[u16], _: &cpal::InputCallbackInfo| {
                                scratch.clear();
                                scratch.extend(data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0));
                                convert.push(&scratch);
                            },
                            on_error,
                            None,
                        )
                    }
                    other => anyhow::bail!("unsupported microphone sample format {other:?}"),
                }
                .context("opening the microphone (grant your terminal microphone access in System Settings > Privacy & Security)")?;
                stream.play().context("starting the microphone")?;
                Ok((stream, name))
            })();
            match stream {
                Ok((stream, name)) => {
                    let _ = ready_tx.send(Ok(name));
                    while !thread_stop.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    drop(stream);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let source = loop {
            if cancel.load(Ordering::SeqCst) {
                break Err(anyhow::anyhow!("microphone start cancelled"));
            }
            match ready_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ready) => break ready,
                Err(mpsc::RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => {}
                Err(_) => {
                    break Err(anyhow::anyhow!(
                        "microphone did not start within 10 seconds"
                    ));
                }
            }
        };
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                stop.store(true, Ordering::SeqCst);
                let _ = thread.join();
                return Err(error);
            }
        };
        Ok((
            Capture {
                stop,
                child: Arc::new(Mutex::new(None)),
                thread: Some(thread),
                source,
            },
            rx,
        ))
    }
}

#[cfg(not(all(feature = "voice", target_os = "macos")))]
mod native {
    use super::{Capture, Chunk};
    use anyhow::Result;
    use std::sync::mpsc;

    pub const AVAILABLE: bool = false;

    pub fn start(
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(Capture, mpsc::Receiver<Chunk>)> {
        anyhow::bail!(
            "native microphone capture is not compiled into this build (it needs the `voice` feature on macOS); set voice.stt.capture = \"command\" to use a recorder program"
        )
    }
}

/// How long [`drain`] waits for the first chunk before returning.
#[cfg(test)]
pub const POLL: Duration = Duration::from_millis(40);

/// Collect every chunk currently queued (waiting up to [`POLL`] for the first). Returns the samples
/// and a terminal error if the capture failed.
#[cfg(test)]
pub fn drain(rx: &mpsc::Receiver<Chunk>, out: &mut Vec<f32>) -> Option<String> {
    let mut next = rx
        .recv_timeout(POLL)
        .map_err(|e| e == mpsc::RecvTimeoutError::Disconnected);
    loop {
        match next {
            Ok(Ok(samples)) => out.extend_from_slice(&samples),
            Ok(Err(error)) => return Some(error),
            Err(true) => return Some("microphone capture ended".into()),
            Err(false) => return None,
        }
        next = rx
            .try_recv()
            .map_err(|e| e == mpsc::TryRecvError::Disconnected);
    }
}

#[cfg(test)]
mod tests;
