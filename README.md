# Speaches Companion

Type what you speak. Read what you mark.

Speaches Companion is the desktop client for
[Speaches](https://github.com/speaches-ai/speaches). It captures microphone
audio, sends it to Speaches for STT, types the transcript into the focused app,
and reads marked text back through Speaches TTS. It is not a standalone speech
engine and only works in conjunction with a running Speaches-compatible server.

The product name is Speaches Companion. The binary, flake app, config
directory, and related identifiers remain `speaches-companion`.

The Nix flake is the primary interface. It builds the `speaches-companion` binary,
wraps it with the required PipeWire and X11 selection helpers, and exposes the
daemon, hotkey, read-aloud, smoke-test, injection, and transcription subcommands
as flake apps.

## Running

Run the default CLI from the flake:

```sh
nix run . -- --help
nix run .#speaches-companion -- --help
nix run . -- daemon
```

The `.` is required. Without it, `nix run -- daemon` treats `daemon` as the flake to run.

Subcommand aliases are also exposed as flake apps:

```sh
nix run .#daemon
nix run .#smoke -- --record-seconds 2 --response-format text
nix run .#hotkey -- down
nix run .#hotkey -- up
nix run .#read-aloud -- --text "hello from Speaches"
nix run .#realtime-check
nix run .#wakeword
```

`speaches-companion` reads configuration from
`$XDG_CONFIG_HOME/speaches-companion/config.toml`, falling back to
`~/.config/speaches-companion/config.toml`. Use `--config` on `daemon`,
`read-aloud`, `transcribe`, `smoke`, `dictate-live`, and `wakeword`, or set
`SPEACHES_COMPANION_CONFIG`, to point at another file. Missing config files are
treated as empty.

```toml
[speaches]
base_url = "http://ono.tail:8000"

[stt]
model = "Systran/faster-whisper-large-v3"
language = "de"

[tts]
model = "speaches-ai/Kokoro-82M-v1.0-ONNX"
voice = "af_heart"
speed = 1.2
response_format = "pcm"
player = "pw-play"
player_args = ["--raw", "--rate", "24000", "--channels", "1", "--format", "s16"]

[dictation]
transcript_dir = "target/speaches-companion-transcripts"
record_dir = "target/speaches-companion-recordings"
stream_response = false
realtime_partials = true
final_pass = false
listening_marker = "💬"
inline_partials = true
append_space = true
inject_delay_microsecs = 3000
leading_silence_ms = 250
preroll_ms = 750

[wakeword]
# root_dir defaults to $XDG_DATA_HOME/speaches-companion/wakewords
engine = "openwakeword"
stock_model = "alexa"
# assets_dir = "/path/to/predownloaded-openwakeword-assets"
threshold = 0.5
frame_ms = 80
silence_timeout_ms = 5000
activation_grace_ms = 5000
max_recording_ms = 30000
press_enter = true
```

For compatible existing setups, environment and CLI values still work. The
resolution order is CLI flags, then environment variables, then TOML, then
built-in defaults. `SPEACHES_BASE_URL`, `SPEACHES_COMPANION_MODEL`, and
`SPEACHES_COMPANION_LANGUAGE` configure STT; the legacy `SPEACHES_STT_MODEL` is
still accepted as a model fallback.

Read-aloud uses Speaches' OpenAI-compatible `/v1/audio/speech` endpoint.
Without `--text`, it reads marked text from the X11 primary selection and falls
back to the clipboard, then plays the returned audio with `pw-play`. Configure
TTS with
`SPEACHES_COMPANION_TTS_MODEL`, `SPEACHES_COMPANION_TTS_VOICE`, and
`SPEACHES_COMPANION_TTS_RESPONSE_FORMAT`, or the matching TOML and CLI values.
Use `--speed`, `SPEACHES_COMPANION_TTS_SPEED`, or `tts.speed` to adjust speech
rate. Use repeated `--player-arg` flags or `tts.player_args` when the selected
response format needs player-specific options, for example raw PCM playback.

By default, the daemon records while the hotkey is held and sends one final
transcription request to Speaches on release. Set `realtime_partials = true` to
use Speaches' realtime WebSocket path for live partials while recording. For
realtime dictation, set `final_pass = false` or pass `--no-final-pass` to stop
on the latest live hypothesis instead of committing a final transcription job on
release. If focus changes during a recording, `speaches-companion` stops editing
the target window instead of sending Backspaces to the wrong place.

On startup the daemon starts continuous `pw-record` capture before it accepts
hotkey-driven dictation. If that capture cannot start, daemon startup fails
instead of falling back to first-use recording. The daemon also attempts a short
silent transcription request to warm the Speaches model/cache. While idle it
keeps a rolling pre-roll window for recovery/debugging, but STT requests use a
short capped pre-roll view with leading and trailing silence trimmed. When a
recording starts, treat the listening marker as the ready-to-speak signal.

Text payloads are injected through Sway/wtype when a Sway IPC socket is
available, otherwise through libxdo text entry with active modifiers temporarily
cleared. Speculative replacement uses Backspace for the portion that changed.
On Sway, text suffixes are pasted as temporary clipboard chunks outside terminal
windows; terminal windows still use virtual key typing. Some browser text fields
drop characters when virtual keyboard events arrive too quickly; set
`inject_delay_microsecs` or pass `--inject-delay-microsecs` to add a per-key
delay and a short pre-injection settle delay for virtual key operations.

Inline partial injection is enabled by default. If the trigger binding keeps a
modifier physically held while dictating, such as some i3 `$sup+d` bindings,
fake typing can trigger window-manager shortcuts. Disable live partial insertion
for those bindings:

```sh
nix run . -- daemon --no-inline-partials
```

Final dictation text gets one trailing space by default, so the next typed word
starts naturally after the injected transcript. Use `append_space = false` or
`--no-append-space` to keep the transcript exact after trimming.

The daemon inserts `💬` after audio capture starts, then replaces it with the
first partial or final text. Override or disable it with:

```sh
nix run . -- daemon --listening-marker "..."
nix run . -- daemon --no-listening-marker
```

Partial transcription starts without an artificial minimum recording duration.
Final and partial STT requests include a short leading silence pad so immediate
speech is less likely to be clipped by the recognizer. Tune the rolling request
cadence and pad if needed:

```sh
nix run . -- daemon --partial-interval-ms 750 --partial-min-duration-ms 0
nix run . -- daemon --leading-silence-ms 400
nix run . -- daemon --preroll-ms 1000
```

To preserve accepted transcripts, set a transcript directory:

```sh
nix run . -- daemon --transcript-dir target/speaches-companion-transcripts
```

The Nix flake build also enables the `debug-recordings` Cargo feature and wraps
`ffmpeg`, so MP3 recording snapshots can be explicitly enabled with a recording
directory:

```sh
nix run . -- daemon --record-dir target/speaches-companion-recordings
```

Plain Cargo builds keep `--record-dir` unavailable unless compiled with
`--features debug-recordings`.

Wake-word dictation runs separately from the hotkey daemon. The canonical flake
entrypoint is `.#wakeword`, which already references predownloaded stock
openWakeWord ONNX assets:

```sh
nix run .#wakeword
nix run .#wakeword -- --stock-model weather
nix run . -- wakeword --assets-dir "$(nix build .#openwakeword-assets --print-out-paths)"
```

`wakeword [name]` defaults to `default` and stores artifacts under
`wakeword.root_dir/<name>/`; unset `root_dir` defaults to
`$XDG_DATA_HOME/speaches-companion/wakewords` or
`~/.local/share/speaches-companion/wakewords`. The canonical default engine is
`openwakeword`: on first run it installs the shared `melspectrogram.onnx` and
`embedding_model.onnx` assets plus the selected stock keyword head, then runs
the full wake-word pipeline locally in Rust. Set `assets_dir` to point at
predownloaded assets instead of downloading them on demand. The flake also
exposes `.#openwakeword-assets` for prefetching those stock ONNX files
explicitly.

Custom wake-word training is intentionally not integrated here. If you want
your own ONNX model, use the Python tooling from the upstream
[openWakeWord](https://github.com/dscripka/openWakeWord) repository, then place
the resulting file at `wakeword.root_dir/<name>/model.onnx`. For a standalone
raw-audio model, run with `--engine onnx`; for a custom openWakeWord keyword
head, keep the default `openwakeword` engine and replace only `model.onnx`.
Speaches STT is still only used after a wake detection, and dictation stops
after `silence_timeout_ms` of silence.

Speaches SSE transcription responses can be tested with:

```sh
nix run . -- transcribe ./audio.wav --response-format text --stream
nix run . -- daemon --stream-response
```

Speaches realtime WebSocket readiness can be tested without recording from the
microphone:

```sh
nix run .#realtime-check
```

This checks `/health`, opens `/v1/realtime?intent=transcription`, sends a short
silent audio buffer, commits it, and waits for a transcription completion event.

## Development

`speaches-companion` links to `libxdo`. On NixOS, plain `cargo run --` works when
`xdotool` is installed in the current system profile; Sway injection also needs
`swaymsg`, `wl-copy`, `wl-paste`, and `wtype` in `PATH`. For a fully provisioned development
environment, use the Nix shell:

```sh
nix develop -c cargo run -- --help
nix develop -c cargo test
```

Or enter the shell first:

```sh
nix develop
cargo run -- daemon
```

Run the full CI set locally:

```sh
nix flake check --print-build-logs
```
