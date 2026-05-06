# trec

Linux realtime dictation hotkey client for Speaches.

## Running

Run the default CLI from the flake:

```sh
nix run . -- --help
nix run . -- daemon
```

The `.` is required. Without it, `nix run -- daemon` treats `daemon` as the flake to run.

Subcommand aliases are also exposed as flake apps:

```sh
nix run .#daemon
nix run .#smoke -- --record-seconds 2 --response-format text
nix run .#hotkey -- down
nix run .#hotkey -- up
```

The daemon uses rolling HTTP dictation: while the hotkey is held, audio is
recorded and partial transcripts are collected. On release, the full recording
is transcribed and injected into the focused X11 window. If focus changes during
a recording, `trec` stops editing the target window instead of sending
Backspaces to the wrong place.

On startup the daemon starts continuous `pw-record` capture before it accepts
hotkey-driven dictation. If that capture cannot start, daemon startup fails
instead of falling back to first-use recording. The daemon also attempts a short
silent transcription request to warm the Speaches model/cache. While idle it
keeps a rolling pre-roll window for recovery/debugging, but STT requests use a
short capped pre-roll view with leading and trailing silence trimmed. When a
recording starts, treat the listening marker as the ready-to-speak signal.

Text payloads are injected through libxdo text entry with active modifiers
temporarily cleared. Speculative replacement uses Backspace for the portion that
changed.

Inline partial injection is enabled by default. If the trigger binding keeps a
modifier physically held while dictating, such as some i3 `$sup+d` bindings,
fake typing can trigger window-manager shortcuts. Disable live partial insertion
for those bindings:

```sh
nix run . -- daemon --no-inline-partials
```

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

To preserve the accepted partial and final transcripts without writing debug
audio into a project directory:

```sh
nix run . -- daemon --transcript-dir target/trec-transcripts
```

Speaches SSE transcription responses can be tested with:

```sh
nix run . -- transcribe ./audio.wav --response-format text --stream
nix run . -- daemon --stream-response
```

## Development

`trec` links to `libxdo`, so raw `cargo run` needs the Nix development shell:

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
