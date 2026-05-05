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
