use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::{sleep, Instant};
use tract_onnx::prelude::*;

use crate::audio::{
    pcm_bytes_for_duration, record_wav_with_pw_record, snapshot_streaming_pcm,
    start_streaming_pcm_capture, write_pcm_wav, SharedPcmBuffer, StreamingPcmCapture,
    StreamingPcmSession, STT_SAMPLE_RATE,
};
use crate::inject::{format_transcript_for_injection, TextInjector};
use crate::stt::{transcribe_file, TranscribeOptions};

pub const DEFAULT_WAKEWORD_NAME: &str = "default";
pub const DEFAULT_WAKEWORD_THRESHOLD: f32 = 0.5;
pub const DEFAULT_WAKEWORD_FRAME_MS: u64 = 80;
pub const DEFAULT_WAKEWORD_SAMPLE_COUNT: usize = 10;
pub const DEFAULT_WAKEWORD_SAMPLE_DURATION_MS: u64 = 1_500;
pub const DEFAULT_WAKEWORD_SILENCE_TIMEOUT_MS: u64 = 900;
pub const DEFAULT_WAKEWORD_ACTIVATION_GRACE_MS: u64 = 5_000;
pub const DEFAULT_WAKEWORD_MAX_RECORDING_MS: u64 = 30_000;
const DEFAULT_WAKEWORD_IDLE_RETAIN_MS: u64 = 5_000;
const SPEECH_RMS_THRESHOLD: f64 = 700.0;

#[derive(Debug, Clone, PartialEq)]
pub struct WakewordSettings {
    pub name: String,
    pub root_dir: PathBuf,
    pub threshold: f32,
    pub frame: Duration,
    pub silence_timeout: Duration,
    pub sample_count: usize,
    pub sample_duration: Duration,
    pub training_command: Option<String>,
    pub activation_grace: Duration,
    pub max_recording: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakewordPaths {
    pub root: PathBuf,
    pub samples_dir: PathBuf,
    pub preprocessed_dir: PathBuf,
    pub model_path: PathBuf,
    pub metadata_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WakeDetection {
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct WakewordRunConfig {
    pub settings: WakewordSettings,
    pub base_url: String,
    pub stt_options: TranscribeOptions,
    pub append_space: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WakewordMetadata {
    name: String,
    threshold: f32,
    frame_ms: u64,
    sample_count: usize,
    model_path: PathBuf,
}

pub trait WakeScorer {
    fn score(&mut self, pcm: &[i16]) -> anyhow::Result<f32>;
}

pub struct WakeDetector<S> {
    scorer: S,
    threshold: f32,
    frame_samples: usize,
    pending: Vec<i16>,
}

impl<S> WakeDetector<S>
where
    S: WakeScorer,
{
    pub fn new(scorer: S, threshold: f32, frame: Duration) -> Self {
        let frame_samples = samples_for_duration(STT_SAMPLE_RATE, frame).max(1);
        Self {
            scorer,
            threshold,
            frame_samples,
            pending: Vec::new(),
        }
    }

    pub fn observe_pcm(&mut self, pcm: &[u8]) -> anyhow::Result<Option<WakeDetection>> {
        self.pending.extend(pcm_s16le_samples(pcm));
        while self.pending.len() >= self.frame_samples {
            let frame = self.pending.drain(..self.frame_samples).collect::<Vec<_>>();
            let score = self.scorer.score(&frame)?;
            if score >= self.threshold {
                self.pending.clear();
                return Ok(Some(WakeDetection { score }));
            }
        }
        Ok(None)
    }

    pub fn scorer_mut(&mut self) -> &mut S {
        &mut self.scorer
    }
}

pub struct OnnxWakeScorer {
    model: Arc<TypedSimplePlan>,
}

impl OnnxWakeScorer {
    pub fn load(path: &Path, frame: Duration) -> anyhow::Result<Self> {
        let frame_samples = samples_for_duration(STT_SAMPLE_RATE, frame).max(1);
        let input_fact = f32::fact([1, frame_samples]).into();
        let model = tract_onnx::onnx()
            .model_for_path(path)
            .with_context(|| format!("failed to load wakeword ONNX model {}", path.display()))?
            .with_input_fact(0, input_fact)
            .context("failed to set wakeword ONNX input shape")?
            .into_optimized()
            .context("failed to optimize wakeword ONNX model")?
            .into_runnable()
            .context("failed to prepare wakeword ONNX model")?;
        Ok(Self { model })
    }
}

impl WakeScorer for OnnxWakeScorer {
    fn score(&mut self, pcm: &[i16]) -> anyhow::Result<f32> {
        let samples = pcm
            .iter()
            .map(|sample| f32::from(*sample) / 32768.0)
            .collect::<Vec<_>>();
        let input = Tensor::from_shape(&[1, samples.len()], &samples)
            .context("failed to build wakeword ONNX input tensor")?;
        let outputs = self
            .model
            .run(tvec!(input.into()))
            .context("wakeword ONNX inference failed")?;
        let first = outputs
            .first()
            .context("wakeword ONNX model produced no outputs")?;
        let scores = first
            .try_as_plain()
            .context("wakeword ONNX output is not a plain tensor")?
            .as_slice::<f32>()
            .context("wakeword ONNX output is not f32")?;
        scores
            .iter()
            .copied()
            .reduce(f32::max)
            .context("wakeword ONNX output was empty")
    }
}

pub fn default_wakeword_root() -> PathBuf {
    default_wakeword_root_with_env(&std::env::vars().collect())
}

pub fn default_wakeword_root_with_env(env: &BTreeMap<String, String>) -> PathBuf {
    env.get("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.get("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        })
        .unwrap_or_else(|| PathBuf::from("."))
        .join("speaches-companion/wakewords")
}

pub fn validate_wakeword_name(name: &str) -> anyhow::Result<&str> {
    if name.is_empty() {
        bail!("wakeword name must not be empty");
    }
    if name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        Ok(name)
    } else {
        bail!("wakeword name must contain only ASCII letters, digits, '-' or '_'");
    }
}

pub fn wakeword_paths(settings: &WakewordSettings) -> anyhow::Result<WakewordPaths> {
    let name = validate_wakeword_name(&settings.name)?;
    let root = settings.root_dir.join(name);
    Ok(WakewordPaths {
        samples_dir: root.join("samples"),
        preprocessed_dir: root.join("preprocessed"),
        model_path: root.join("model.onnx"),
        metadata_path: root.join("metadata.json"),
        root,
    })
}

pub async fn ensure_wakeword_model(
    settings: &WakewordSettings,
    retrain: bool,
) -> anyhow::Result<WakewordPaths> {
    let paths = wakeword_paths(settings)?;
    if !retrain && tokio::fs::metadata(&paths.model_path).await.is_ok() {
        return Ok(paths);
    }

    collect_wakeword_samples(settings, &paths).await?;
    prepare_training_samples(&paths).await?;
    run_training_command(settings, &paths).await?;

    tokio::fs::metadata(&paths.model_path)
        .await
        .with_context(|| {
            format!(
                "wakeword training did not create expected ONNX model {}",
                paths.model_path.display()
            )
        })?;
    write_metadata(settings, &paths).await?;
    Ok(paths)
}

pub async fn collect_wakeword_samples(
    settings: &WakewordSettings,
    paths: &WakewordPaths,
) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(&paths.samples_dir)
        .await
        .with_context(|| format!("failed to create {}", paths.samples_dir.display()))?;

    for index in 1..=settings.sample_count {
        let sample_path = paths.samples_dir.join(format!("sample-{index:02}.wav"));
        if tokio::fs::metadata(&sample_path).await.is_ok() {
            continue;
        }
        eprintln!(
            "speaches-companion wakeword '{}': recording sample {index}/{} to {}",
            settings.name,
            settings.sample_count,
            sample_path.display()
        );
        sleep(Duration::from_millis(750)).await;
        record_wav_with_pw_record(&sample_path, settings.sample_duration, STT_SAMPLE_RATE).await?;
    }
    Ok(())
}

pub async fn prepare_training_samples(paths: &WakewordPaths) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(&paths.preprocessed_dir)
        .await
        .with_context(|| format!("failed to create {}", paths.preprocessed_dir.display()))?;
    let mut entries = tokio::fs::read_dir(&paths.samples_dir)
        .await
        .with_context(|| format!("failed to read {}", paths.samples_dir.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("wav")) {
            continue;
        }
        let target = paths
            .preprocessed_dir
            .join(path.file_name().context("sample path has no file name")?);
        tokio::fs::copy(&path, &target).await.with_context(|| {
            format!(
                "failed to copy wakeword sample {} to {}",
                path.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

pub async fn run_training_command(
    settings: &WakewordSettings,
    paths: &WakewordPaths,
) -> anyhow::Result<()> {
    let Some(command) = settings
        .training_command
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        bail!(
            "wakeword model {} is missing and no training command is configured",
            paths.model_path.display()
        );
    };

    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("SPEACHES_COMPANION_WAKEWORD_NAME", &settings.name)
        .env("SPEACHES_COMPANION_WAKEWORD_ROOT", &paths.root)
        .env(
            "SPEACHES_COMPANION_WAKEWORD_SAMPLES_DIR",
            &paths.samples_dir,
        )
        .env(
            "SPEACHES_COMPANION_WAKEWORD_PREPROCESSED_DIR",
            &paths.preprocessed_dir,
        )
        .env("SPEACHES_COMPANION_WAKEWORD_MODEL", &paths.model_path)
        .env("SPEACHES_COMPANION_WAKEWORD_METADATA", &paths.metadata_path)
        .env(
            "SPEACHES_COMPANION_WAKEWORD_SAMPLE_COUNT",
            settings.sample_count.to_string(),
        )
        .stdin(Stdio::null())
        .status()
        .await
        .context("failed to start wakeword training command")?;
    if !status.success() {
        bail!("wakeword training command failed with {status}");
    }
    Ok(())
}

pub async fn run_wakeword_loop<I>(config: WakewordRunConfig, injector: I) -> anyhow::Result<()>
where
    I: TextInjector,
{
    let paths = ensure_wakeword_model(&config.settings, false).await?;
    let scorer = OnnxWakeScorer::load(&paths.model_path, config.settings.frame)?;
    run_wakeword_loop_with_scorer(config, scorer, injector).await
}

pub async fn run_wakeword_loop_with_scorer<S, I>(
    config: WakewordRunConfig,
    scorer: S,
    injector: I,
) -> anyhow::Result<()>
where
    S: WakeScorer,
    I: TextInjector,
{
    let _paths = wakeword_paths(&config.settings)?;
    let capture = start_streaming_pcm_capture(
        STT_SAMPLE_RATE,
        Duration::from_millis(DEFAULT_WAKEWORD_IDLE_RETAIN_MS),
    )
    .await
    .context("failed to start wakeword microphone capture")?;
    let shared_pcm = capture.shared_pcm();
    let mut detector = WakeDetector::new(scorer, config.settings.threshold, config.settings.frame);
    run_wakeword_loop_on_capture(config, capture, shared_pcm, &mut detector, injector).await
}

async fn run_wakeword_loop_on_capture<S, I>(
    config: WakewordRunConfig,
    _capture: StreamingPcmCapture,
    shared_pcm: SharedPcmBuffer,
    detector: &mut WakeDetector<S>,
    injector: I,
) -> anyhow::Result<()>
where
    S: WakeScorer,
    I: TextInjector,
{
    loop {
        let detection = wait_for_wake(&shared_pcm, detector, &config.settings).await?;
        eprintln!(
            "speaches-companion wakeword '{}' detected with score {:.3}",
            config.settings.name, detection.score
        );
        let pcm = record_until_silence(&shared_pcm, &config.settings).await?;
        if pcm.is_empty() {
            continue;
        }
        let transcript = transcribe_wake_recording(&config, &pcm).await?;
        if let Some(text) = format_transcript_for_injection(&transcript, config.append_space) {
            injector.inject_text(&text)?;
        }
    }
}

pub async fn wait_for_wake<S>(
    shared_pcm: &SharedPcmBuffer,
    detector: &mut WakeDetector<S>,
    settings: &WakewordSettings,
) -> anyhow::Result<WakeDetection>
where
    S: WakeScorer,
{
    let mut cursor = 0usize;
    loop {
        let snapshot = snapshot_streaming_pcm(shared_pcm).await;
        let buffer_start = snapshot.bytes_seen.saturating_sub(snapshot.pcm.len());
        let start = cursor.saturating_sub(buffer_start).min(snapshot.pcm.len());
        let new_pcm = &snapshot.pcm[start..];
        cursor = snapshot.bytes_seen;
        if let Some(detection) = detector.observe_pcm(new_pcm)? {
            return Ok(detection);
        }
        sleep((settings.frame / 2).max(Duration::from_millis(10))).await;
    }
}

pub async fn record_until_silence(
    shared_pcm: &SharedPcmBuffer,
    settings: &WakewordSettings,
) -> anyhow::Result<Vec<u8>> {
    let session =
        StreamingPcmSession::start(shared_pcm.clone(), STT_SAMPLE_RATE, Duration::ZERO).await?;
    let started = Instant::now();
    let mut cursor = 0usize;
    let mut saw_speech = false;
    let mut last_speech = started;

    loop {
        sleep((settings.frame / 2).max(Duration::from_millis(10))).await;
        let snapshot = session.snapshot().await;
        let new_pcm = &snapshot[cursor.min(snapshot.len())..];
        cursor = snapshot.len();
        if rms_s16le(new_pcm) >= SPEECH_RMS_THRESHOLD {
            saw_speech = true;
            last_speech = Instant::now();
        }
        let now = Instant::now();
        if saw_speech && now.duration_since(last_speech) >= settings.silence_timeout {
            break;
        }
        if !saw_speech && now.duration_since(started) >= settings.activation_grace {
            break;
        }
        if now.duration_since(started) >= settings.max_recording {
            break;
        }
    }

    let pcm = session.snapshot().await;
    session.finish().await;
    if saw_speech {
        Ok(pcm)
    } else {
        Ok(Vec::new())
    }
}

async fn transcribe_wake_recording(
    config: &WakewordRunConfig,
    pcm: &[u8],
) -> anyhow::Result<String> {
    let path = std::env::temp_dir().join(format!("speaches-companion-wakeword-{}.wav", unix_ms()));
    write_pcm_wav(&path, pcm, STT_SAMPLE_RATE).await?;
    let result = transcribe_file(&config.base_url, &path, &config.stt_options).await;
    let cleanup = tokio::fs::remove_file(&path).await;
    if let Err(error) = cleanup {
        eprintln!("failed to remove {}: {error:#}", path.display());
    }
    result
}

async fn write_metadata(settings: &WakewordSettings, paths: &WakewordPaths) -> anyhow::Result<()> {
    let metadata = WakewordMetadata {
        name: settings.name.clone(),
        threshold: settings.threshold,
        frame_ms: settings.frame.as_millis() as u64,
        sample_count: settings.sample_count,
        model_path: paths.model_path.clone(),
    };
    let data = serde_json::to_vec_pretty(&metadata)?;
    tokio::fs::write(&paths.metadata_path, data)
        .await
        .with_context(|| format!("failed to write {}", paths.metadata_path.display()))
}

fn samples_for_duration(sample_rate: u32, duration: Duration) -> usize {
    pcm_bytes_for_duration(sample_rate, duration) / 2
}

fn pcm_s16le_samples(pcm: &[u8]) -> impl Iterator<Item = i16> + '_ {
    pcm.chunks_exact(2)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
}

fn rms_s16le(pcm: &[u8]) -> f64 {
    let mut sum = 0f64;
    let mut count = 0usize;
    for sample in pcm_s16le_samples(pcm) {
        let value = f64::from(sample);
        sum += value * value;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt()
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeScorer {
        scores: Vec<f32>,
    }

    impl WakeScorer for FakeScorer {
        fn score(&mut self, _pcm: &[i16]) -> anyhow::Result<f32> {
            Ok(if self.scores.is_empty() {
                0.0
            } else {
                self.scores.remove(0)
            })
        }
    }

    #[test]
    fn wake_detector_triggers_when_score_crosses_threshold() {
        let scorer = FakeScorer {
            scores: vec![0.1, 0.8],
        };
        let mut detector = WakeDetector::new(scorer, 0.5, Duration::from_millis(1));

        assert_eq!(detector.observe_pcm(&[0; 32]).unwrap(), None);
        assert_eq!(
            detector.observe_pcm(&[0; 64]).unwrap(),
            Some(WakeDetection { score: 0.8 })
        );
    }

    #[test]
    fn wakeword_name_rejects_path_traversal() {
        assert!(validate_wakeword_name("default").is_ok());
        assert!(validate_wakeword_name("hey-samantha_2").is_ok());
        assert!(validate_wakeword_name("../default").is_err());
        assert!(validate_wakeword_name("").is_err());
    }

    #[test]
    fn default_wakeword_root_uses_xdg_data_home() {
        let mut env = BTreeMap::new();
        env.insert("XDG_DATA_HOME".to_string(), "/tmp/data".to_string());

        assert_eq!(
            default_wakeword_root_with_env(&env),
            PathBuf::from("/tmp/data/speaches-companion/wakewords")
        );
    }

    #[test]
    fn wakeword_paths_are_under_named_root() {
        let settings = WakewordSettings {
            name: "default".to_string(),
            root_dir: PathBuf::from("/tmp/wakewords"),
            threshold: DEFAULT_WAKEWORD_THRESHOLD,
            frame: Duration::from_millis(DEFAULT_WAKEWORD_FRAME_MS),
            silence_timeout: Duration::from_millis(DEFAULT_WAKEWORD_SILENCE_TIMEOUT_MS),
            sample_count: DEFAULT_WAKEWORD_SAMPLE_COUNT,
            sample_duration: Duration::from_millis(DEFAULT_WAKEWORD_SAMPLE_DURATION_MS),
            training_command: None,
            activation_grace: Duration::from_millis(DEFAULT_WAKEWORD_ACTIVATION_GRACE_MS),
            max_recording: Duration::from_millis(DEFAULT_WAKEWORD_MAX_RECORDING_MS),
        };

        let paths = wakeword_paths(&settings).unwrap();

        assert_eq!(
            paths.model_path,
            PathBuf::from("/tmp/wakewords/default/model.onnx")
        );
        assert_eq!(
            paths.preprocessed_dir,
            PathBuf::from("/tmp/wakewords/default/preprocessed")
        );
    }
}
