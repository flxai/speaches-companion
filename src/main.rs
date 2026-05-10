use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{ArgAction, Args, Parser, Subcommand};
use speaches_companion::audio::{record_wav_with_pw_record, STT_SAMPLE_RATE};
use speaches_companion::config::{
    load_file_config, resolve_config, resolve_tts_config, ConfigInput, FileConfig, TtsConfig,
    TtsConfigInput,
};
use speaches_companion::daemon::run_daemon;
use speaches_companion::inject::{
    DesktopTextInjector, TextInjector,
    DEFAULT_PASTE_SETTLE_DELAY_MS as DEFAULT_INJECT_PASTE_SETTLE_DELAY_MS,
};
use speaches_companion::ipc::{default_socket_path, send_command, IpcCommand};
use speaches_companion::notification::{
    DesktopErrorNotifier, ErrorNotifier, NoopErrorNotifier, NoopTranscriptNotifier,
    HOTKEY_ERROR_SUMMARY, READ_ALOUD_ERROR_SUMMARY,
};
use speaches_companion::phase::PhaseResult;
use speaches_companion::realtime::{check_realtime, run_dictate_live, RealtimeTranscriber};
use speaches_companion::streaming::{
    FinalHttpTranscriber, LiveTranscriber, PartialChunkingConfig, StreamingDictationController,
};
use speaches_companion::stt::{transcribe_file, ResponseFormat, TranscribeOptions};
use speaches_companion::tts::{
    normalize_read_aloud_text, play_audio_file, selected_or_clipboard_text, synthesize_speech,
    write_speech_temp_file, SpeechOptions,
};
use speaches_companion::wakeword::{
    default_wakeword_root, run_wakeword_loop, OpenWakewordStockModel, WakewordEngine,
    WakewordRunConfig, WakewordSettings, WakewordStreamingConfig, DEFAULT_OPENWAKEWORD_STOCK_MODEL,
    DEFAULT_WAKEWORD_ACTIVATION_GRACE_MS, DEFAULT_WAKEWORD_FRAME_MS,
    DEFAULT_WAKEWORD_MAX_RECORDING_MS, DEFAULT_WAKEWORD_NAME, DEFAULT_WAKEWORD_PRESS_ENTER,
    DEFAULT_WAKEWORD_SILENCE_TIMEOUT_MS, DEFAULT_WAKEWORD_THRESHOLD,
};

const DEFAULT_LISTENING_MARKER: &str = "💬";
const DEFAULT_INJECT_DELAY_MICROSECS: u32 = 0;
const DEFAULT_PASTE_SETTLE_DELAY_MS: u64 = DEFAULT_INJECT_PASTE_SETTLE_DELAY_MS;
const DEFAULT_PASTE_IN_TERMINALS: bool = false;
const DEFAULT_LEADING_SILENCE_MS: u64 = 250;
const DEFAULT_PARTIAL_CHUNK_DELAY_MS: u64 = 80;
const DEFAULT_PARTIAL_CHUNK_MAX_DELAY_MS: u64 = 250;
const DEFAULT_PARTIAL_CHUNKING: bool = true;
const DEFAULT_PREROLL_MS: u64 = 750;
const DEFAULT_WAKEWORD_NOTIFY_ON_DETECT: bool = false;

#[derive(Debug, Parser)]
#[command(
    name = "speaches-companion",
    about = "Speaches Companion for typing what you speak and reading what you mark"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Daemon(DaemonArgs),
    DictateLive(DictateLiveArgs),
    Hotkey(HotkeyArgs),
    Inject(InjectArgs),
    ReadAloud(ReadAloudArgs),
    RealtimeCheck(RealtimeCheckArgs),
    Smoke(SmokeArgs),
    Transcribe(TranscribeArgs),
    Wakeword(WakewordArgs),
}

#[derive(Debug, Args)]
struct DaemonArgs {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    socket_path: Option<PathBuf>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    language: Option<String>,
    #[arg(long)]
    transcript_dir: Option<PathBuf>,
    #[cfg(feature = "debug-recordings")]
    #[arg(long)]
    record_dir: Option<PathBuf>,
    #[arg(long, conflicts_with = "no_stream_response")]
    stream_response: bool,
    #[arg(long)]
    no_stream_response: bool,
    #[arg(long, conflicts_with = "no_realtime_partials")]
    realtime_partials: bool,
    #[arg(long)]
    no_realtime_partials: bool,
    #[arg(long, conflicts_with = "no_final_pass")]
    final_pass: bool,
    #[arg(long)]
    no_final_pass: bool,
    #[arg(long)]
    listening_marker: Option<String>,
    #[arg(long)]
    no_listening_marker: bool,
    #[arg(long, conflicts_with = "no_inline_partials")]
    inline_partials: bool,
    #[arg(long)]
    no_inline_partials: bool,
    #[arg(long, conflicts_with = "no_partial_chunking")]
    partial_chunking: bool,
    #[arg(long)]
    no_partial_chunking: bool,
    #[arg(long)]
    partial_chunk_delay_ms: Option<u64>,
    #[arg(long)]
    partial_chunk_max_delay_ms: Option<u64>,
    #[arg(long, conflicts_with = "no_append_space")]
    append_space: bool,
    #[arg(long)]
    no_append_space: bool,
    #[arg(long)]
    inject_delay_microsecs: Option<u32>,
    #[arg(long)]
    paste_settle_delay_ms: Option<u64>,
    #[arg(long, conflicts_with = "no_paste_in_terminals")]
    paste_in_terminals: bool,
    #[arg(long)]
    no_paste_in_terminals: bool,
    #[arg(long)]
    leading_silence_ms: Option<u64>,
    #[arg(long)]
    preroll_ms: Option<u64>,
}

#[derive(Debug, Args)]
struct DictateLiveArgs {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    language: Option<String>,
    #[arg(long)]
    duration_seconds: Option<u64>,
    #[arg(long)]
    trace_path: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct WakewordArgs {
    name: Option<String>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, value_enum)]
    engine: Option<WakewordEngine>,
    #[arg(long, value_enum)]
    stock_model: Option<OpenWakewordStockModel>,
    #[arg(long)]
    assets_dir: Option<PathBuf>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    language: Option<String>,
    #[arg(long)]
    root_dir: Option<PathBuf>,
    #[arg(long)]
    threshold: Option<f32>,
    #[arg(long)]
    frame_ms: Option<u64>,
    #[arg(long)]
    silence_timeout_ms: Option<u64>,
    #[arg(long)]
    activation_grace_ms: Option<u64>,
    #[arg(long)]
    max_recording_ms: Option<u64>,
    #[arg(long, conflicts_with = "no_press_enter")]
    press_enter: bool,
    #[arg(long)]
    no_press_enter: bool,
    #[arg(long, conflicts_with = "no_append_space")]
    append_space: bool,
    #[arg(long)]
    no_append_space: bool,
    #[arg(long, conflicts_with = "no_notify_on_detect")]
    notify_on_detect: bool,
    #[arg(long)]
    no_notify_on_detect: bool,
    #[arg(long)]
    inject_delay_microsecs: Option<u32>,
}

#[derive(Debug, Args)]
struct HotkeyArgs {
    #[command(subcommand)]
    action: HotkeyAction,
    #[arg(long)]
    socket_path: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct InjectArgs {
    text: String,
    #[arg(long, default_value = "0")]
    delay_microsecs: u32,
    #[arg(long, default_value_t = DEFAULT_PASTE_SETTLE_DELAY_MS)]
    paste_settle_delay_ms: u64,
    #[arg(long, default_value_t = DEFAULT_PASTE_IN_TERMINALS)]
    paste_in_terminals: bool,
}

#[derive(Debug, Args)]
struct ReadAloudArgs {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    text: Option<String>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    voice: Option<String>,
    #[arg(long, value_parser = parse_tts_speed)]
    speed: Option<f32>,
    #[arg(long)]
    response_format: Option<String>,
    #[arg(long)]
    player: Option<String>,
    #[arg(long = "player-arg", action = ArgAction::Append, allow_hyphen_values = true)]
    player_args: Vec<String>,
}

#[derive(Debug, Args)]
struct RealtimeCheckArgs {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    language: Option<String>,
}

#[derive(Debug, Args)]
struct TranscribeArgs {
    audio: PathBuf,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value = "json")]
    response_format: ResponseFormat,
    #[arg(long)]
    language: Option<String>,
    #[arg(long)]
    prompt: Option<String>,
    #[arg(long)]
    hotwords: Option<String>,
    #[arg(long)]
    with_timestamps: bool,
    #[arg(long)]
    stream: bool,
}

#[derive(Debug, Args)]
struct SmokeArgs {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, default_value = "3")]
    record_seconds: u64,
    #[arg(
        long,
        default_value = "target/speaches-companion-smoke/speaches-mic.wav"
    )]
    record_output: PathBuf,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value = "text")]
    response_format: ResponseFormat,
    #[arg(long)]
    language: Option<String>,
}

#[derive(Debug, Subcommand)]
enum HotkeyAction {
    Down,
    Up,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Daemon(args) => run_daemon_command(args).await,
        Command::DictateLive(args) => run_dictate_live_command(args).await,
        Command::Hotkey(args) => run_hotkey_command(args).await,
        Command::Inject(args) => run_inject_command(args).await,
        Command::ReadAloud(args) => run_read_aloud_command(args).await,
        Command::RealtimeCheck(args) => run_realtime_check_command(args).await,
        Command::Smoke(args) => run_smoke_command(args).await,
        Command::Transcribe(args) => run_transcribe_command(args).await,
        Command::Wakeword(args) => run_wakeword_command(args).await,
    }
}

async fn run_daemon_command(args: DaemonArgs) -> ExitCode {
    let file_config = match load_command_file_config(args.config.clone()) {
        Ok(file_config) => file_config,
        Err(exit_code) => return exit_code,
    };
    let daemon_settings = resolve_daemon_settings(&args, &file_config);
    let socket_path = args.socket_path.unwrap_or_else(default_socket_path);
    if let Some(transcript_dir) = daemon_settings.transcript_dir.as_ref() {
        eprintln!(
            "speaches-companion preserving transcripts in {}",
            transcript_dir.display()
        );
    }
    #[cfg(feature = "debug-recordings")]
    if let Some(record_dir) = daemon_settings.record_dir.as_ref() {
        eprintln!(
            "speaches-companion preserving MP3 recordings in {}",
            record_dir.display()
        );
    }
    let config = resolve_config(stt_config_input(
        args.base_url,
        args.model,
        args.language,
        &file_config,
    ));
    if daemon_settings.realtime_partials {
        let transcriber = RealtimeTranscriber::new(config.base_url, config.model, config.language)
            .with_preroll(Duration::from_millis(daemon_settings.preroll_ms))
            .with_final_pass(daemon_settings.final_pass);
        #[cfg(feature = "debug-recordings")]
        let transcriber = transcriber.with_record_dir(daemon_settings.record_dir.clone());
        match prepare_realtime_daemon_transcriber(transcriber).await {
            Ok(transcriber) => {
                run_streaming_daemon(socket_path, daemon_settings, transcriber).await
            }
            Err(error) => {
                eprintln!("speaches-companion realtime startup failed: {error:#}");
                ExitCode::from(1)
            }
        }
    } else {
        let transcriber = FinalHttpTranscriber::new(
            config.base_url,
            TranscribeOptions {
                model: config.model,
                response_format: ResponseFormat::Text,
                language: config.language,
                prompt: None,
                hotwords: None,
                without_timestamps: true,
                stream: daemon_settings.stream_response,
            },
        )
        .with_leading_silence(Duration::from_millis(daemon_settings.leading_silence_ms))
        .with_preroll(Duration::from_millis(daemon_settings.preroll_ms))
        .with_transcript_dir(daemon_settings.transcript_dir.clone());
        #[cfg(feature = "debug-recordings")]
        let transcriber = transcriber.with_record_dir(daemon_settings.record_dir.clone());
        eprintln!("speaches-companion starting continuous audio capture...");
        if let Err(error) = transcriber.prepare_capture().await {
            eprintln!("speaches-companion failed to start continuous audio capture: {error:#}");
            return ExitCode::from(1);
        }
        eprintln!("speaches-companion continuous audio capture ready");
        eprintln!("speaches-companion warming transcription backend...");
        match transcriber.warm_up_transcription().await {
            Ok(()) => eprintln!("speaches-companion transcription backend ready"),
            Err(error) => eprintln!("speaches-companion transcription warmup failed: {error:#}"),
        }
        run_streaming_daemon(socket_path, daemon_settings, transcriber).await
    }
}

async fn run_wakeword_command(args: WakewordArgs) -> ExitCode {
    let file_config = match load_command_file_config(args.config.clone()) {
        Ok(file_config) => file_config,
        Err(exit_code) => return exit_code,
    };
    let config = resolve_config(stt_config_input(
        args.base_url.clone(),
        args.model.clone(),
        args.language.clone(),
        &file_config,
    ));
    let settings = resolve_wakeword_settings(&args, &file_config);
    let streaming = match build_wakeword_streaming_config(&config, &file_config).await {
        Ok(streaming) => streaming,
        Err(error) => {
            eprintln!("speaches-companion wakeword realtime startup failed: {error:#}");
            return ExitCode::from(1);
        }
    };
    let run_config = WakewordRunConfig {
        settings,
        base_url: config.base_url,
        stt_options: TranscribeOptions {
            model: config.model,
            response_format: ResponseFormat::Text,
            language: config.language,
            prompt: None,
            hotwords: None,
            without_timestamps: true,
            stream: false,
        },
        append_space: resolve_wakeword_append_space(&args, &file_config),
        notify_on_detect: resolve_wakeword_notify_on_detect(&args, &file_config),
        streaming,
    };
    let injector = DesktopTextInjector::new_with_options(
        resolve_wakeword_inject_delay(&args, &file_config),
        resolve_wakeword_paste_settle_delay_ms(&file_config),
        resolve_wakeword_paste_in_terminals(&file_config),
    );
    match run_wakeword_loop(run_config, injector).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("speaches-companion wakeword failed: {error:#}");
            ExitCode::from(1)
        }
    }
}

#[async_trait::async_trait]
trait RealtimeDaemonStartup: Sized + Sync {
    async fn prepare_capture(&self) -> anyhow::Result<()>;
    async fn warm_up(&self) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
impl RealtimeDaemonStartup for RealtimeTranscriber {
    async fn prepare_capture(&self) -> anyhow::Result<()> {
        RealtimeTranscriber::prepare_capture(self).await
    }

    async fn warm_up(&self) -> anyhow::Result<()> {
        RealtimeTranscriber::warm_up(self).await
    }
}

async fn prepare_realtime_daemon_transcriber<T>(transcriber: T) -> anyhow::Result<T>
where
    T: RealtimeDaemonStartup,
{
    eprintln!("speaches-companion starting continuous audio capture...");
    transcriber
        .prepare_capture()
        .await
        .context("failed to start continuous audio capture")?;
    eprintln!("speaches-companion continuous audio capture ready");
    eprintln!("speaches-companion warming realtime transcription backend...");
    transcriber
        .warm_up()
        .await
        .context("realtime transcription backend is unavailable")?;
    eprintln!("speaches-companion realtime transcription backend ready");
    Ok(transcriber)
}

async fn build_wakeword_streaming_config(
    config: &speaches_companion::config::DictateLiveConfig,
    file_config: &FileConfig,
) -> anyhow::Result<Option<WakewordStreamingConfig>> {
    if !file_config.dictation.realtime_partials.unwrap_or(false) {
        return Ok(None);
    }

    let final_pass = file_config.dictation.final_pass.unwrap_or(true);
    let transcriber = RealtimeTranscriber::new(
        config.base_url.clone(),
        config.model.clone(),
        config.language.clone(),
    )
    // Wake detection uses a separate 16 kHz capture; do not feed the wake phrase
    // from realtime pre-roll into the dictated command.
    .with_preroll(Duration::ZERO)
    .with_final_pass(final_pass);
    #[cfg(feature = "debug-recordings")]
    let transcriber = transcriber.with_record_dir(file_config.dictation.record_dir.clone());
    let transcriber = prepare_realtime_daemon_transcriber(transcriber).await?;

    Ok(Some(WakewordStreamingConfig {
        transcriber,
        listening_marker: file_config
            .dictation
            .listening_marker
            .clone()
            .or_else(|| Some(DEFAULT_LISTENING_MARKER.to_string()))
            .and_then(non_empty_string),
        inline_partials: file_config.dictation.inline_partials.unwrap_or(true),
        partial_chunking: PartialChunkingConfig {
            enabled: file_config
                .dictation
                .partial_chunking
                .unwrap_or(DEFAULT_PARTIAL_CHUNKING),
            delay: Duration::from_millis(
                file_config
                    .dictation
                    .partial_chunk_delay_ms
                    .unwrap_or(DEFAULT_PARTIAL_CHUNK_DELAY_MS),
            ),
            max_delay: Duration::from_millis(
                file_config
                    .dictation
                    .partial_chunk_max_delay_ms
                    .unwrap_or(DEFAULT_PARTIAL_CHUNK_MAX_DELAY_MS),
            ),
        },
        final_transcript: final_pass,
    }))
}

async fn run_streaming_daemon<L>(
    socket_path: PathBuf,
    daemon_settings: DaemonSettings,
    transcriber: L,
) -> ExitCode
where
    L: LiveTranscriber + 'static,
{
    let injector = DesktopTextInjector::new_with_options(
        daemon_settings.inject_delay_microsecs,
        daemon_settings.paste_settle_delay_ms,
        daemon_settings.paste_in_terminals,
    );
    let controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        NoopTranscriptNotifier,
        NoopErrorNotifier,
    )
    .with_listening_marker(daemon_settings.listening_marker)
    .with_inline_partials(daemon_settings.inline_partials)
    .with_partial_chunking_config(PartialChunkingConfig {
        enabled: daemon_settings.partial_chunking,
        delay: Duration::from_millis(daemon_settings.partial_chunk_delay_ms),
        max_delay: Duration::from_millis(daemon_settings.partial_chunk_max_delay_ms),
    })
    .with_final_transcript(daemon_settings.final_pass || !daemon_settings.realtime_partials)
    .with_append_space(daemon_settings.append_space);

    eprintln!(
        "speaches-companion daemon listening on {}",
        socket_path.display()
    );
    match run_daemon(&socket_path, controller).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("speaches-companion daemon failed: {error:#}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonSettings {
    transcript_dir: Option<PathBuf>,
    #[cfg(feature = "debug-recordings")]
    record_dir: Option<PathBuf>,
    stream_response: bool,
    realtime_partials: bool,
    final_pass: bool,
    listening_marker: Option<String>,
    inline_partials: bool,
    partial_chunking: bool,
    partial_chunk_delay_ms: u64,
    partial_chunk_max_delay_ms: u64,
    append_space: bool,
    inject_delay_microsecs: u32,
    paste_settle_delay_ms: u64,
    paste_in_terminals: bool,
    leading_silence_ms: u64,
    preroll_ms: u64,
}

fn resolve_daemon_settings(args: &DaemonArgs, file_config: &FileConfig) -> DaemonSettings {
    DaemonSettings {
        transcript_dir: args
            .transcript_dir
            .clone()
            .or_else(|| file_config.dictation.transcript_dir.clone()),
        #[cfg(feature = "debug-recordings")]
        record_dir: args
            .record_dir
            .clone()
            .or_else(|| file_config.dictation.record_dir.clone()),
        stream_response: resolve_stream_response(args, file_config),
        realtime_partials: resolve_realtime_partials(args, file_config),
        final_pass: resolve_final_pass(args, file_config),
        listening_marker: resolve_listening_marker(args, file_config),
        inline_partials: resolve_inline_partials(args, file_config),
        partial_chunking: resolve_partial_chunking(args, file_config),
        partial_chunk_delay_ms: args
            .partial_chunk_delay_ms
            .or(file_config.dictation.partial_chunk_delay_ms)
            .unwrap_or(DEFAULT_PARTIAL_CHUNK_DELAY_MS),
        partial_chunk_max_delay_ms: args
            .partial_chunk_max_delay_ms
            .or(file_config.dictation.partial_chunk_max_delay_ms)
            .unwrap_or(DEFAULT_PARTIAL_CHUNK_MAX_DELAY_MS),
        append_space: resolve_append_space(args, file_config),
        inject_delay_microsecs: args
            .inject_delay_microsecs
            .or(file_config.dictation.inject_delay_microsecs)
            .unwrap_or(DEFAULT_INJECT_DELAY_MICROSECS),
        paste_settle_delay_ms: args
            .paste_settle_delay_ms
            .or(file_config.dictation.paste_settle_delay_ms)
            .unwrap_or(DEFAULT_PASTE_SETTLE_DELAY_MS),
        paste_in_terminals: resolve_paste_in_terminals(args, file_config),
        leading_silence_ms: args
            .leading_silence_ms
            .or(file_config.dictation.leading_silence_ms)
            .unwrap_or(DEFAULT_LEADING_SILENCE_MS),
        preroll_ms: args
            .preroll_ms
            .or(file_config.dictation.preroll_ms)
            .unwrap_or(DEFAULT_PREROLL_MS),
    }
}

fn resolve_wakeword_settings(args: &WakewordArgs, file_config: &FileConfig) -> WakewordSettings {
    WakewordSettings {
        name: args
            .name
            .clone()
            .or_else(|| file_config.wakeword.name.clone())
            .unwrap_or_else(|| DEFAULT_WAKEWORD_NAME.to_owned()),
        engine: args
            .engine
            .or(file_config.wakeword.engine)
            .unwrap_or(WakewordEngine::Openwakeword),
        stock_model: args
            .stock_model
            .or(file_config.wakeword.stock_model)
            .unwrap_or(DEFAULT_OPENWAKEWORD_STOCK_MODEL),
        assets_dir: args
            .assets_dir
            .clone()
            .or_else(|| file_config.wakeword.assets_dir.clone()),
        root_dir: args
            .root_dir
            .clone()
            .or_else(|| file_config.wakeword.root_dir.clone())
            .unwrap_or_else(default_wakeword_root),
        threshold: args
            .threshold
            .or(file_config.wakeword.threshold)
            .unwrap_or(DEFAULT_WAKEWORD_THRESHOLD),
        frame: Duration::from_millis(
            args.frame_ms
                .or(file_config.wakeword.frame_ms)
                .unwrap_or(DEFAULT_WAKEWORD_FRAME_MS),
        ),
        silence_timeout: Duration::from_millis(
            args.silence_timeout_ms
                .or(file_config.wakeword.silence_timeout_ms)
                .unwrap_or(DEFAULT_WAKEWORD_SILENCE_TIMEOUT_MS),
        ),
        activation_grace: Duration::from_millis(
            args.activation_grace_ms
                .or(file_config.wakeword.activation_grace_ms)
                .unwrap_or(DEFAULT_WAKEWORD_ACTIVATION_GRACE_MS),
        ),
        max_recording: Duration::from_millis(
            args.max_recording_ms
                .or(file_config.wakeword.max_recording_ms)
                .unwrap_or(DEFAULT_WAKEWORD_MAX_RECORDING_MS),
        ),
        press_enter: if args.press_enter {
            true
        } else if args.no_press_enter {
            false
        } else {
            file_config
                .wakeword
                .press_enter
                .unwrap_or(DEFAULT_WAKEWORD_PRESS_ENTER)
        },
    }
}

fn resolve_realtime_partials(args: &DaemonArgs, file_config: &FileConfig) -> bool {
    if args.realtime_partials {
        true
    } else if args.no_realtime_partials {
        false
    } else {
        file_config.dictation.realtime_partials.unwrap_or(false)
    }
}

fn resolve_final_pass(args: &DaemonArgs, file_config: &FileConfig) -> bool {
    if args.final_pass {
        true
    } else if args.no_final_pass {
        false
    } else {
        file_config.dictation.final_pass.unwrap_or(true)
    }
}

fn resolve_listening_marker(args: &DaemonArgs, file_config: &FileConfig) -> Option<String> {
    if args.no_listening_marker {
        return None;
    }

    args.listening_marker
        .clone()
        .or_else(|| file_config.dictation.listening_marker.clone())
        .or_else(|| Some(DEFAULT_LISTENING_MARKER.to_string()))
        .and_then(non_empty_string)
}

fn resolve_stream_response(args: &DaemonArgs, file_config: &FileConfig) -> bool {
    if args.stream_response {
        true
    } else if args.no_stream_response {
        false
    } else {
        file_config.dictation.stream_response.unwrap_or(false)
    }
}

fn resolve_inline_partials(args: &DaemonArgs, file_config: &FileConfig) -> bool {
    if args.inline_partials {
        true
    } else if args.no_inline_partials {
        false
    } else {
        file_config.dictation.inline_partials.unwrap_or(true)
    }
}

fn resolve_partial_chunking(args: &DaemonArgs, file_config: &FileConfig) -> bool {
    if args.partial_chunking {
        true
    } else if args.no_partial_chunking {
        false
    } else {
        file_config
            .dictation
            .partial_chunking
            .unwrap_or(DEFAULT_PARTIAL_CHUNKING)
    }
}

fn resolve_append_space(args: &DaemonArgs, file_config: &FileConfig) -> bool {
    if args.append_space {
        true
    } else if args.no_append_space {
        false
    } else {
        file_config.dictation.append_space.unwrap_or(true)
    }
}

fn resolve_wakeword_append_space(args: &WakewordArgs, file_config: &FileConfig) -> bool {
    if args.append_space {
        true
    } else if args.no_append_space {
        false
    } else {
        file_config.dictation.append_space.unwrap_or(true)
    }
}

fn resolve_wakeword_notify_on_detect(args: &WakewordArgs, file_config: &FileConfig) -> bool {
    if args.notify_on_detect {
        true
    } else if args.no_notify_on_detect {
        false
    } else {
        file_config
            .wakeword
            .notify_on_detect
            .unwrap_or(DEFAULT_WAKEWORD_NOTIFY_ON_DETECT)
    }
}

fn resolve_wakeword_inject_delay(args: &WakewordArgs, file_config: &FileConfig) -> u32 {
    args.inject_delay_microsecs
        .or(file_config.dictation.inject_delay_microsecs)
        .unwrap_or(DEFAULT_INJECT_DELAY_MICROSECS)
}

fn resolve_wakeword_paste_settle_delay_ms(file_config: &FileConfig) -> u64 {
    file_config
        .dictation
        .paste_settle_delay_ms
        .unwrap_or(DEFAULT_PASTE_SETTLE_DELAY_MS)
}

fn resolve_wakeword_paste_in_terminals(file_config: &FileConfig) -> bool {
    file_config
        .dictation
        .paste_in_terminals
        .unwrap_or(DEFAULT_PASTE_IN_TERMINALS)
}

fn resolve_paste_in_terminals(args: &DaemonArgs, file_config: &FileConfig) -> bool {
    if args.paste_in_terminals {
        true
    } else if args.no_paste_in_terminals {
        false
    } else {
        file_config
            .dictation
            .paste_in_terminals
            .unwrap_or(DEFAULT_PASTE_IN_TERMINALS)
    }
}

async fn run_hotkey_command(args: HotkeyArgs) -> ExitCode {
    run_hotkey_command_with_notifier(args, DesktopErrorNotifier).await
}

async fn run_hotkey_command_with_notifier<N>(args: HotkeyArgs, notifier: N) -> ExitCode
where
    N: ErrorNotifier,
{
    let socket_path = args.socket_path.unwrap_or_else(default_socket_path);
    let command = match args.action {
        HotkeyAction::Down => IpcCommand::HotkeyDown,
        HotkeyAction::Up => IpcCommand::HotkeyUp,
    };

    match send_command(&socket_path, command).await {
        Ok(response) => {
            if !response.is_empty() {
                eprintln!("{response}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("speaches-companion hotkey failed: {error:#}");
            notify_hotkey_failure(&notifier, &error);
            ExitCode::from(1)
        }
    }
}

fn notify_hotkey_failure<N>(notifier: &N, error: &anyhow::Error)
where
    N: ErrorNotifier,
{
    let body = format!("{error:#}");
    if let Err(notify_error) = notifier.notify_error(HOTKEY_ERROR_SUMMARY, &body) {
        eprintln!("speaches-companion notification failed: {notify_error:#}");
    }
}

async fn run_inject_command(args: InjectArgs) -> ExitCode {
    let injector = DesktopTextInjector::new_with_options(
        args.delay_microsecs,
        args.paste_settle_delay_ms,
        args.paste_in_terminals,
    );
    match injector.inject_text(&args.text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("inject failed: {error:#}");
            ExitCode::from(1)
        }
    }
}

async fn run_read_aloud_command(args: ReadAloudArgs) -> ExitCode {
    run_read_aloud_command_with_notifier(args, DesktopErrorNotifier).await
}

async fn run_read_aloud_command_with_notifier<N>(args: ReadAloudArgs, notifier: N) -> ExitCode
where
    N: ErrorNotifier,
{
    let file_config = match load_command_file_config(args.config.clone()) {
        Ok(file_config) => file_config,
        Err(exit_code) => return exit_code,
    };
    let cli_tts_config = TtsCliConfigInput {
        base_url: args.base_url,
        model: args.model,
        voice: args.voice,
        speed: args.speed,
        response_format: args.response_format,
        player: args.player,
        player_args: args.player_args,
    };
    let config = resolve_tts_config(tts_config_input(cli_tts_config, &file_config));
    let result = read_aloud(args.text, config).await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("read-aloud failed: {error:#}");
            notify_read_aloud_failure(&notifier, &error);
            ExitCode::from(1)
        }
    }
}

async fn read_aloud(text: Option<String>, config: TtsConfig) -> anyhow::Result<()> {
    let text = match text {
        Some(text) => normalize_read_aloud_text(&text).context("--text is empty")?,
        None => selected_or_clipboard_text().await?,
    };
    let options = SpeechOptions {
        model: config.model,
        voice: config.voice,
        speed: config.speed,
        response_format: config.response_format.clone(),
    };
    let audio = synthesize_speech(&config.base_url, &text, &options).await?;
    let audio_path = write_speech_temp_file(&audio, &config.response_format).await?;
    let play_result = play_audio_file(&audio_path, &config.player, &config.player_args).await;
    if let Err(error) = tokio::fs::remove_file(&audio_path).await {
        eprintln!(
            "speaches-companion failed to remove temporary speech audio {}: {error:#}",
            audio_path.display()
        );
    }
    play_result
}

fn notify_read_aloud_failure<N>(notifier: &N, error: &anyhow::Error)
where
    N: ErrorNotifier,
{
    let body = format!("{error:#}");
    if let Err(notify_error) = notifier.notify_error(READ_ALOUD_ERROR_SUMMARY, &body) {
        eprintln!("speaches-companion notification failed: {notify_error:#}");
    }
}

async fn run_realtime_check_command(args: RealtimeCheckArgs) -> ExitCode {
    let file_config = match load_command_file_config(args.config.clone()) {
        Ok(file_config) => file_config,
        Err(exit_code) => return exit_code,
    };
    let config = resolve_config(stt_config_input(
        args.base_url,
        args.model,
        args.language,
        &file_config,
    ));

    eprintln!(
        "speaches-companion checking Speaches health at {}",
        config.base_url
    );
    match check_realtime(&config.base_url, &config.model, config.language.as_deref()).await {
        Ok(()) => {
            eprintln!("speaches-companion realtime websocket ready");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("speaches-companion realtime check failed: {error:#}");
            ExitCode::from(1)
        }
    }
}

fn env_or(primary: &str, fallback: &str) -> Option<String> {
    std::env::var(primary)
        .ok()
        .or_else(|| std::env::var(fallback).ok())
}

fn env_args_or(primary: &str, fallback: &str) -> Option<Vec<String>> {
    env_or(primary, fallback)
        .map(|value| {
            value
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|args| !args.is_empty())
}

fn env_speed_or(primary: &str, fallback: &str) -> Option<f32> {
    env_or(primary, fallback).and_then(|value| parse_tts_speed(&value).ok())
}

fn parse_tts_speed(value: &str) -> Result<f32, String> {
    let speed = value
        .parse::<f32>()
        .map_err(|error| format!("invalid TTS speed {value:?}: {error}"))?;
    if speed.is_finite() && speed > 0.0 {
        Ok(speed)
    } else {
        Err("TTS speed must be a positive finite number".to_string())
    }
}

fn load_command_file_config(config_path: Option<PathBuf>) -> Result<FileConfig, ExitCode> {
    load_file_config(config_path)
        .map(|loaded| loaded.config)
        .map_err(|error| {
            eprintln!("config failed: {error:#}");
            ExitCode::from(1)
        })
}

fn stt_config_input(
    cli_base_url: Option<String>,
    cli_model: Option<String>,
    cli_language: Option<String>,
    file_config: &FileConfig,
) -> ConfigInput {
    ConfigInput {
        cli_base_url,
        cli_model,
        cli_language,
        env_base_url: std::env::var("SPEACHES_BASE_URL").ok(),
        env_model: std::env::var("SPEACHES_COMPANION_MODEL")
            .ok()
            .or_else(|| std::env::var("SPEACHES_STT_MODEL").ok()),
        env_language: std::env::var("SPEACHES_COMPANION_LANGUAGE").ok(),
        file_base_url: file_config.speaches.base_url.clone(),
        file_model: file_config.stt.model.clone(),
        file_language: file_config.stt.language.clone(),
        ..ConfigInput::default()
    }
}

struct TtsCliConfigInput {
    base_url: Option<String>,
    model: Option<String>,
    voice: Option<String>,
    speed: Option<f32>,
    response_format: Option<String>,
    player: Option<String>,
    player_args: Vec<String>,
}

fn tts_config_input(cli: TtsCliConfigInput, file_config: &FileConfig) -> TtsConfigInput {
    TtsConfigInput {
        cli_base_url: cli.base_url,
        cli_model: cli.model,
        cli_voice: cli.voice,
        cli_speed: cli.speed,
        cli_response_format: cli.response_format,
        cli_player: cli.player,
        cli_player_args: cli.player_args,
        env_base_url: std::env::var("SPEACHES_BASE_URL").ok(),
        env_model: env_or("SPEACHES_COMPANION_TTS_MODEL", "SPEACHES_TTS_MODEL"),
        env_voice: env_or("SPEACHES_COMPANION_TTS_VOICE", "SPEACHES_TTS_VOICE"),
        env_speed: env_speed_or("SPEACHES_COMPANION_TTS_SPEED", "SPEACHES_TTS_SPEED"),
        env_response_format: env_or(
            "SPEACHES_COMPANION_TTS_RESPONSE_FORMAT",
            "SPEACHES_TTS_RESPONSE_FORMAT",
        ),
        env_player: env_or("SPEACHES_COMPANION_TTS_PLAYER", "SPEACHES_TTS_PLAYER"),
        env_player_args: env_args_or(
            "SPEACHES_COMPANION_TTS_PLAYER_ARGS",
            "SPEACHES_TTS_PLAYER_ARGS",
        ),
        file_base_url: file_config.speaches.base_url.clone(),
        file_model: file_config.tts.model.clone(),
        file_voice: file_config.tts.voice.clone(),
        file_speed: file_config.tts.speed,
        file_response_format: file_config.tts.response_format.clone(),
        file_player: file_config.tts.player.clone(),
        file_player_args: file_config.tts.player_args.clone(),
    }
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

async fn run_transcribe_command(args: TranscribeArgs) -> ExitCode {
    let file_config = match load_command_file_config(args.config.clone()) {
        Ok(file_config) => file_config,
        Err(exit_code) => return exit_code,
    };
    let config = resolve_config(stt_config_input(
        args.base_url,
        args.model,
        args.language.clone(),
        &file_config,
    ));
    let options = TranscribeOptions {
        model: config.model,
        response_format: args.response_format,
        language: config.language,
        prompt: args.prompt,
        hotwords: args.hotwords,
        without_timestamps: !args.with_timestamps,
        stream: args.stream,
    };

    match transcribe_file(&config.base_url, &args.audio, &options).await {
        Ok(response) => {
            println!("{response}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("transcribe failed: {error:#}");
            ExitCode::from(1)
        }
    }
}

async fn run_smoke_command(args: SmokeArgs) -> ExitCode {
    let duration = std::time::Duration::from_secs(args.record_seconds.max(1));
    eprintln!(
        "recording microphone for {}s to {}",
        duration.as_secs(),
        args.record_output.display()
    );
    if let Err(error) =
        record_wav_with_pw_record(&args.record_output, duration, STT_SAMPLE_RATE).await
    {
        eprintln!("record failed: {error:#}");
        return ExitCode::from(1);
    }

    let transcribe_args = TranscribeArgs {
        audio: args.record_output,
        config: args.config,
        base_url: args.base_url,
        model: args.model,
        response_format: args.response_format,
        language: args.language,
        prompt: None,
        hotwords: None,
        with_timestamps: false,
        stream: false,
    };
    run_transcribe_command(transcribe_args).await
}

async fn run_dictate_live_command(args: DictateLiveArgs) -> ExitCode {
    let file_config = match load_command_file_config(args.config.clone()) {
        Ok(file_config) => file_config,
        Err(exit_code) => return exit_code,
    };
    let config = resolve_config(ConfigInput {
        cli_duration_seconds: args.duration_seconds,
        cli_trace_path: args.trace_path,
        ..stt_config_input(args.base_url, args.model, args.language, &file_config)
    });

    match run_dictate_live(config).await {
        Ok(outcome) => {
            eprintln!(
                "phase0 result={:?} trace={} audio_bytes={}",
                outcome.result,
                outcome.trace_path.display(),
                outcome.total_audio_bytes
            );
            match outcome.result {
                PhaseResult::Passed => ExitCode::SUCCESS,
                PhaseResult::Blocked => ExitCode::from(2),
                PhaseResult::Failed => ExitCode::from(1),
                PhaseResult::Incomplete => ExitCode::from(3),
            }
        }
        Err(error) => {
            eprintln!("dictate-live failed: {error:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use speaches_companion::config::{DictationFileConfig, WakewordFileConfig};

    #[tokio::test]
    async fn hotkey_connection_error_sends_desktop_error_notification() {
        let socket_path = std::env::temp_dir().join(format!(
            "speaches-companion-missing-hotkey-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path);
        let notifier = FakeErrorNotifier::default();
        let notifications = notifier.notifications.clone();

        let exit_code = run_hotkey_command_with_notifier(
            HotkeyArgs {
                action: HotkeyAction::Down,
                socket_path: Some(socket_path.clone()),
            },
            notifier,
        )
        .await;

        assert_eq!(exit_code, ExitCode::from(1));
        let notifications = notifications.lock().unwrap();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].0, HOTKEY_ERROR_SUMMARY);
        assert!(notifications[0]
            .1
            .contains("failed to connect to speaches-companion daemon"));
        assert!(notifications[0]
            .1
            .contains(&socket_path.display().to_string()));
    }

    #[test]
    fn daemon_defaults_insert_marker_and_live_partials() {
        let args = parse_daemon_args(["speaches-companion", "daemon"]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());

        assert!(!settings.realtime_partials);
        assert_eq!(
            settings.listening_marker,
            Some(DEFAULT_LISTENING_MARKER.to_string())
        );
        assert!(settings.inline_partials);
        assert!(settings.partial_chunking);
        assert_eq!(
            settings.partial_chunk_delay_ms,
            DEFAULT_PARTIAL_CHUNK_DELAY_MS
        );
        assert_eq!(
            settings.partial_chunk_max_delay_ms,
            DEFAULT_PARTIAL_CHUNK_MAX_DELAY_MS
        );
        assert_eq!(
            settings.paste_settle_delay_ms,
            DEFAULT_PASTE_SETTLE_DELAY_MS
        );
        assert_eq!(settings.paste_in_terminals, DEFAULT_PASTE_IN_TERMINALS);
        assert!(settings.final_pass);
        assert!(settings.append_space);
        assert_eq!(settings.preroll_ms, DEFAULT_PREROLL_MS);
    }

    #[test]
    fn daemon_flags_can_disable_marker_and_live_partials() {
        let args = parse_daemon_args(["speaches-companion", "daemon", "--no-listening-marker"]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());
        assert_eq!(settings.listening_marker, None);

        let args = parse_daemon_args(["speaches-companion", "daemon", "--no-inline-partials"]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());
        assert!(!settings.inline_partials);

        let args = parse_daemon_args(["speaches-companion", "daemon", "--no-partial-chunking"]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());
        assert!(!settings.partial_chunking);

        let args = parse_daemon_args(["speaches-companion", "daemon", "--no-append-space"]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());
        assert!(!settings.append_space);
    }

    #[test]
    fn daemon_reads_dictation_settings_from_file_config() {
        let args = parse_daemon_args(["speaches-companion", "daemon"]);
        let file_config = FileConfig {
            dictation: DictationFileConfig {
                transcript_dir: Some(PathBuf::from("transcripts")),
                #[cfg(feature = "debug-recordings")]
                record_dir: Some(PathBuf::from("recordings")),
                stream_response: Some(true),
                realtime_partials: Some(true),
                final_pass: Some(false),
                listening_marker: Some("...".to_string()),
                inline_partials: Some(false),
                partial_chunking: Some(false),
                partial_chunk_delay_ms: Some(25),
                partial_chunk_max_delay_ms: Some(90),
                append_space: Some(false),
                inject_delay_microsecs: Some(3_000),
                paste_settle_delay_ms: Some(450),
                paste_in_terminals: Some(true),
                leading_silence_ms: Some(400),
                preroll_ms: Some(1_000),
            },
            ..FileConfig::default()
        };

        let settings = resolve_daemon_settings(&args, &file_config);

        assert_eq!(settings.transcript_dir, Some(PathBuf::from("transcripts")));
        #[cfg(feature = "debug-recordings")]
        assert_eq!(settings.record_dir, Some(PathBuf::from("recordings")));
        assert!(settings.stream_response);
        assert!(settings.realtime_partials);
        assert!(!settings.final_pass);
        assert_eq!(settings.listening_marker, Some("...".to_string()));
        assert!(!settings.inline_partials);
        assert!(!settings.partial_chunking);
        assert_eq!(settings.partial_chunk_delay_ms, 25);
        assert_eq!(settings.partial_chunk_max_delay_ms, 90);
        assert!(!settings.append_space);
        assert_eq!(settings.inject_delay_microsecs, 3_000);
        assert_eq!(settings.paste_settle_delay_ms, 450);
        assert!(settings.paste_in_terminals);
        assert_eq!(settings.leading_silence_ms, 400);
        assert_eq!(settings.preroll_ms, 1_000);
    }

    #[test]
    fn daemon_can_disable_stream_response_from_file_config() {
        let args = parse_daemon_args(["speaches-companion", "daemon", "--no-stream-response"]);
        let file_config = FileConfig {
            dictation: DictationFileConfig {
                stream_response: Some(true),
                ..DictationFileConfig::default()
            },
            ..FileConfig::default()
        };

        let settings = resolve_daemon_settings(&args, &file_config);

        assert!(!settings.stream_response);
    }

    #[test]
    fn daemon_can_disable_realtime_partials_from_file_config() {
        let args = parse_daemon_args(["speaches-companion", "daemon", "--no-realtime-partials"]);
        let file_config = FileConfig {
            dictation: DictationFileConfig {
                realtime_partials: Some(true),
                ..DictationFileConfig::default()
            },
            ..FileConfig::default()
        };

        let settings = resolve_daemon_settings(&args, &file_config);

        assert!(!settings.realtime_partials);
    }

    #[test]
    fn daemon_can_disable_final_pass_from_file_config() {
        let args = parse_daemon_args(["speaches-companion", "daemon", "--no-final-pass"]);
        let file_config = FileConfig {
            dictation: DictationFileConfig {
                final_pass: Some(true),
                ..DictationFileConfig::default()
            },
            ..FileConfig::default()
        };

        let settings = resolve_daemon_settings(&args, &file_config);

        assert!(!settings.final_pass);
    }

    #[tokio::test]
    async fn realtime_daemon_startup_fails_when_warmup_fails() {
        let startup = FakeRealtimeStartup::new(Ok(()), Err("connection refused"));
        let events = startup.events.clone();

        let error = prepare_realtime_daemon_transcriber(startup)
            .await
            .unwrap_err();

        let error = format!("{error:#}");
        assert!(error.contains("realtime transcription backend is unavailable"));
        assert!(error.contains("connection refused"));
        assert_eq!(*events.lock().unwrap(), vec!["prepare_capture", "warm_up"]);
    }

    #[tokio::test]
    async fn realtime_daemon_startup_prepares_before_warmup() {
        let startup = FakeRealtimeStartup::new(Ok(()), Ok(()));
        let events = startup.events.clone();

        prepare_realtime_daemon_transcriber(startup).await.unwrap();

        assert_eq!(*events.lock().unwrap(), vec!["prepare_capture", "warm_up"]);
    }

    #[test]
    fn daemon_accepts_custom_listening_marker() {
        let args = parse_daemon_args(["speaches-companion", "daemon", "--listening-marker", "..."]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());

        assert_eq!(settings.listening_marker, Some("...".to_string()));
        assert!(settings.inline_partials);
    }

    #[test]
    fn daemon_accepts_custom_preroll() {
        let args = parse_daemon_args(["speaches-companion", "daemon", "--preroll-ms", "1000"]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());

        assert_eq!(args.preroll_ms, Some(1_000));
        assert_eq!(settings.preroll_ms, 1_000);
    }

    #[test]
    fn daemon_accepts_custom_injection_delay() {
        let args = parse_daemon_args([
            "speaches-companion",
            "daemon",
            "--inject-delay-microsecs",
            "3000",
        ]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());

        assert_eq!(settings.inject_delay_microsecs, 3_000);
    }

    #[test]
    fn daemon_accepts_custom_paste_settle_delay() {
        let args = parse_daemon_args([
            "speaches-companion",
            "daemon",
            "--paste-settle-delay-ms",
            "450",
        ]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());

        assert_eq!(settings.paste_settle_delay_ms, 450);
    }

    #[test]
    fn daemon_accepts_terminal_paste_flags() {
        let args = parse_daemon_args(["speaches-companion", "daemon", "--paste-in-terminals"]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());
        assert!(settings.paste_in_terminals);

        let args = parse_daemon_args(["speaches-companion", "daemon", "--no-paste-in-terminals"]);
        let file_config = FileConfig {
            dictation: DictationFileConfig {
                paste_in_terminals: Some(true),
                ..DictationFileConfig::default()
            },
            ..FileConfig::default()
        };
        let settings = resolve_daemon_settings(&args, &file_config);
        assert!(!settings.paste_in_terminals);
    }

    #[test]
    fn daemon_accepts_custom_partial_chunking_delays() {
        let args = parse_daemon_args([
            "speaches-companion",
            "daemon",
            "--partial-chunk-delay-ms",
            "40",
            "--partial-chunk-max-delay-ms",
            "120",
        ]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());

        assert_eq!(settings.partial_chunk_delay_ms, 40);
        assert_eq!(settings.partial_chunk_max_delay_ms, 120);
    }

    #[test]
    fn wakeword_defaults_to_default_name_and_config_values() {
        let args = parse_wakeword_args(["speaches-companion", "wakeword"]);
        let settings = resolve_wakeword_settings(&args, &FileConfig::default());

        assert_eq!(settings.name, DEFAULT_WAKEWORD_NAME);
        assert_eq!(settings.engine, WakewordEngine::Openwakeword);
        assert_eq!(settings.stock_model, DEFAULT_OPENWAKEWORD_STOCK_MODEL);
        assert_eq!(settings.assets_dir, None);
        assert_eq!(settings.threshold, DEFAULT_WAKEWORD_THRESHOLD);
        assert_eq!(
            settings.silence_timeout,
            Duration::from_millis(DEFAULT_WAKEWORD_SILENCE_TIMEOUT_MS)
        );
        assert!(settings.press_enter);
        assert!(!resolve_wakeword_notify_on_detect(
            &args,
            &FileConfig::default()
        ));
    }

    #[test]
    fn wakeword_reads_settings_from_file_config() {
        let args = parse_wakeword_args(["speaches-companion", "wakeword"]);
        let file_config = FileConfig {
            wakeword: WakewordFileConfig {
                name: Some(String::from("aurgob")),
                engine: Some(WakewordEngine::Onnx),
                stock_model: Some(OpenWakewordStockModel::Weather),
                assets_dir: Some(PathBuf::from("/tmp/openwakeword-assets")),
                root_dir: Some(PathBuf::from("/tmp/wakewords")),
                threshold: Some(0.72),
                frame_ms: Some(40),
                silence_timeout_ms: Some(700),
                activation_grace_ms: Some(4_000),
                max_recording_ms: Some(20_000),
                press_enter: Some(false),
                notify_on_detect: Some(true),
            },
            ..FileConfig::default()
        };

        let settings = resolve_wakeword_settings(&args, &file_config);

        assert_eq!(settings.name, "aurgob");
        assert_eq!(settings.engine, WakewordEngine::Onnx);
        assert_eq!(settings.stock_model, OpenWakewordStockModel::Weather);
        assert_eq!(
            settings.assets_dir,
            Some(PathBuf::from("/tmp/openwakeword-assets"))
        );
        assert_eq!(settings.root_dir, PathBuf::from("/tmp/wakewords"));
        assert_eq!(settings.threshold, 0.72);
        assert_eq!(settings.frame, Duration::from_millis(40));
        assert_eq!(settings.silence_timeout, Duration::from_millis(700));
        assert_eq!(settings.activation_grace, Duration::from_millis(4_000));
        assert_eq!(settings.max_recording, Duration::from_millis(20_000));
        assert!(!settings.press_enter);
        assert!(resolve_wakeword_notify_on_detect(&args, &file_config));
    }

    #[test]
    fn wakeword_cli_name_overrides_file_config_name() {
        let args = parse_wakeword_args(["speaches-companion", "wakeword", "samantha"]);
        let file_config = FileConfig {
            wakeword: WakewordFileConfig {
                name: Some(String::from("hey_computer")),
                ..WakewordFileConfig::default()
            },
            ..FileConfig::default()
        };

        let settings = resolve_wakeword_settings(&args, &file_config);

        assert_eq!(settings.name, "samantha");
    }

    #[test]
    fn wakeword_cli_overrides_file_config() {
        let args = parse_wakeword_args([
            "speaches-companion",
            "wakeword",
            "samantha",
            "--engine",
            "onnx",
            "--stock-model",
            "timer",
            "--assets-dir",
            "/tmp/cli-assets",
            "--root-dir",
            "/tmp/cli-wakewords",
            "--threshold",
            "0.8",
            "--no-press-enter",
            "--no-notify-on-detect",
            "--no-append-space",
            "--inject-delay-microsecs",
            "3000",
        ]);
        let file_config = FileConfig {
            wakeword: WakewordFileConfig {
                name: Some(String::from("hey_computer")),
                engine: Some(WakewordEngine::Openwakeword),
                stock_model: Some(OpenWakewordStockModel::Alexa),
                assets_dir: Some(PathBuf::from("/tmp/file-assets")),
                root_dir: Some(PathBuf::from("/tmp/file-wakewords")),
                threshold: Some(0.4),
                press_enter: Some(true),
                notify_on_detect: Some(true),
                ..WakewordFileConfig::default()
            },
            dictation: DictationFileConfig {
                append_space: Some(true),
                inject_delay_microsecs: Some(40),
                ..DictationFileConfig::default()
            },
            ..FileConfig::default()
        };

        let settings = resolve_wakeword_settings(&args, &file_config);

        assert_eq!(settings.name, "samantha");
        assert_eq!(settings.engine, WakewordEngine::Onnx);
        assert_eq!(settings.stock_model, OpenWakewordStockModel::Timer);
        assert_eq!(settings.assets_dir, Some(PathBuf::from("/tmp/cli-assets")));
        assert_eq!(settings.root_dir, PathBuf::from("/tmp/cli-wakewords"));
        assert_eq!(settings.threshold, 0.8);
        assert!(!settings.press_enter);
        assert!(!resolve_wakeword_notify_on_detect(&args, &file_config));
        assert!(!resolve_wakeword_append_space(&args, &file_config));
        assert_eq!(resolve_wakeword_inject_delay(&args, &file_config), 3_000);
    }

    #[test]
    fn daemon_accepts_transcript_dir() {
        let args = parse_daemon_args([
            "speaches-companion",
            "daemon",
            "--transcript-dir",
            "target/speaches-companion-debug",
        ]);

        assert_eq!(
            args.transcript_dir,
            Some(PathBuf::from("target/speaches-companion-debug"))
        );
    }

    #[cfg(feature = "debug-recordings")]
    #[test]
    fn daemon_accepts_record_dir() {
        let args = parse_daemon_args([
            "speaches-companion",
            "daemon",
            "--record-dir",
            "target/speaches-companion-recordings",
        ]);
        let settings = resolve_daemon_settings(&args, &FileConfig::default());

        assert_eq!(
            args.record_dir,
            Some(PathBuf::from("target/speaches-companion-recordings"))
        );
        assert_eq!(
            settings.record_dir,
            Some(PathBuf::from("target/speaches-companion-recordings"))
        );
    }

    #[cfg(not(feature = "debug-recordings"))]
    #[test]
    fn daemon_rejects_record_dir() {
        assert!(Cli::try_parse_from([
            "speaches-companion",
            "daemon",
            "--record-dir",
            "target/speaches-companion-debug"
        ])
        .is_err());
    }

    #[test]
    fn read_aloud_accepts_text_and_tts_options() {
        match Cli::parse_from([
            "speaches-companion",
            "read-aloud",
            "--text",
            "read this",
            "--model",
            "tts-1",
            "--voice",
            "lessac",
            "--speed",
            "1.2",
            "--response-format",
            "wav",
            "--player",
            "pw-play",
            "--player-arg=--raw",
            "--player-arg=--rate",
            "--player-arg",
            "24000",
        ])
        .command
        {
            Command::ReadAloud(args) => {
                assert_eq!(args.text.as_deref(), Some("read this"));
                assert_eq!(args.model.as_deref(), Some("tts-1"));
                assert_eq!(args.voice.as_deref(), Some("lessac"));
                assert_eq!(args.speed, Some(1.2));
                assert_eq!(args.response_format.as_deref(), Some("wav"));
                assert_eq!(args.player.as_deref(), Some("pw-play"));
                assert_eq!(args.player_args, ["--raw", "--rate", "24000"]);
            }
            _ => panic!("expected read-aloud command"),
        }
    }

    #[test]
    fn realtime_check_accepts_stt_options() {
        match Cli::parse_from([
            "speaches-companion",
            "realtime-check",
            "--base-url",
            "http://speaches.example:8000",
            "--model",
            "model/name",
            "--language",
            "de",
        ])
        .command
        {
            Command::RealtimeCheck(args) => {
                assert_eq!(
                    args.base_url.as_deref(),
                    Some("http://speaches.example:8000")
                );
                assert_eq!(args.model.as_deref(), Some("model/name"));
                assert_eq!(args.language.as_deref(), Some("de"));
            }
            _ => panic!("expected realtime-check command"),
        }
    }

    fn parse_daemon_args<const N: usize>(args: [&str; N]) -> DaemonArgs {
        match Cli::parse_from(args).command {
            Command::Daemon(args) => args,
            _ => panic!("expected daemon command"),
        }
    }

    fn parse_wakeword_args<const N: usize>(args: [&str; N]) -> WakewordArgs {
        match Cli::parse_from(args).command {
            Command::Wakeword(args) => args,
            _ => panic!("expected wakeword command"),
        }
    }

    #[derive(Debug)]
    struct FakeRealtimeStartup {
        events: Arc<Mutex<Vec<&'static str>>>,
        prepare_result: Result<(), &'static str>,
        warm_up_result: Result<(), &'static str>,
    }

    impl FakeRealtimeStartup {
        fn new(
            prepare_result: Result<(), &'static str>,
            warm_up_result: Result<(), &'static str>,
        ) -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
                prepare_result,
                warm_up_result,
            }
        }
    }

    #[async_trait::async_trait]
    impl RealtimeDaemonStartup for FakeRealtimeStartup {
        async fn prepare_capture(&self) -> anyhow::Result<()> {
            self.events.lock().unwrap().push("prepare_capture");
            self.prepare_result.map_err(anyhow::Error::msg)
        }

        async fn warm_up(&self) -> anyhow::Result<()> {
            self.events.lock().unwrap().push("warm_up");
            self.warm_up_result.map_err(anyhow::Error::msg)
        }
    }

    #[derive(Clone, Default)]
    struct FakeErrorNotifier {
        notifications: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl ErrorNotifier for FakeErrorNotifier {
        fn notify_error(&self, summary: &str, body: &str) -> anyhow::Result<()> {
            self.notifications
                .lock()
                .unwrap()
                .push((summary.to_string(), body.to_string()));
            Ok(())
        }
    }
}
