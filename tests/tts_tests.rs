use httpmock::prelude::*;
use speaches_companion::tts::{
    normalize_read_aloud_text, play_audio_file_with_state, speech_url, synthesize_speech,
    PlaybackState, SpeechOptions,
};
use tempfile::tempdir;
use tokio::time::{sleep, Duration, Instant};

#[test]
fn speech_url_uses_openai_compatible_audio_speech_path() {
    let url = speech_url("http://localhost:8000/base?ignored=true").unwrap();

    assert_eq!(url.as_str(), "http://localhost:8000/base/v1/audio/speech");
}

#[test]
fn read_aloud_text_is_trimmed_and_rejects_empty_input() {
    assert_eq!(
        normalize_read_aloud_text("  read this\n"),
        Some("read this".to_string())
    );
    assert_eq!(normalize_read_aloud_text(" \n\t"), None);
}

#[tokio::test]
async fn synthesize_speech_posts_openai_compatible_json() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/audio/speech")
            .json_body_obj(&serde_json::json!({
                "input": "hello window",
                "model": "tts-1",
                "voice": "lessac",
                "speed": 1.2,
                "response_format": "wav",
            }));
        then.status(200)
            .header("content-type", "audio/wav")
            .body("RIFF fake wav");
    });
    let options = SpeechOptions {
        model: "tts-1".to_string(),
        voice: "lessac".to_string(),
        speed: 1.2,
        response_format: "wav".to_string(),
    };

    let audio = synthesize_speech(&server.base_url(), " hello window ", &options)
        .await
        .unwrap();

    mock.assert();
    assert_eq!(audio, b"RIFF fake wav");
}

#[tokio::test]
async fn play_audio_file_passes_player_args_before_path() {
    let dir = tempdir().unwrap();
    let audio_path = dir.path().join("audio.pcm");
    let args_path = dir.path().join("args.txt");
    let state = PlaybackState::in_dir(dir.path().join("state"));
    std::fs::write(&audio_path, b"fake audio").unwrap();

    play_audio_file_with_state(
        &audio_path,
        "sh",
        &[
            "-c".to_string(),
            format!("printf '%s\\n' \"$@\" > {}", args_path.display()),
            "sh".to_string(),
            "--raw".to_string(),
            "--rate".to_string(),
            "24000".to_string(),
        ],
        &state,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(args_path).unwrap(),
        format!("--raw\n--rate\n24000\n{}\n", audio_path.display())
    );
}

#[tokio::test]
async fn play_audio_file_stops_prior_recorded_playback() {
    let dir = tempdir().unwrap();
    let audio_path = dir.path().join("audio.pcm");
    let log_path = dir.path().join("playback.log");
    let state = PlaybackState::in_dir(dir.path().join("state"));
    std::fs::write(&audio_path, b"fake audio").unwrap();

    let first_audio_path = audio_path.clone();
    let first_log_path = log_path.clone();
    let first_state = state.clone();
    let first = tokio::spawn(async move {
        play_audio_file_with_state(
            &first_audio_path,
            "sh",
            &[
                "-c".to_string(),
                "trap 'echo first_term >> \"$1\"; exit 0' TERM; echo first_start >> \"$1\"; sleep 30; echo first_done >> \"$1\"".to_string(),
                "sh".to_string(),
                first_log_path.display().to_string(),
            ],
            &first_state,
        )
        .await
    });

    wait_for_log_contains(&log_path, "first_start").await;

    play_audio_file_with_state(
        &audio_path,
        "sh",
        &[
            "-c".to_string(),
            "echo second_start >> \"$1\"".to_string(),
            "sh".to_string(),
            log_path.display().to_string(),
        ],
        &state,
    )
    .await
    .unwrap();

    first.await.unwrap().unwrap();

    let log = std::fs::read_to_string(log_path).unwrap();
    assert!(log.contains("first_start"));
    assert!(log.contains("first_term"));
    assert!(log.contains("second_start"));
    assert!(!log.contains("first_done"));
}

async fn wait_for_log_contains(path: &std::path::Path, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(path)
            .map(|log| log.contains(needle))
            .unwrap_or(false)
        {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {needle}");
        sleep(Duration::from_millis(20)).await;
    }
}
