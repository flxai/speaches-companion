# Recovered Implementation Plan

This is the implementation plan recovered from [design.md](./design.md), updated for the hard requirement that partial transcripts must appear while recording.

That requirement changes the backend choice:

- `POST /v1/audio/transcriptions` with `stream=true` is not enough as the primary path
- the first implementation must support live transcript updates before hotkey release
- final insertion and live partial display must be treated as separate concerns

## What Changes From `design.md`

1. Realtime streaming moves to the front.
   Start with `/v1/realtime?...&intent=transcription`.

2. Live partial display is a first-class feature.
   Show partial transcript in an overlay or HUD while recording. Do not insert partials into the target app.

3. HTTP transcription becomes a fallback path, not the default.
   Keep `POST /v1/audio/transcriptions` with `stream=true` as a backup strategy and as a useful test harness.

4. Marker support moves later.
   Do not implement `…` in the first cut. Add shell-native integration first.

5. Context detection gets simplified.
   Do not start with PTY/process/editor detection. Start with focus snapshot + allowlist.

6. The first target is X11 on this machine.
   Current environment has `DISPLAY=:1`, `alacritty`, `xclip`, `xdotool`, `notify-send`, `arecord`, and `pw-record`. It does not currently expose `wl-copy` or `wtype`.

## Why This Is The Right Recovery

The core requirement is now:

- hotkey down: start recording
- while held: show partial transcript updates
- hotkey up: finalize transcript and insert safely

This means the earlier SSE-first plan is no longer sufficient, because it only begins once there is a completed audio chunk to upload.

Speaches exposes two relevant streaming surfaces:

1. Realtime transcription over `/v1/realtime`
2. Completed-audio transcription over `POST /v1/audio/transcriptions` with `stream=true`

For true live partials, the Realtime path must be attempted first.

There is one important implementation risk:

- current Speaches documentation describes transcription delta updates in Realtime mode
- the current upstream source tree is not obviously emitting those deltas in the realtime transcription wrapper; the server-side path I inspected clearly emits `completed`, but delta support is ambiguous

So the correct recovery is not "assume deltas work". The correct recovery is:

1. spike the Realtime path immediately
2. verify whether the deployed Speaches instance emits usable live deltas
3. if not, fall back to a client-managed rolling-chunk strategy that still delivers live draft text

## Product MVP

The first real deliverable should behave like this:

1. User holds a global hotkey.
2. Daemon snapshots focused window state.
3. Daemon starts microphone streaming to Speaches.
4. While the key is held, daemon renders partial transcript updates in a small overlay/HUD.
5. User releases the hotkey.
6. Daemon flushes/finalizes the current utterance and obtains final transcript text.
7. Daemon copies the final text to the clipboard.
8. If the focused window is still the same and is allowlisted, daemon pastes the final text.
9. Otherwise daemon only notifies and leaves the final text in the clipboard.

No marker. No partial text insertion. No editor-specific logic.

## Explicit Non-Goals For MVP

- No universal `…` marker behavior
- No Vim/Neovim/TUI detection
- No SSH detection
- No Wayland-first support
- No shell widget in the first cut
- No partial insertion into the target app while the key is still held

## Recommended Phase Order

### Phase 0: Realtime Capability Spike

Build a one-shot CLI that:

```text
dictate-live
```

Behavior:

1. Connect to Speaches Realtime transcription mode.
2. Stream microphone PCM audio while a key is held or for a fixed short session.
3. Log all server events.
4. Record whether live delta events appear before final completion.
5. Save raw event traces for inspection.

Acceptance:

- We can reliably stream local mic audio to Speaches.
- Speaches auth/base URL/model config works for Realtime.
- We know whether actual live deltas are available in the deployed server.

### Phase 1A: Push-to-Talk Daemon, Realtime Path

Use this phase if Realtime emits usable partial updates.

Build the first daemon with:

- global hotkey
- press-to-start recording
- release-to-finalize
- Speaches Realtime transcription
- live partial overlay
- clipboard write
- desktop notifications

Behavior:

1. On hotkey down, snapshot focused window ID/class/title.
2. Start audio capture and stream to Speaches.
3. Render partial transcript updates in a small HUD.
4. On hotkey up, flush/finalize the current utterance.
5. Copy final transcript to clipboard.
6. Notify success or failure.

Acceptance:

- Partial transcript is visible during recording.
- Final transcript is available immediately after release.
- Failure mode is safe: nothing is typed blindly.

### Phase 1B: Rolling-Chunk Fallback Path

Use this phase if Speaches Realtime does not emit usable partial deltas in practice.

Approach:

1. Capture mic audio continuously.
2. Every N milliseconds, export a rolling overlapping audio slice.
3. Send that slice to `POST /v1/audio/transcriptions`.
4. Update the on-screen hypothesis from the newest result.
5. On release, run one final transcription on the complete utterance.

Notes:

- This is not as elegant as true Realtime.
- It still satisfies the product requirement for live draft text.
- The overlap logic must prevent the visible hypothesis from jumping backward too aggressively.

### Phase 2: Safe Auto-Paste For Alacritty On X11

Add an allowlisted insertion route:

- if focused window at insert time matches the window captured at hotkey-down
- and WM_CLASS is `Alacritty`
- then paste using clipboard + `xdotool`

Behavior:

1. Save previous clipboard.
2. Put transcript into clipboard.
3. Send configured paste shortcut.
4. Restore previous clipboard after a short delay.

Guardrails:

- If focus changed between start and insertion, do not paste.
- If window class is unknown, do not paste.
- If paste command fails, keep transcript in clipboard and notify.

Acceptance:

- Works in Alacritty shell prompts.
- Does not type character-by-character.
- Does not paste into a different app if focus changed.

### Phase 3: Shell-Native Integration

Add zsh first.

Do this before any inline marker logic.

Approach:

- daemon writes transcript to a local Unix socket or file
- zsh widget reads the transcript and inserts into `BUFFER`
- widget refreshes prompt via `zle reset-prompt`

This is the first truly robust terminal insertion mode.

Acceptance:

- Dictation lands in shell edit buffer without fake typing.
- Shell history/edit state stays intact.

### Phase 4: Marker Support, But Only Inside Shell Integration

If we still want `…`, add it only inside the zsh integration.

Do not add system-wide marker typing.

Reason:

- shell integration already knows it is operating on a shell buffer
- marker insertion/removal is predictable there
- doing this through global synthetic keys is fragile

### Phase 5: Tightening Realtime Behavior

Once the first live-partials version works, improve:

- finalization latency
- overlay quality
- overlap/hypothesis stability
- interruption and cancellation behavior
- optional shell/editor integrations

## Minimal Architecture

Keep the first cut small:

```text
trec-daemon
├── hotkey
├── recorder
├── speaches_client
├── live_hypothesis
├── clipboard
├── paste_router
└── notifier
```

Do not build a full context detector yet.

For MVP, the only routing decision needed is:

```text
same focused window + allowlisted app -> paste
otherwise                        -> clipboard + notify
```

## Interfaces To Keep Stable

Even in the spike, define these boundaries clearly:

### Recorder

Input:

- start
- stop

Output:

- audio chunks or temp file path
- duration
- sample rate

### Speaches Client

Input:

- audio chunks and/or audio file path
- model
- language
- backend mode

Output:

- stream of partial transcript events
- final transcript

### Live Hypothesis

Input:

- partial transcript updates
- session state

Output:

- current visible draft text
- final committed text

### Insertion Router

Input:

- final transcript
- focus snapshot from hotkey-down
- current focus at insertion time

Output:

- pasted
- copied_only
- failed

## Technical Decisions

### Audio Format

For the first live-partials implementation:

- use the format required by the selected backend path
- for Realtime, prefer 24 kHz mono PCM16
- for HTTP fallback, keep temp WAV generation simple and deterministic

### Turn Detection

There are two viable modes:

1. Realtime with server VAD enabled
2. Client-managed chunking/finalization

Do not assume "disable VAD and commit only on release" will satisfy live partials. That approach usually delays transcription until commit.

### Insertion Policy

Default policy:

```text
unknown context -> clipboard + notify
```

This should remain the default even after later integrations exist.

## First Concrete Tasks

1. Prove recording with `pw-record` or `arecord`.
2. Prove Speaches Realtime on the deployed server.
3. Confirm whether live delta events actually arrive.
4. If not, build rolling-chunk fallback.
5. Add a live on-screen hypothesis view.
6. Add clipboard write.
7. Add notifications.
8. Add global hotkey.
9. Add Alacritty paste allowlist.

## Open Questions

These are the only questions that should block implementation choices:

1. Which exact hotkey should we reserve?
2. Which Speaches model do we want by default?
3. Should the first auto-paste target be only `Alacritty`, or also other terminals?
4. Does the deployed Speaches server actually provide usable live delta updates for our session mode?
5. If not, do we accept the rolling-chunk fallback as v1, or do we need to patch/fork Speaches?

## Sources

- Speaches README: https://github.com/speaches-ai/speaches
- Speaches Realtime docs: https://speaches.ai/usage/realtime-api/
- Speaches Speech-to-Text docs: https://speaches.ai/usage/speech-to-text/
- OpenAI Speech-to-Text guide: https://platform.openai.com/docs/guides/speech-to-text
- OpenAI Realtime transcription guide: https://platform.openai.com/docs/guides/realtime-transcription
