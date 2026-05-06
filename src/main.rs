use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use trec::audio::{record_wav_with_pw_record, STT_SAMPLE_RATE};
use trec::config::{resolve_config, ConfigInput};
use trec::daemon::run_daemon;
use trec::inject::{LibXdoTextInjector, TextInjector};
use trec::ipc::{default_socket_path, send_command, IpcCommand};
use trec::notification::{DesktopErrorNotifier, NoopTranscriptNotifier};
use trec::phase::PhaseResult;
use trec::realtime::run_dictate_live;
use trec::streaming::{RollingHttpTranscriber, StreamingDictationController};
use trec::stt::{transcribe_file, ResponseFormat, TranscribeOptions};

#[derive(Debug, Parser)]
#[command(name = "trec", about = "Linux realtime dictation spike")]
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
    Smoke(SmokeArgs),
    Transcribe(TranscribeArgs),
}

#[derive(Debug, Args)]
struct DaemonArgs {
    #[arg(long)]
    socket_path: Option<PathBuf>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    language: Option<String>,
    #[arg(long)]
    record_dir: Option<PathBuf>,
    #[arg(long)]
    stream_response: bool,
}

#[derive(Debug, Args)]
struct DictateLiveArgs {
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
}

#[derive(Debug, Args)]
struct TranscribeArgs {
    audio: PathBuf,
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
    #[arg(long, default_value = "3")]
    record_seconds: u64,
    #[arg(long, default_value = "target/trec-smoke/speaches-mic.wav")]
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
        Command::Smoke(args) => run_smoke_command(args).await,
        Command::Transcribe(args) => run_transcribe_command(args).await,
    }
}

async fn run_daemon_command(args: DaemonArgs) -> ExitCode {
    let socket_path = args.socket_path.unwrap_or_else(default_socket_path);
    if let Some(record_dir) = args.record_dir {
        eprintln!(
            "--record-dir is ignored by the streaming daemon: {}",
            record_dir.display()
        );
    }
    let config = resolve_config(ConfigInput {
        cli_base_url: args.base_url,
        cli_model: args.model,
        cli_language: args.language,
        env_base_url: std::env::var("SPEACHES_BASE_URL").ok(),
        env_model: std::env::var("TREC_MODEL")
            .ok()
            .or_else(|| std::env::var("SPEACHES_STT_MODEL").ok()),
        env_language: std::env::var("TREC_LANGUAGE").ok(),
        ..ConfigInput::default()
    });
    let transcriber = RollingHttpTranscriber::new(
        config.base_url,
        TranscribeOptions {
            model: config.model,
            response_format: ResponseFormat::Text,
            language: config.language,
            prompt: None,
            hotwords: None,
            without_timestamps: true,
            stream: args.stream_response,
        },
    );
    let injector = LibXdoTextInjector::default();
    let controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        NoopTranscriptNotifier,
        DesktopErrorNotifier,
    );

    eprintln!("trec daemon listening on {}", socket_path.display());
    match run_daemon(&socket_path, controller).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("trec daemon failed: {error:#}");
            ExitCode::from(1)
        }
    }
}

async fn run_hotkey_command(args: HotkeyArgs) -> ExitCode {
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
            eprintln!("trec hotkey failed: {error:#}");
            ExitCode::from(1)
        }
    }
}

async fn run_inject_command(args: InjectArgs) -> ExitCode {
    let injector = LibXdoTextInjector::new(args.delay_microsecs);
    match injector.inject_text(&args.text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("inject failed: {error:#}");
            ExitCode::from(1)
        }
    }
}

async fn run_transcribe_command(args: TranscribeArgs) -> ExitCode {
    let config = resolve_config(ConfigInput {
        cli_base_url: args.base_url,
        cli_model: args.model,
        env_base_url: std::env::var("SPEACHES_BASE_URL").ok(),
        env_model: std::env::var("TREC_MODEL")
            .ok()
            .or_else(|| std::env::var("SPEACHES_STT_MODEL").ok()),
        ..ConfigInput::default()
    });
    let options = TranscribeOptions {
        model: config.model,
        response_format: args.response_format,
        language: args.language,
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
    let config = resolve_config(ConfigInput {
        cli_base_url: args.base_url,
        cli_model: args.model,
        cli_language: args.language,
        cli_duration_seconds: args.duration_seconds,
        cli_trace_path: args.trace_path,
        env_base_url: std::env::var("SPEACHES_BASE_URL").ok(),
        env_model: std::env::var("TREC_MODEL").ok(),
        env_language: std::env::var("TREC_LANGUAGE").ok(),
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
