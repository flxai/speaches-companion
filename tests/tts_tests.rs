use httpmock::prelude::*;
use speaches_scribe::tts::{
    normalize_read_aloud_text, play_audio_file, speech_url, synthesize_speech, SpeechOptions,
};
use tempfile::tempdir;

#[test]
fn speech_url_uses_openai_compatible_audio_speech_path() {
    let url = speech_url("http://ono.tail:8000/base?ignored=true").unwrap();

    assert_eq!(url.as_str(), "http://ono.tail:8000/v1/audio/speech");
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
                "response_format": "wav",
            }));
        then.status(200)
            .header("content-type", "audio/wav")
            .body("RIFF fake wav");
    });
    let options = SpeechOptions {
        model: "tts-1".to_string(),
        voice: "lessac".to_string(),
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
    std::fs::write(&audio_path, b"fake audio").unwrap();

    play_audio_file(
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
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(args_path).unwrap(),
        format!("--raw\n--rate\n24000\n{}\n", audio_path.display())
    );
}
