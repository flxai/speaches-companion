use httpmock::prelude::*;
use tempfile::tempdir;
use trec::config::DEFAULT_MODEL;
use trec::stt::{transcribe_file, ResponseFormat, TranscribeOptions};

#[test]
fn response_format_round_trips_cli_values() {
    assert_eq!(ResponseFormat::Text.as_str(), "text");
    assert_eq!(
        "verbose_json".parse::<ResponseFormat>().unwrap(),
        ResponseFormat::VerboseJson
    );
    assert!("bogus".parse::<ResponseFormat>().is_err());
}

#[tokio::test]
async fn transcribe_file_posts_openai_compatible_multipart() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/audio/transcriptions")
            .body_includes(format!("name=\"model\"\r\n\r\n{DEFAULT_MODEL}"))
            .body_includes("name=\"response_format\"\r\n\r\ntext")
            .body_includes("name=\"without_timestamps\"\r\n\r\ntrue")
            .body_includes("name=\"language\"\r\n\r\nde")
            .body_includes("name=\"prompt\"\r\n\r\nterminal dictation")
            .body_includes("name=\"hotwords\"\r\n\r\nSpeaches")
            .body_includes("name=\"file\"")
            .body_includes("hello audio");
        then.status(200)
            .header("content-type", "text/plain")
            .body("hello transcript");
    });
    let dir = tempdir().unwrap();
    let audio_path = dir.path().join("audio.wav");
    tokio::fs::write(&audio_path, b"hello audio").await.unwrap();
    let options = TranscribeOptions {
        model: DEFAULT_MODEL.to_string(),
        response_format: ResponseFormat::Text,
        language: Some("de".to_string()),
        prompt: Some("terminal dictation".to_string()),
        hotwords: Some("Speaches".to_string()),
        without_timestamps: true,
    };

    let text = transcribe_file(&server.base_url(), &audio_path, &options)
        .await
        .unwrap();

    mock.assert();
    assert_eq!(text, "hello transcript");
}
