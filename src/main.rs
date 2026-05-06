use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use trec::audio::{record_wav_with_pw_record, STT_SAMPLE_RATE};
use trec::config::{resolve_config, ConfigInput};
use trec::daemon::run_daemon;
use trec::inject::{LibXdoTextInjector, TextInjector};
use trec::ipc::{default_socket_path, send_command, IpcCommand};
use trec::notification::{
    DesktopErrorNotifier, ErrorNotifier, NoopTranscriptNotifier, HOTKEY_ERROR_SUMMARY,
};
use trec::phase::PhaseResult;
use trec::realtime::run_dictate_live;
use trec::streaming::{RollingHttpTranscriber, StreamingDictationController};
use trec::stt::{transcribe_file, ResponseFormat, TranscribeOptions};

const DEFAULT_LISTENING_MARKER: &str = "💬";

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
    transcript_dir: Option<PathBuf>,
    #[arg(long)]
    stream_response: bool,
    #[arg(long)]
    listening_marker: Option<String>,
    #[arg(long)]
    no_listening_marker: bool,
    #[arg(long, conflicts_with = "no_inline_partials")]
    inline_partials: bool,
    #[arg(long)]
    no_inline_partials: bool,
    #[arg(long, default_value = "1250")]
    partial_interval_ms: u64,
    #[arg(long, default_value = "0")]
    partial_min_duration_ms: u64,
    #[arg(long, default_value = "250")]
    leading_silence_ms: u64,
    #[arg(long, default_value = "750")]
    preroll_ms: u64,
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
    let listening_marker = resolve_listening_marker(&args);
    let inline_partials = resolve_inline_partials(&args);
    let transcript_dir = args.transcript_dir.clone();
    let socket_path = args.socket_path.unwrap_or_else(default_socket_path);
    if let Some(transcript_dir) = transcript_dir.as_ref() {
        eprintln!(
            "trec preserving transcripts in {}",
            transcript_dir.display()
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
    )
    .with_partial_interval(Duration::from_millis(args.partial_interval_ms))
    .with_partial_min_duration(Duration::from_millis(args.partial_min_duration_ms))
    .with_leading_silence(Duration::from_millis(args.leading_silence_ms))
    .with_preroll(Duration::from_millis(args.preroll_ms))
    .with_transcript_dir(transcript_dir);
    eprintln!("trec starting continuous audio capture...");
    if let Err(error) = transcriber.prepare_capture().await {
        eprintln!("trec failed to start continuous audio capture: {error:#}");
        return ExitCode::from(1);
    }
    eprintln!("trec continuous audio capture ready");
    eprintln!("trec warming transcription backend...");
    match transcriber.warm_up_transcription().await {
        Ok(()) => eprintln!("trec transcription backend ready"),
        Err(error) => eprintln!("trec transcription warmup failed: {error:#}"),
    }
    let injector = LibXdoTextInjector::default();
    let controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        NoopTranscriptNotifier,
        DesktopErrorNotifier,
    )
    .with_listening_marker(listening_marker)
    .with_inline_partials(inline_partials);

    eprintln!("trec daemon listening on {}", socket_path.display());
    match run_daemon(&socket_path, controller).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("trec daemon failed: {error:#}");
            ExitCode::from(1)
        }
    }
}

fn resolve_listening_marker(args: &DaemonArgs) -> Option<String> {
    if args.no_listening_marker {
        None
    } else {
        Some(
            args.listening_marker
                .clone()
                .unwrap_or_else(|| DEFAULT_LISTENING_MARKER.to_string()),
        )
    }
}

fn resolve_inline_partials(args: &DaemonArgs) -> bool {
    args.inline_partials || !args.no_inline_partials
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
            eprintln!("trec hotkey failed: {error:#}");
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
        eprintln!("trec notification failed: {notify_error:#}");
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[tokio::test]
    async fn hotkey_connection_error_sends_desktop_error_notification() {
        let socket_path =
            std::env::temp_dir().join(format!("trec-missing-hotkey-{}.sock", std::process::id()));
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
            .contains("failed to connect to trec daemon"));
        assert!(notifications[0]
            .1
            .contains(&socket_path.display().to_string()));
    }

    #[test]
    fn daemon_defaults_insert_marker_and_live_partials() {
        let args = parse_daemon_args(["trec", "daemon"]);

        assert_eq!(
            resolve_listening_marker(&args),
            Some(DEFAULT_LISTENING_MARKER.to_string())
        );
        assert!(resolve_inline_partials(&args));
        assert_eq!(args.preroll_ms, 750);
    }

    #[test]
    fn daemon_flags_can_disable_marker_and_live_partials() {
        let args = parse_daemon_args(["trec", "daemon", "--no-listening-marker"]);
        assert_eq!(resolve_listening_marker(&args), None);

        let args = parse_daemon_args(["trec", "daemon", "--no-inline-partials"]);
        assert!(!resolve_inline_partials(&args));
    }

    #[test]
    fn daemon_accepts_custom_listening_marker() {
        let args = parse_daemon_args(["trec", "daemon", "--listening-marker", "..."]);

        assert_eq!(resolve_listening_marker(&args), Some("...".to_string()));
        assert!(resolve_inline_partials(&args));
    }

    #[test]
    fn daemon_accepts_custom_preroll() {
        let args = parse_daemon_args(["trec", "daemon", "--preroll-ms", "1000"]);

        assert_eq!(args.preroll_ms, 1_000);
    }

    #[test]
    fn daemon_accepts_transcript_dir() {
        let args = parse_daemon_args(["trec", "daemon", "--transcript-dir", "target/trec-debug"]);

        assert_eq!(
            args.transcript_dir,
            Some(PathBuf::from("target/trec-debug"))
        );
    }

    #[test]
    fn daemon_rejects_record_dir() {
        assert!(
            Cli::try_parse_from(["trec", "daemon", "--record-dir", "target/trec-debug"]).is_err()
        );
    }

    fn parse_daemon_args<const N: usize>(args: [&str; N]) -> DaemonArgs {
        match Cli::parse_from(args).command {
            Command::Daemon(args) => args,
            _ => panic!("expected daemon command"),
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
