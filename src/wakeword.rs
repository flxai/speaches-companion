use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, Instant};
use tract_onnx::prelude::*;

use crate::audio::{
    pcm_bytes_for_duration, snapshot_streaming_pcm, start_streaming_pcm_capture, write_pcm_wav,
    SharedPcmBuffer, StreamingPcmCapture, StreamingPcmSession, STT_SAMPLE_RATE,
};
use crate::daemon::{DaemonResponse, HotkeyHandler};
use crate::inject::{format_transcript_for_injection, TextInjector};
use crate::ipc::IpcCommand;
use crate::notification::{DesktopWakewordNotifier, WakewordNotifier};
use crate::realtime::RealtimeTranscriber;
use crate::streaming::{PartialChunkingConfig, StreamingDictationController};
use crate::stt::{transcribe_file, TranscribeOptions};

pub const DEFAULT_WAKEWORD_NAME: &str = "default";
pub const DEFAULT_WAKEWORD_THRESHOLD: f32 = 0.5;
pub const DEFAULT_WAKEWORD_FRAME_MS: u64 = 80;
pub const DEFAULT_WAKEWORD_SILENCE_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_WAKEWORD_ACTIVATION_GRACE_MS: u64 = 5_000;
pub const DEFAULT_WAKEWORD_MAX_RECORDING_MS: u64 = 30_000;
pub const DEFAULT_WAKEWORD_PRESS_ENTER: bool = true;
pub const DEFAULT_OPENWAKEWORD_STOCK_MODEL: OpenWakewordStockModel = OpenWakewordStockModel::Alexa;

const DEFAULT_WAKEWORD_IDLE_RETAIN_MS: u64 = 5_000;
const OPENWAKEWORD_CACHE_DIR: &str = "_openwakeword";
const OPENWAKEWORD_RELEASE_VERSION: &str = "v0.5.1";
const OPENWAKEWORD_RELEASE_BASE_URL: &str =
    "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1";
const OPENWAKEWORD_MELSPECTROGRAM_FILENAME: &str = "melspectrogram.onnx";
const OPENWAKEWORD_EMBEDDING_FILENAME: &str = "embedding_model.onnx";
const OPENWAKEWORD_FRAME_SAMPLES: usize = 1_280;
const OPENWAKEWORD_MELSPEC_CONTEXT_SAMPLES: usize = 160 * 3;
const OPENWAKEWORD_MEL_BINS: usize = 32;
const OPENWAKEWORD_MEL_WINDOW_FRAMES: usize = 76;
const OPENWAKEWORD_MELSPEC_MAX_FRAMES: usize = 970;
const OPENWAKEWORD_EMBEDDING_STEP_FRAMES: usize = 8;
const WAKEWORD_ACTIVITY_STATE_FILE_ENV: &str = "SPEACHES_COMPANION_WAKEWORD_ACTIVITY_STATE_FILE";
const WAKEWORD_ACTIVITY_REFRESH_COMMAND_ENV: &str =
    "SPEACHES_COMPANION_WAKEWORD_ACTIVITY_REFRESH_COMMAND";
const OPENWAKEWORD_EMBEDDING_DIM: usize = 96;
const OPENWAKEWORD_FEATURE_MAX_FRAMES: usize = 120;
const OPENWAKEWORD_RAW_BUFFER_MAX_SAMPLES: usize = STT_SAMPLE_RATE as usize * 10;
const OPENWAKEWORD_WARMUP_WINDOWS: usize = 5;
const SPEECH_RMS_THRESHOLD: f64 = 700.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum WakewordEngine {
    #[default]
    Openwakeword,
    Onnx,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum OpenWakewordStockModel {
    #[default]
    Alexa,
    Timer,
    Weather,
}

impl OpenWakewordStockModel {
    fn asset_filename(self) -> &'static str {
        match self {
            Self::Alexa => "alexa_v0.1.onnx",
            Self::Timer => "timer_v0.1.onnx",
            Self::Weather => "weather_v0.1.onnx",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WakewordSettings {
    pub name: String,
    pub engine: WakewordEngine,
    pub stock_model: OpenWakewordStockModel,
    pub assets_dir: Option<PathBuf>,
    pub root_dir: PathBuf,
    pub threshold: f32,
    pub frame: Duration,
    pub silence_timeout: Duration,
    pub activation_grace: Duration,
    pub max_recording: Duration,
    pub press_enter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakewordPaths {
    pub root: PathBuf,
    pub model_path: PathBuf,
    pub metadata_path: PathBuf,
    pub shared_assets_dir: PathBuf,
    pub melspectrogram_path: PathBuf,
    pub embedding_model_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WakeDetection {
    pub score: f32,
}

#[derive(Clone)]
pub struct WakewordRunConfig {
    pub settings: WakewordSettings,
    pub base_url: String,
    pub stt_options: TranscribeOptions,
    pub append_space: bool,
    pub notify_on_detect: bool,
    pub streaming: Option<WakewordStreamingConfig>,
}

#[derive(Clone)]
pub struct WakewordStreamingConfig {
    pub transcriber: RealtimeTranscriber,
    pub listening_marker: Option<String>,
    pub inline_partials: bool,
    pub partial_chunking: PartialChunkingConfig,
    pub final_transcript: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WakewordMetadata {
    name: String,
    engine: WakewordEngine,
    stock_model: Option<OpenWakewordStockModel>,
    threshold: f32,
    frame_ms: u64,
    model_path: PathBuf,
    shared_assets_dir: Option<PathBuf>,
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

trait OpenWakewordBackend {
    fn keyword_frames(&self) -> usize;

    fn run_melspectrogram(&mut self, samples: &[i16]) -> anyhow::Result<Vec<f32>>;

    fn run_embedding(&mut self, mel_window: &[f32]) -> anyhow::Result<Vec<f32>>;

    fn run_keyword(&mut self, feature_window: &[f32]) -> anyhow::Result<f32>;
}

struct TractOpenWakewordBackend {
    melspectrogram: Arc<TypedSimplePlan>,
    embedding: Arc<TypedSimplePlan>,
    keyword: Arc<TypedSimplePlan>,
    keyword_frames: usize,
}

impl TractOpenWakewordBackend {
    fn load(paths: &WakewordPaths) -> anyhow::Result<Self> {
        let melspectrogram = tract_onnx::onnx()
            .model_for_path(&paths.melspectrogram_path)
            .with_context(|| {
                format!(
                    "failed to load openWakeWord melspectrogram model {}",
                    paths.melspectrogram_path.display()
                )
            })?
            .into_optimized()
            .context("failed to optimize openWakeWord melspectrogram model")?
            .into_runnable()
            .context("failed to prepare openWakeWord melspectrogram model")?;

        let embedding = tract_onnx::onnx()
            .model_for_path(&paths.embedding_model_path)
            .with_context(|| {
                format!(
                    "failed to load openWakeWord embedding model {}",
                    paths.embedding_model_path.display()
                )
            })?
            .into_optimized()
            .context("failed to optimize openWakeWord embedding model")?
            .into_runnable()
            .context("failed to prepare openWakeWord embedding model")?;

        let keyword_model = tract_onnx::onnx()
            .model_for_path(&paths.model_path)
            .with_context(|| {
                format!(
                    "failed to load openWakeWord keyword model {}",
                    paths.model_path.display()
                )
            })?
            .into_optimized()
            .context("failed to optimize openWakeWord keyword model")?;
        let keyword_frames = infer_keyword_frames(&keyword_model)?;
        let keyword = keyword_model
            .into_runnable()
            .context("failed to prepare openWakeWord keyword model")?;

        Ok(Self {
            melspectrogram,
            embedding,
            keyword,
            keyword_frames,
        })
    }
}

impl OpenWakewordBackend for TractOpenWakewordBackend {
    fn keyword_frames(&self) -> usize {
        self.keyword_frames
    }

    fn run_melspectrogram(&mut self, samples: &[i16]) -> anyhow::Result<Vec<f32>> {
        let input = samples
            .iter()
            .map(|sample| f32::from(*sample))
            .collect::<Vec<_>>();
        let input = Tensor::from_shape(&[1, input.len()], &input)
            .context("failed to build openWakeWord melspectrogram input")?;
        let outputs = self
            .melspectrogram
            .run(tvec!(input.into()))
            .context("openWakeWord melspectrogram inference failed")?;
        let values = first_output_as_slice(&outputs, "openWakeWord melspectrogram output")?;
        Ok(values.iter().map(|value| *value / 10.0 + 2.0).collect())
    }

    fn run_embedding(&mut self, mel_window: &[f32]) -> anyhow::Result<Vec<f32>> {
        let input = Tensor::from_shape(
            &[1, OPENWAKEWORD_MEL_WINDOW_FRAMES, OPENWAKEWORD_MEL_BINS, 1],
            mel_window,
        )
        .context("failed to build openWakeWord embedding input")?;
        let outputs = self
            .embedding
            .run(tvec!(input.into()))
            .context("openWakeWord embedding inference failed")?;
        Ok(first_output_as_slice(&outputs, "openWakeWord embedding output")?.to_vec())
    }

    fn run_keyword(&mut self, feature_window: &[f32]) -> anyhow::Result<f32> {
        let input = Tensor::from_shape(
            &[1, self.keyword_frames, OPENWAKEWORD_EMBEDDING_DIM],
            feature_window,
        )
        .context("failed to build openWakeWord keyword input")?;
        let outputs = self
            .keyword
            .run(tvec!(input.into()))
            .context("openWakeWord keyword inference failed")?;
        let scores = first_output_as_slice(&outputs, "openWakeWord keyword output")?;
        scores
            .iter()
            .copied()
            .reduce(f32::max)
            .context("openWakeWord keyword output was empty")
    }
}

struct OpenWakewordPipelineScorer<B> {
    backend: B,
    raw_data_buffer: VecDeque<i16>,
    raw_data_remainder: Vec<i16>,
    accumulated_samples: usize,
    melspectrogram_buffer: Vec<f32>,
    feature_buffer: Vec<f32>,
    warmup_windows_remaining: usize,
    last_score: f32,
}

impl OpenWakewordPipelineScorer<TractOpenWakewordBackend> {
    fn load(paths: &WakewordPaths) -> anyhow::Result<Self> {
        Self::with_backend(TractOpenWakewordBackend::load(paths)?)
    }
}

impl<B> OpenWakewordPipelineScorer<B>
where
    B: OpenWakewordBackend,
{
    fn with_backend(backend: B) -> anyhow::Result<Self> {
        let keyword_frames = backend.keyword_frames();
        if keyword_frames == 0 {
            bail!("openWakeWord keyword model must consume at least one feature frame");
        }
        Ok(Self {
            backend,
            raw_data_buffer: VecDeque::with_capacity(OPENWAKEWORD_RAW_BUFFER_MAX_SAMPLES),
            raw_data_remainder: Vec::new(),
            accumulated_samples: 0,
            melspectrogram_buffer: vec![
                1.0;
                OPENWAKEWORD_MEL_WINDOW_FRAMES * OPENWAKEWORD_MEL_BINS
            ],
            feature_buffer: vec![0.0; OPENWAKEWORD_FEATURE_MAX_FRAMES * OPENWAKEWORD_EMBEDDING_DIM],
            warmup_windows_remaining: OPENWAKEWORD_WARMUP_WINDOWS,
            last_score: 0.0,
        })
    }

    fn buffer_raw_data(&mut self, samples: &[i16]) {
        self.raw_data_buffer.extend(samples.iter().copied());
        while self.raw_data_buffer.len() > OPENWAKEWORD_RAW_BUFFER_MAX_SAMPLES {
            self.raw_data_buffer.pop_front();
        }
    }

    fn stream_melspectrogram(&mut self, new_sample_count: usize) -> anyhow::Result<()> {
        if self.raw_data_buffer.len() < OPENWAKEWORD_MELSPEC_CONTEXT_SAMPLES {
            bail!("openWakeWord needs at least 480 samples of context");
        }

        let take = (new_sample_count + OPENWAKEWORD_MELSPEC_CONTEXT_SAMPLES)
            .min(self.raw_data_buffer.len());
        let start = self.raw_data_buffer.len() - take;
        let samples = self
            .raw_data_buffer
            .iter()
            .skip(start)
            .copied()
            .collect::<Vec<_>>();
        let mel = self.backend.run_melspectrogram(&samples)?;
        if mel.len() % OPENWAKEWORD_MEL_BINS != 0 {
            bail!(
                "openWakeWord melspectrogram output {} is not divisible by {} mel bins",
                mel.len(),
                OPENWAKEWORD_MEL_BINS
            );
        }
        self.melspectrogram_buffer.extend(mel);
        trim_frame_buffer(
            &mut self.melspectrogram_buffer,
            OPENWAKEWORD_MEL_BINS,
            OPENWAKEWORD_MELSPEC_MAX_FRAMES,
        );
        Ok(())
    }

    fn stream_features(&mut self, pcm: &[i16]) -> anyhow::Result<usize> {
        let mut samples = if self.raw_data_remainder.is_empty() {
            pcm.to_vec()
        } else {
            let mut merged = Vec::with_capacity(self.raw_data_remainder.len() + pcm.len());
            merged.extend_from_slice(&self.raw_data_remainder);
            merged.extend_from_slice(pcm);
            self.raw_data_remainder.clear();
            merged
        };

        if self.accumulated_samples + samples.len() >= OPENWAKEWORD_FRAME_SAMPLES {
            let remainder = (self.accumulated_samples + samples.len()) % OPENWAKEWORD_FRAME_SAMPLES;
            if remainder != 0 {
                let even_len = samples.len() - remainder;
                self.buffer_raw_data(&samples[..even_len]);
                self.accumulated_samples += even_len;
                self.raw_data_remainder = samples.split_off(even_len);
            } else {
                self.buffer_raw_data(&samples);
                self.accumulated_samples += samples.len();
            }
        } else {
            self.accumulated_samples += samples.len();
            self.buffer_raw_data(&samples);
        }

        if self.accumulated_samples >= OPENWAKEWORD_FRAME_SAMPLES
            && self.accumulated_samples % OPENWAKEWORD_FRAME_SAMPLES == 0
        {
            let new_frames = self.accumulated_samples / OPENWAKEWORD_FRAME_SAMPLES;
            self.stream_melspectrogram(self.accumulated_samples)?;
            for offset in (0..new_frames).rev() {
                let mel_frames = frame_count(&self.melspectrogram_buffer, OPENWAKEWORD_MEL_BINS);
                let end_frame = mel_frames - offset * OPENWAKEWORD_EMBEDDING_STEP_FRAMES;
                let start_frame = end_frame.saturating_sub(OPENWAKEWORD_MEL_WINDOW_FRAMES);
                if end_frame - start_frame != OPENWAKEWORD_MEL_WINDOW_FRAMES {
                    continue;
                }
                let start = start_frame * OPENWAKEWORD_MEL_BINS;
                let end = end_frame * OPENWAKEWORD_MEL_BINS;
                let embedding = self
                    .backend
                    .run_embedding(&self.melspectrogram_buffer[start..end])?;
                if embedding.len() != OPENWAKEWORD_EMBEDDING_DIM {
                    bail!(
                        "openWakeWord embedding output has {} values, expected {}",
                        embedding.len(),
                        OPENWAKEWORD_EMBEDDING_DIM
                    );
                }
                self.feature_buffer.extend(embedding);
            }
            trim_frame_buffer(
                &mut self.feature_buffer,
                OPENWAKEWORD_EMBEDDING_DIM,
                OPENWAKEWORD_FEATURE_MAX_FRAMES,
            );
            self.accumulated_samples = 0;
            Ok(new_frames)
        } else {
            Ok(0)
        }
    }
}

impl<B> WakeScorer for OpenWakewordPipelineScorer<B>
where
    B: OpenWakewordBackend,
{
    fn score(&mut self, pcm: &[i16]) -> anyhow::Result<f32> {
        let new_frames = self.stream_features(pcm)?;
        if new_frames == 0 {
            return Ok(self.last_score);
        }

        let keyword_frames = self.backend.keyword_frames();
        let total_feature_frames = frame_count(&self.feature_buffer, OPENWAKEWORD_EMBEDDING_DIM);
        let mut batch_max = 0.0f32;

        for offset in (0..new_frames).rev() {
            let end_frame = total_feature_frames - offset;
            let start_frame = end_frame.saturating_sub(keyword_frames);
            if end_frame - start_frame != keyword_frames {
                continue;
            }
            let start = start_frame * OPENWAKEWORD_EMBEDDING_DIM;
            let end = end_frame * OPENWAKEWORD_EMBEDDING_DIM;
            let mut score = self.backend.run_keyword(&self.feature_buffer[start..end])?;
            if self.warmup_windows_remaining > 0 {
                score = 0.0;
                self.warmup_windows_remaining -= 1;
            }
            batch_max = batch_max.max(score);
        }

        self.last_score = batch_max;
        Ok(batch_max)
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
    let shared_assets_dir = settings
        .root_dir
        .join(OPENWAKEWORD_CACHE_DIR)
        .join(OPENWAKEWORD_RELEASE_VERSION);
    Ok(WakewordPaths {
        model_path: root.join("model.onnx"),
        metadata_path: root.join("metadata.json"),
        melspectrogram_path: shared_assets_dir.join(OPENWAKEWORD_MELSPECTROGRAM_FILENAME),
        embedding_model_path: shared_assets_dir.join(OPENWAKEWORD_EMBEDDING_FILENAME),
        root,
        shared_assets_dir,
    })
}

pub async fn ensure_wakeword_model(settings: &WakewordSettings) -> anyhow::Result<WakewordPaths> {
    let paths = wakeword_paths(settings)?;
    tokio::fs::create_dir_all(&paths.root)
        .await
        .with_context(|| format!("failed to create {}", paths.root.display()))?;

    match settings.engine {
        WakewordEngine::Openwakeword => ensure_openwakeword_model(settings, &paths).await?,
        WakewordEngine::Onnx => ensure_legacy_onnx_model(&paths).await?,
    }

    write_metadata(settings, &paths).await?;
    Ok(paths)
}

async fn ensure_openwakeword_model(
    settings: &WakewordSettings,
    paths: &WakewordPaths,
) -> anyhow::Result<()> {
    ensure_openwakeword_shared_assets(settings, paths).await?;
    if tokio::fs::metadata(&paths.model_path).await.is_err() {
        install_openwakeword_stock_head(settings, paths, false).await?;
    }

    tokio::fs::metadata(&paths.model_path)
        .await
        .with_context(|| {
            format!(
                "wakeword model is missing at {}",
                paths.model_path.display()
            )
        })?;
    Ok(())
}

async fn ensure_legacy_onnx_model(paths: &WakewordPaths) -> anyhow::Result<()> {
    if tokio::fs::metadata(&paths.model_path).await.is_ok() {
        return Ok(());
    }

    bail!(
        "wakeword model {} is missing; provide your own ONNX file at that path or switch back to the default openwakeword engine",
        paths.model_path.display()
    );
}

async fn ensure_openwakeword_shared_assets(
    settings: &WakewordSettings,
    paths: &WakewordPaths,
) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(&paths.shared_assets_dir)
        .await
        .with_context(|| format!("failed to create {}", paths.shared_assets_dir.display()))?;
    ensure_openwakeword_asset(
        settings,
        OPENWAKEWORD_MELSPECTROGRAM_FILENAME,
        &paths.melspectrogram_path,
        false,
    )
    .await?;
    ensure_openwakeword_asset(
        settings,
        OPENWAKEWORD_EMBEDDING_FILENAME,
        &paths.embedding_model_path,
        false,
    )
    .await?;
    Ok(())
}

async fn install_openwakeword_stock_head(
    settings: &WakewordSettings,
    paths: &WakewordPaths,
    force: bool,
) -> anyhow::Result<()> {
    ensure_openwakeword_asset(
        settings,
        settings.stock_model.asset_filename(),
        &paths.model_path,
        force,
    )
    .await
}

async fn ensure_openwakeword_asset(
    settings: &WakewordSettings,
    asset_name: &str,
    destination: &Path,
    force: bool,
) -> anyhow::Result<()> {
    if !force && tokio::fs::metadata(destination).await.is_ok() {
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if let Some(source_dir) = settings.assets_dir.as_ref() {
        copy_openwakeword_asset(source_dir, asset_name, destination).await
    } else {
        download_openwakeword_asset(asset_name, destination).await
    }
}

async fn copy_openwakeword_asset(
    source_dir: &Path,
    asset_name: &str,
    destination: &Path,
) -> anyhow::Result<()> {
    let source = source_dir.join(asset_name);
    tokio::fs::metadata(&source).await.with_context(|| {
        format!(
            "openWakeWord asset {} is missing from configured assets_dir {}",
            asset_name,
            source_dir.display()
        )
    })?;
    tokio::fs::copy(&source, destination)
        .await
        .with_context(|| {
            format!(
                "failed to copy openWakeWord asset {} to {}",
                source.display(),
                destination.display()
            )
        })?;
    Ok(())
}

async fn download_openwakeword_asset(asset_name: &str, destination: &Path) -> anyhow::Result<()> {
    let url = format!("{OPENWAKEWORD_RELEASE_BASE_URL}/{asset_name}");
    let response = reqwest::get(&url)
        .await
        .with_context(|| format!("failed to download openWakeWord asset {url}"))?
        .error_for_status()
        .with_context(|| format!("openWakeWord asset request failed for {url}"))?;
    let body = response
        .bytes()
        .await
        .with_context(|| format!("failed to read openWakeWord asset body from {url}"))?;
    tokio::fs::write(destination, body)
        .await
        .with_context(|| format!("failed to write {}", destination.display()))?;
    Ok(())
}

pub async fn run_wakeword_loop<I>(config: WakewordRunConfig, injector: I) -> anyhow::Result<()>
where
    I: TextInjector + Clone + Send + 'static,
{
    let paths = ensure_wakeword_model(&config.settings).await?;
    match config.settings.engine {
        WakewordEngine::Onnx => {
            let scorer = OnnxWakeScorer::load(&paths.model_path, config.settings.frame)?;
            log_wakeword_ready(&config.settings, &paths);
            run_wakeword_loop_with_scorer(config, scorer, injector).await
        }
        WakewordEngine::Openwakeword => {
            let scorer = OpenWakewordPipelineScorer::load(&paths)?;
            log_wakeword_ready(&config.settings, &paths);
            run_wakeword_loop_with_scorer(config, scorer, injector).await
        }
    }
}

fn log_wakeword_ready(settings: &WakewordSettings, paths: &WakewordPaths) {
    eprintln!(
        "speaches-companion wakeword '{}' listening: engine={:?} model={} threshold={:.3} frame_ms={}",
        settings.name,
        settings.engine,
        paths.model_path.display(),
        settings.threshold,
        settings.frame.as_millis()
    );
}

#[derive(Debug, Clone)]
struct WakewordActivityReporter {
    state_file: Option<PathBuf>,
    refresh_command: Option<String>,
}

impl WakewordActivityReporter {
    fn from_env() -> Self {
        Self {
            state_file: env::var_os(WAKEWORD_ACTIVITY_STATE_FILE_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            refresh_command: env::var(WAKEWORD_ACTIVITY_REFRESH_COMMAND_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty()),
        }
    }

    fn set_recording(&self) {
        self.set_state("recording");
    }

    fn set_waiting(&self) {
        self.set_state("waiting");
    }

    fn clear(&self) {
        if let Some(state_file) = &self.state_file {
            if let Err(error) = fs::remove_file(state_file) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "speaches-companion wakeword failed to clear activity state {}: {error}",
                        state_file.display()
                    );
                }
            }
        }
        self.refresh();
    }

    fn set_state(&self, state: &str) {
        if let Some(state_file) = &self.state_file {
            if let Some(parent) = state_file.parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    eprintln!(
                        "speaches-companion wakeword failed to create activity state dir {}: {error}",
                        parent.display()
                    );
                    self.refresh();
                    return;
                }
            }
            if let Err(error) = fs::write(state_file, format!("{state}\n")) {
                eprintln!(
                    "speaches-companion wakeword failed to write activity state {}: {error}",
                    state_file.display()
                );
            }
        }
        self.refresh();
    }

    fn refresh(&self) {
        let Some(command) = &self.refresh_command else {
            return;
        };
        if let Err(error) = Command::new("/bin/sh").arg("-c").arg(command).status() {
            eprintln!("speaches-companion wakeword failed to refresh activity status: {error}");
        }
    }
}

pub async fn run_wakeword_loop_with_scorer<S, I>(
    config: WakewordRunConfig,
    scorer: S,
    injector: I,
) -> anyhow::Result<()>
where
    S: WakeScorer,
    I: TextInjector + Clone + Send + 'static,
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
    I: TextInjector + Clone + Send + 'static,
{
    let activity = WakewordActivityReporter::from_env();
    loop {
        let detection = wait_for_wake(&shared_pcm, detector, &config.settings).await?;
        eprintln!(
            "speaches-companion wakeword '{}' detected with score {:.3}",
            config.settings.name, detection.score
        );
        if config.notify_on_detect {
            notify_wakeword_detected(config.settings.name.clone(), detection.score);
        }
        if let Some(streaming) = config.streaming.as_ref() {
            run_streaming_wake_recording(
                &shared_pcm,
                &config,
                streaming.clone(),
                injector.clone(),
                &activity,
            )
            .await?;
            continue;
        }
        run_final_wake_recording(&shared_pcm, &config, injector.clone(), &activity).await?;
    }
}

async fn run_streaming_wake_recording<I>(
    shared_pcm: &SharedPcmBuffer,
    config: &WakewordRunConfig,
    streaming: WakewordStreamingConfig,
    injector: I,
    activity: &WakewordActivityReporter,
) -> anyhow::Result<()>
where
    I: TextInjector + Clone + Send + 'static,
{
    let mut controller = StreamingDictationController::new(streaming.transcriber, injector.clone())
        .with_listening_marker(streaming.listening_marker)
        .with_inline_partials(streaming.inline_partials)
        .with_partial_chunking_config(streaming.partial_chunking)
        .with_final_transcript(streaming.final_transcript)
        .with_append_space(config.append_space);

    activity.set_recording();
    if let Err(error) = controller.handle_hotkey(IpcCommand::HotkeyDown).await {
        activity.clear();
        return Err(error);
    }
    let saw_speech = wait_for_recording_silence(shared_pcm, &config.settings).await;
    activity.set_waiting();
    let stop_result = controller.handle_hotkey(IpcCommand::HotkeyUp).await;

    let result = async {
        let saw_speech = saw_speech?;
        let stop_response = stop_result?;
        if saw_speech
            && config.settings.press_enter
            && matches!(stop_response, DaemonResponse::Stopped)
        {
            injector.press_enter()?;
        }
        Ok(())
    }
    .await;
    activity.clear();
    result
}

async fn run_final_wake_recording<I>(
    shared_pcm: &SharedPcmBuffer,
    config: &WakewordRunConfig,
    injector: I,
    activity: &WakewordActivityReporter,
) -> anyhow::Result<()>
where
    I: TextInjector + Clone + Send + 'static,
{
    activity.set_recording();
    let pcm = record_until_silence(shared_pcm, &config.settings).await;
    activity.set_waiting();

    let result = async {
        let pcm = pcm?;
        if pcm.is_empty() {
            return Ok(());
        }
        let transcript = transcribe_wake_recording(config, &pcm).await?;
        if let Some(text) = format_transcript_for_injection(&transcript, config.append_space) {
            injector.inject_text(&text)?;
            if config.settings.press_enter {
                injector.press_enter()?;
            }
        }
        Ok(())
    }
    .await;
    activity.clear();
    result
}

fn notify_wakeword_detected(name: String, score: f32) {
    let _ = tokio::task::spawn_blocking(move || {
        let notifier = DesktopWakewordNotifier;
        if let Err(error) = notifier.notify_detected(&name, score) {
            eprintln!("speaches-companion wakeword notification failed: {error:#}");
        }
    });
}

pub async fn wait_for_wake<S>(
    shared_pcm: &SharedPcmBuffer,
    detector: &mut WakeDetector<S>,
    settings: &WakewordSettings,
) -> anyhow::Result<WakeDetection>
where
    S: WakeScorer,
{
    let mut cursor = snapshot_streaming_pcm(shared_pcm).await.bytes_seen;
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

async fn wait_for_recording_silence(
    shared_pcm: &SharedPcmBuffer,
    settings: &WakewordSettings,
) -> anyhow::Result<bool> {
    let started = Instant::now();
    let mut cursor = snapshot_streaming_pcm(shared_pcm).await.bytes_seen;
    let mut saw_speech = false;
    let mut last_speech = started;

    loop {
        sleep((settings.frame / 2).max(Duration::from_millis(10))).await;
        let snapshot = snapshot_streaming_pcm(shared_pcm).await;
        let buffer_start = snapshot.bytes_seen.saturating_sub(snapshot.pcm.len());
        let start = cursor.saturating_sub(buffer_start).min(snapshot.pcm.len());
        let new_pcm = &snapshot.pcm[start..];
        cursor = snapshot.bytes_seen;

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

    Ok(saw_speech)
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
        engine: settings.engine,
        stock_model: (settings.engine == WakewordEngine::Openwakeword)
            .then_some(settings.stock_model),
        threshold: settings.threshold,
        frame_ms: settings.frame.as_millis() as u64,
        model_path: paths.model_path.clone(),
        shared_assets_dir: (settings.engine == WakewordEngine::Openwakeword)
            .then_some(paths.shared_assets_dir.clone()),
    };
    let data = serde_json::to_vec_pretty(&metadata)?;
    tokio::fs::write(&paths.metadata_path, data)
        .await
        .with_context(|| format!("failed to write {}", paths.metadata_path.display()))
}

fn infer_keyword_frames(model: &TypedModel) -> anyhow::Result<usize> {
    let shape = model
        .input_fact(0)
        .context("openWakeWord keyword model has no inputs")?
        .shape
        .as_concrete()
        .context("openWakeWord keyword model requires a concrete input shape")?;
    if shape.len() != 3 {
        bail!(
            "openWakeWord keyword model input rank must be 3, got shape {:?}",
            shape
        );
    }
    if shape[0] != 1 || shape[2] != OPENWAKEWORD_EMBEDDING_DIM {
        bail!(
            "openWakeWord keyword model input must be [1, frames, {}], got {:?}",
            OPENWAKEWORD_EMBEDDING_DIM,
            shape
        );
    }
    Ok(shape[1])
}

fn first_output_as_slice<'a>(outputs: &'a TVec<TValue>, label: &str) -> anyhow::Result<&'a [f32]> {
    outputs
        .first()
        .with_context(|| format!("{label} produced no tensors"))?
        .try_as_plain()
        .with_context(|| format!("{label} is not a plain tensor"))?
        .as_slice::<f32>()
        .with_context(|| format!("{label} is not f32"))
}

fn trim_frame_buffer(buffer: &mut Vec<f32>, frame_width: usize, max_frames: usize) {
    let frames = frame_count(buffer, frame_width);
    if frames > max_frames {
        let drop_values = (frames - max_frames) * frame_width;
        buffer.drain(..drop_values);
    }
}

fn frame_count(buffer: &[f32], frame_width: usize) -> usize {
    buffer.len() / frame_width
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
    use crate::audio::{append_test_streaming_pcm, new_test_shared_pcm_buffer};
    use tempfile::tempdir;
    use tokio::time::Instant;

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

    #[derive(Debug, Default)]
    struct FakeOpenWakewordBackend {
        keyword_frames: usize,
        melspectrogram_calls: Vec<usize>,
        embedding_calls: usize,
        keyword_calls: usize,
        next_keyword_scores: Vec<f32>,
    }

    impl FakeOpenWakewordBackend {
        fn new() -> Self {
            Self {
                keyword_frames: 16,
                ..Self::default()
            }
        }
    }

    impl OpenWakewordBackend for FakeOpenWakewordBackend {
        fn keyword_frames(&self) -> usize {
            self.keyword_frames
        }

        fn run_melspectrogram(&mut self, samples: &[i16]) -> anyhow::Result<Vec<f32>> {
            self.melspectrogram_calls.push(samples.len());
            let frame_count = samples.len() / 160 - 3;
            Ok(vec![0.0; frame_count * OPENWAKEWORD_MEL_BINS])
        }

        fn run_embedding(&mut self, _mel_window: &[f32]) -> anyhow::Result<Vec<f32>> {
            self.embedding_calls += 1;
            Ok(vec![0.0; OPENWAKEWORD_EMBEDDING_DIM])
        }

        fn run_keyword(&mut self, _feature_window: &[f32]) -> anyhow::Result<f32> {
            self.keyword_calls += 1;
            Ok(if self.next_keyword_scores.is_empty() {
                0.0
            } else {
                self.next_keyword_scores.remove(0)
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

    #[tokio::test]
    async fn wait_for_wake_ignores_retained_audio_from_before_the_call() {
        let shared_pcm = new_test_shared_pcm_buffer(STT_SAMPLE_RATE, Duration::from_secs(5));
        append_test_streaming_pcm(&shared_pcm, &[0; 64]).await;

        let delayed_pcm = shared_pcm.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            append_test_streaming_pcm(&delayed_pcm, &[0; 64]).await;
        });

        let scorer = FakeScorer { scores: vec![0.9] };
        let mut detector = WakeDetector::new(scorer, 0.5, Duration::from_millis(1));
        let settings = WakewordSettings {
            name: "default".to_string(),
            engine: WakewordEngine::Openwakeword,
            stock_model: DEFAULT_OPENWAKEWORD_STOCK_MODEL,
            assets_dir: None,
            root_dir: PathBuf::from("/tmp/wakewords"),
            threshold: DEFAULT_WAKEWORD_THRESHOLD,
            frame: Duration::from_millis(1),
            silence_timeout: Duration::from_millis(DEFAULT_WAKEWORD_SILENCE_TIMEOUT_MS),
            activation_grace: Duration::from_millis(DEFAULT_WAKEWORD_ACTIVATION_GRACE_MS),
            max_recording: Duration::from_millis(DEFAULT_WAKEWORD_MAX_RECORDING_MS),
            press_enter: DEFAULT_WAKEWORD_PRESS_ENTER,
        };

        let started = Instant::now();
        let detection = wait_for_wake(&shared_pcm, &mut detector, &settings)
            .await
            .unwrap();

        assert_eq!(detection, WakeDetection { score: 0.9 });
        assert!(started.elapsed() >= Duration::from_millis(80));
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
    fn wakeword_paths_include_shared_openwakeword_assets() {
        let settings = WakewordSettings {
            name: "default".to_string(),
            engine: WakewordEngine::Openwakeword,
            stock_model: DEFAULT_OPENWAKEWORD_STOCK_MODEL,
            assets_dir: None,
            root_dir: PathBuf::from("/tmp/wakewords"),
            threshold: DEFAULT_WAKEWORD_THRESHOLD,
            frame: Duration::from_millis(DEFAULT_WAKEWORD_FRAME_MS),
            silence_timeout: Duration::from_millis(DEFAULT_WAKEWORD_SILENCE_TIMEOUT_MS),
            activation_grace: Duration::from_millis(DEFAULT_WAKEWORD_ACTIVATION_GRACE_MS),
            max_recording: Duration::from_millis(DEFAULT_WAKEWORD_MAX_RECORDING_MS),
            press_enter: DEFAULT_WAKEWORD_PRESS_ENTER,
        };

        let paths = wakeword_paths(&settings).unwrap();

        assert_eq!(
            paths.model_path,
            PathBuf::from("/tmp/wakewords/default/model.onnx")
        );
        assert_eq!(
            paths.shared_assets_dir,
            PathBuf::from("/tmp/wakewords/_openwakeword/v0.5.1")
        );
        assert_eq!(
            paths.melspectrogram_path,
            PathBuf::from("/tmp/wakewords/_openwakeword/v0.5.1/melspectrogram.onnx")
        );
    }

    #[test]
    fn openwakeword_scorer_accumulates_partial_frames() {
        let backend = FakeOpenWakewordBackend::new();
        let mut scorer = OpenWakewordPipelineScorer::with_backend(backend).unwrap();
        scorer.warmup_windows_remaining = 0;

        assert_eq!(scorer.score(&vec![0; 640]).unwrap(), 0.0);
        assert_eq!(scorer.backend.keyword_calls, 0);

        scorer.backend.next_keyword_scores = vec![0.6];
        assert_eq!(scorer.score(&vec![0; 640]).unwrap(), 0.6);
        assert_eq!(scorer.backend.keyword_calls, 1);
        assert_eq!(scorer.backend.embedding_calls, 1);
    }

    #[test]
    fn openwakeword_scorer_uses_max_score_for_multi_frame_batches() {
        let backend = FakeOpenWakewordBackend {
            next_keyword_scores: vec![0.2, 0.8],
            ..FakeOpenWakewordBackend::new()
        };
        let mut scorer = OpenWakewordPipelineScorer::with_backend(backend).unwrap();
        scorer.warmup_windows_remaining = 0;

        assert_eq!(
            scorer
                .score(&vec![0; OPENWAKEWORD_FRAME_SAMPLES * 2])
                .unwrap(),
            0.8
        );
        assert_eq!(scorer.backend.embedding_calls, 2);
        assert_eq!(scorer.backend.keyword_calls, 2);
    }

    #[test]
    fn openwakeword_scorer_suppresses_warmup_windows() {
        let backend = FakeOpenWakewordBackend {
            next_keyword_scores: vec![0.9; 6],
            ..FakeOpenWakewordBackend::new()
        };
        let mut scorer = OpenWakewordPipelineScorer::with_backend(backend).unwrap();

        for _ in 0..OPENWAKEWORD_WARMUP_WINDOWS {
            assert_eq!(
                scorer.score(&vec![0; OPENWAKEWORD_FRAME_SAMPLES]).unwrap(),
                0.0
            );
        }
        assert_eq!(
            scorer.score(&vec![0; OPENWAKEWORD_FRAME_SAMPLES]).unwrap(),
            0.9
        );
    }

    #[tokio::test]
    async fn openwakeword_assets_can_be_copied_from_local_dir() {
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let root_dir = dir.path().join("wakewords");
        std::fs::create_dir_all(&assets_dir).unwrap();
        std::fs::write(
            assets_dir.join(OPENWAKEWORD_MELSPECTROGRAM_FILENAME),
            b"mel",
        )
        .unwrap();
        std::fs::write(assets_dir.join(OPENWAKEWORD_EMBEDDING_FILENAME), b"embed").unwrap();
        std::fs::write(
            assets_dir.join(DEFAULT_OPENWAKEWORD_STOCK_MODEL.asset_filename()),
            b"head",
        )
        .unwrap();

        let settings = WakewordSettings {
            name: "default".to_string(),
            engine: WakewordEngine::Openwakeword,
            stock_model: DEFAULT_OPENWAKEWORD_STOCK_MODEL,
            assets_dir: Some(assets_dir.clone()),
            root_dir: root_dir.clone(),
            threshold: DEFAULT_WAKEWORD_THRESHOLD,
            frame: Duration::from_millis(DEFAULT_WAKEWORD_FRAME_MS),
            silence_timeout: Duration::from_millis(DEFAULT_WAKEWORD_SILENCE_TIMEOUT_MS),
            activation_grace: Duration::from_millis(DEFAULT_WAKEWORD_ACTIVATION_GRACE_MS),
            max_recording: Duration::from_millis(DEFAULT_WAKEWORD_MAX_RECORDING_MS),
            press_enter: DEFAULT_WAKEWORD_PRESS_ENTER,
        };

        let paths = ensure_wakeword_model(&settings).await.unwrap();

        assert_eq!(tokio::fs::read(&paths.model_path).await.unwrap(), b"head");
        assert_eq!(
            tokio::fs::read(&paths.melspectrogram_path).await.unwrap(),
            b"mel"
        );
        assert_eq!(
            tokio::fs::read(&paths.embedding_model_path).await.unwrap(),
            b"embed"
        );
        assert!(tokio::fs::metadata(&paths.metadata_path).await.is_ok());
    }

    #[tokio::test]
    async fn openwakeword_local_assets_are_required_when_assets_dir_is_configured() {
        let dir = tempdir().unwrap();
        let assets_dir = dir.path().join("assets");
        let root_dir = dir.path().join("wakewords");
        std::fs::create_dir_all(&assets_dir).unwrap();
        std::fs::write(assets_dir.join(OPENWAKEWORD_EMBEDDING_FILENAME), b"embed").unwrap();

        let settings = WakewordSettings {
            name: "default".to_string(),
            engine: WakewordEngine::Openwakeword,
            stock_model: DEFAULT_OPENWAKEWORD_STOCK_MODEL,
            assets_dir: Some(assets_dir.clone()),
            root_dir,
            threshold: DEFAULT_WAKEWORD_THRESHOLD,
            frame: Duration::from_millis(DEFAULT_WAKEWORD_FRAME_MS),
            silence_timeout: Duration::from_millis(DEFAULT_WAKEWORD_SILENCE_TIMEOUT_MS),
            activation_grace: Duration::from_millis(DEFAULT_WAKEWORD_ACTIVATION_GRACE_MS),
            max_recording: Duration::from_millis(DEFAULT_WAKEWORD_MAX_RECORDING_MS),
            press_enter: DEFAULT_WAKEWORD_PRESS_ENTER,
        };

        let error = ensure_wakeword_model(&settings).await.unwrap_err();
        assert!(format!("{error:#}").contains("assets_dir"));
    }
}
