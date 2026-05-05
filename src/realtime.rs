use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::audio::capture_with_pw_record;
use crate::config::{health_url, realtime_ws_url, DictateLiveConfig};
use crate::event::{classify_event, RealtimeEvent};
use crate::phase::{PhaseGate, PhaseResult};
use crate::trace::TraceWriter;

pub struct RunOutcome {
    pub result: PhaseResult,
    pub trace_path: std::path::PathBuf,
    pub total_audio_bytes: usize,
}

pub async fn preflight_health(base_url: &str) -> Result<()> {
    let url = health_url(base_url)?;
    let response = reqwest::get(&url)
        .await
        .with_context(|| format!("failed to reach Speaches health endpoint {url}"))?;
    if !response.status().is_success() {
        bail!(
            "Speaches health endpoint {url} returned {}",
            response.status()
        );
    }
    Ok(())
}

pub async fn run_dictate_live(config: DictateLiveConfig) -> Result<RunOutcome> {
    preflight_health(&config.base_url).await?;
    let ws_url = realtime_ws_url(&config.base_url, &config.model, config.language.as_deref())?;
    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .with_context(|| format!("failed to connect to Speaches realtime websocket {ws_url}"))?;
    let (mut sink, mut stream) = ws_stream.split();
    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(32);
    let duration = Duration::from_secs(config.duration_seconds);

    let capture_task = tokio::spawn(async move {
        capture_with_pw_record(duration, |chunk| {
            let audio_tx = audio_tx.clone();
            async move {
                audio_tx
                    .send(chunk)
                    .await
                    .context("failed to queue audio chunk for websocket")
            }
        })
        .await
    });

    let sender_task = tokio::spawn(async move {
        while let Some(chunk) = audio_rx.recv().await {
            let event = json!({
                "type": "input_audio_buffer.append",
                "audio": STANDARD.encode(chunk),
            });
            sink.send(Message::Text(event.to_string().into()))
                .await
                .context("failed to send audio chunk")?;
        }

        sink.send(Message::Text(
            json!({"type": "input_audio_buffer.commit"})
                .to_string()
                .into(),
        ))
        .await
        .context("failed to send input_audio_buffer.commit")?;
        Result::<()>::Ok(())
    });

    let mut trace = TraceWriter::default();
    let mut gate = PhaseGate::default();

    while let Some(message) = stream.next().await {
        let message = message.context("failed to receive websocket message")?;
        let Some(text) = websocket_text(message)? else {
            continue;
        };
        println!("{text}");
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("invalid server event JSON: {text}"))?;
        let classified = classify_event(&value);
        gate.observe(&classified);
        trace.push(value);

        match classified {
            RealtimeEvent::Completed(_) | RealtimeEvent::Failed(_) | RealtimeEvent::Error(_) => {
                break
            }
            _ => {}
        }
    }

    let total_audio_bytes = capture_task
        .await
        .context("audio capture task panicked")??;
    sender_task
        .await
        .context("websocket sender task panicked")??;
    trace.write_jsonl(&config.trace_path).await?;

    Ok(RunOutcome {
        result: gate.result(),
        trace_path: config.trace_path,
        total_audio_bytes,
    })
}

fn websocket_text(message: Message) -> Result<Option<String>> {
    match message {
        Message::Text(text) => Ok(Some(text.to_string())),
        Message::Binary(bytes) => String::from_utf8(bytes.to_vec())
            .map(Some)
            .context("received non-UTF8 binary websocket message"),
        Message::Ping(_) | Message::Pong(_) => Ok(None),
        Message::Close(_) => Ok(None),
        Message::Frame(_) => Ok(None),
    }
}
