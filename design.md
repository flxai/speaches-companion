# Linux Global Dictation → Speaches → Alacritty/Terminal Insertion

## Goal

Build a **global push-to-talk dictation daemon** for Linux.

Core flow:

```text
global hotkey
→ record mic
→ optional inline marker: …
→ send audio to Speaches
→ receive transcript
→ insert transcript robustly
```

Speaches is a suitable backend because it is OpenAI API-compatible, supports transcription, and uses `faster-whisper` for STT.

Source: <https://github.com/speaches-ai/speaches>

---

## 1. Design principles

### Main rule

Do **not** pretend terminal insertion is universal.

A terminal may currently contain:

```text
shell prompt
vim/neovim
nano
fzf
python/ipython REPL
ssh session
password prompt
TUIs
```

Each interprets keystrokes differently.

So use a layered insertion strategy:

```text
best known context → direct/safe insertion
unknown context    → clipboard + notification
```

### Avoid

```text
type … → cursor left → type transcript → delete …
```

as the universal mechanism.

It is acceptable only in trusted contexts, mainly shell prompt.

---

## 2. Architecture

```text
dictation-daemon
├── hotkey listener
├── audio recorder
├── VAD / silence detector              optional
├── Speaches client
├── context detector
├── insertion router
│   ├── shell integration
│   ├── clipboard paste
│   ├── Wayland typing fallback
│   ├── X11 typing fallback
│   └── no-insert fallback
├── marker renderer
└── notification/error layer
```

---

## 3. Backend: Speaches

### Required config

```toml
[backend]
type = "speaches"
base_url = "http://localhost:8000"
api_key = "optional-or-required"
model = "Systran/faster-whisper-small"
language = "auto"
```

### API behavior

Use OpenAI-compatible transcription endpoint:

```text
POST /v1/audio/transcriptions
multipart:
  file=@recording.wav
  model=<model>
  language=<optional>
```

Extension path: streaming transcription later.

Source: <https://github.com/speaches-ai/speaches>

---

## 4. Recording UX

### MVP UX

```text
press hotkey down
→ insert marker "…" only if safe context
→ record

release hotkey
→ transcribe
→ replace/remove marker
→ insert final transcript
```

### Better default

Use **no inline marker** unless context is known safe.

Instead:

```text
system notification / tiny status window:
Recording…
Transcribing…
Inserted.
```

Reason: Alacritty does not expose a rich plugin/overlay API; its IPC controls running instances but is not a text-rendering overlay API.

Source: <https://man.archlinux.org/man/alacritty-msg.1.en>

---

## 5. Marker strategy: `…`

### Safe-context marker behavior

Only in shell prompt / known line editor:

```text
1. insert "…"
2. move cursor left
3. after transcription, paste transcript
4. delete forward once
```

State example:

```text
…|
|…
hello|…
hello|
```

### Marker constraints

Use only if:

```text
active app == terminal
foreground process == shell
not inside ssh
not inside vim/nvim/nano/fzf/tmux-copy-mode/password prompt
```

Fallback if uncertain:

```text
copy transcript to clipboard
notify user
```

---

## 6. Insertion router

Priority order:

### 6.1 Shell-native insertion — best

For zsh/bash/fish, create shell widgets/functions.

Daemon sends transcript to shell integration via local socket/FIFO.

Example concept:

```text
~/.cache/dictation/latest.txt
or
unix socket: /run/user/$UID/dictation.sock
```

Shell widget inserts text into current line buffer.

Pros:

```text
does not fake keystrokes
does not rely on Delete/backspace
works with shell editing state
```

Cons:

```text
requires shell setup
not universal
```

---

### 6.2 Bracketed paste / terminal paste — good fallback

Clipboard-paste into terminal is safer than simulated typing.

Bracketed paste wraps pasted text so shell/editor can distinguish paste from typed keys.

Source: <https://invisible-island.net/xterm/xterm-paste64.html>

Flow:

```text
save clipboard
set clipboard = transcript
send paste shortcut
restore clipboard after delay
```

Use this for:

```text
shell
most REPLs
many editors
```

But still avoid password prompts and unknown TUIs.

---

### 6.3 Wayland typing fallback

Use `wtype` only when clipboard paste is unavailable or explicitly enabled.

`wtype` is a Wayland virtual-keyboard typing tool; support depends on compositor/protocol support.

Source: <https://github.com/atx/wtype>

Config:

```toml
[insertion.wayland]
method = "wtype"
enabled = false
```

Default disabled.

---

### 6.4 X11 typing fallback

Can be added with `xdotool`, but treat as lower priority than clipboard paste.

Default:

```toml
[insertion.x11]
method = "clipboard_paste"
```

---

### 6.5 Unknown context

Do not auto-insert.

```text
copy transcript to clipboard
notify: "Dictation copied; not inserted because active context is unknown."
```

This prevents damage in Vim, SSH, password prompts, etc.

---

## 7. Context detector

### Inputs

```text
display server: Wayland/X11
focused window class/title
active terminal emulator
foreground process in terminal PTY
tmux/screen status
ssh nesting detection
editor detection
```

### Context classes

```rust
enum Context {
    ShellPrompt { shell: ShellKind },
    TerminalUnknown,
    Editor(EditorKind),
    Tui(String),
    SshRemote,
    PasswordPromptMaybe,
    NonTerminalApp,
}
```

### Policy

```rust
match context {
    ShellPrompt => ShellNativeOrBracketedPaste,
    NonTerminalApp => ClipboardPaste,
    Editor(Neovim) => FutureNeovimRpc,
    TerminalUnknown | Tui(_) | SshRemote | PasswordPromptMaybe => ClipboardOnlyNotify,
}
```

---

## 8. Config schema

```toml
[hotkey]
mode = "push_to_talk"
key = "Super+Space"

[backend]
type = "speaches"
base_url = "http://localhost:8000"
api_key_env = "SPEACHES_API_KEY"
model = "Systran/faster-whisper-small"
language = "auto"

[audio]
sample_rate = 16000
channels = 1
format = "wav"
vad = false

[ui]
marker = "…"
marker_mode = "safe-context-only"
notify = true

[insertion]
default = "clipboard_then_paste"
unknown_context = "clipboard_notify"
restore_clipboard = true

[insertion.shell]
enabled = true
socket = "/run/user/%UID/dictation.sock"

[insertion.wayland]
typing_fallback = "wtype"
enabled = false

[insertion.x11]
typing_fallback = "xdotool"
enabled = false
```

---

## 9. MVP implementation sequence

### Phase 0 — spike

Build CLI:

```bash
dictate-once --backend speaches --copy
```

Does:

```text
record 5s
send to Speaches
print transcript
copy transcript
```

No hotkey. No insertion.

---

### Phase 1 — push-to-talk daemon

Add:

```text
global hotkey
press/release recording
Speaches transcription
clipboard result
desktop notification
```

Acceptance:

```text
works in any app without auto-insert risk
```

---

### Phase 2 — terminal paste

Add focused-window detection.

If active app is Alacritty:

```text
set clipboard
send Ctrl+Shift+V or configured paste shortcut
restore clipboard
```

Alacritty supports configurable key bindings and paste actions in config, but not arbitrary plugin overlays.

Source: <https://alacritty.org/config-alacritty.html>

Acceptance:

```text
works at shell prompt
does not type char-by-char
```

---

### Phase 3 — safe `…` marker

Only for shell prompt.

Flow:

```text
insert …
left
record
paste transcript
delete marker
```

Acceptance:

```text
does not trigger in vim/nvim/fzf/ssh
can be disabled globally
```

---

### Phase 4 — shell integration

Add zsh first.

zsh widget:

```text
start dictation
show marker in BUFFER
on result:
  remove marker
  insert transcript into BUFFER
  zle reset-prompt
```

This is the robust version of the marker idea.

Then add:

```text
bash/readline
fish
```

---

### Phase 5 — editor integrations

Optional.

```text
Neovim RPC
VS Code command
Emacs server
JetBrains maybe later
```

Each integration bypasses fake typing.

---

## 10. State machine

```text
Idle
 └─ hotkey_down → Recording

Recording
 ├─ hotkey_up → Transcribing
 ├─ cancel_key → Cancelled
 └─ error → Error

Transcribing
 ├─ success → Inserting
 └─ failure → Error

Inserting
 ├─ success → Idle
 └─ unsafe_context → ClipboardOnly

ClipboardOnly
 └─ notify → Idle

Cancelled/Error
 └─ cleanup marker/audio/temp files → Idle
```

---

## 11. Safety rules

Never auto-insert when:

```text
terminal foreground process unknown
inside ssh
inside password/passphrase prompt
screen locked
clipboard unavailable
transcript empty
transcript contains suspicious control chars
```

Sanitize transcript:

```text
remove NUL
normalize newlines
optionally strip trailing period
optionally append space
```

---

## 12. Extension hooks

Future agents can add:

```text
streaming partial transcript
live volume indicator
tmux pane detection
Hyprland/Sway/KDE/GNOME adapters
Neovim RPC adapter
per-app policies
local Whisper fallback
translation mode
command mode: "run this"
post-processing via LLM
```

---

## Recommended MVP

Build this first:

```text
push-to-talk
→ Speaches final transcription
→ clipboard paste into Alacritty only when safe
→ otherwise clipboard + notification
```

Then add the `…` marker **only through shell integration**, not generic fake typing.
