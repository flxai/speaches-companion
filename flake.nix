{
  description = "Speaches twin for desktop dictation";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    systems = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    eachSystem = nixpkgs.lib.genAttrs systems;
    forSystem = system: let
      pkgs = nixpkgs.legacyPackages.${system};
      lib = pkgs.lib;
      fetchOpenWakewordAsset = name: hash:
        pkgs.fetchurl {
          url = "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/${name}";
          inherit hash;
        };

      openWakewordAssets = pkgs.linkFarm "speaches-companion-openwakeword-assets" [
        {
          name = "melspectrogram.onnx";
          path = fetchOpenWakewordAsset "melspectrogram.onnx" "sha256-uisOD4t7h1NposicsTNg/1O6xDbyiVzO2fR5+mXrF28=";
        }
        {
          name = "embedding_model.onnx";
          path = fetchOpenWakewordAsset "embedding_model.onnx" "sha256-cNFkKQwdCV0dTuFJvF4AVDJQpzFrWfMdBWz/e9MHXB8=";
        }
        {
          name = "alexa_v0.1.onnx";
          path = fetchOpenWakewordAsset "alexa_v0.1.onnx" "sha256-b/VmoB0SZw6NnjxZ2jJlHbFXXRcnKmAbf4o5KD37rj4=";
        }
        {
          name = "timer_v0.1.onnx";
          path = fetchOpenWakewordAsset "timer_v0.1.onnx" "sha256-Nx5EU1RwopJIs7jxu7uvJSXIZBf9j3XGf88CrguWJt8=";
        }
        {
          name = "weather_v0.1.onnx";
          path = fetchOpenWakewordAsset "weather_v0.1.onnx" "sha256-hEHajnRomejZaVKNW61WUc3VYwecBZYniPd3UwQfYOc=";
        }
      ];

      commonArgs = {
        pname = "speaches-companion";
        version = "0.3.0";
        src = self;
        cargoLock.lockFile = ./Cargo.lock;
        buildFeatures = [
          "debug-recordings"
        ];
        buildInputs = [
          pkgs.xdotool
        ];

        meta = with lib; {
          description = "Speaches twin for Linux desktop dictation";
          mainProgram = "speaches-companion";
          platforms = platforms.linux;
        };
      };

      speachesCompanion = pkgs.rustPlatform.buildRustPackage (commonArgs
        // {
          doCheck = false;
          nativeBuildInputs = [
            pkgs.makeWrapper
          ];
          postInstall = ''
            wrapProgram "$out/bin/speaches-companion" \
              --prefix PATH : ${lib.makeBinPath [pkgs.ffmpeg pkgs.pipewire pkgs.sway pkgs.wl-clipboard pkgs.wtype pkgs.xclip]}
          '';
        });

      fmt =
        pkgs.runCommand "speaches-companion-fmt-check" {
          nativeBuildInputs = [
            pkgs.cargo
            pkgs.rustfmt
          ];
          src = self;
        } ''
          cp -r "$src" source
          chmod -R +w source
          cd source
          cargo fmt --check
          touch "$out"
        '';

      clippy = pkgs.rustPlatform.buildRustPackage (commonArgs
        // {
          pname = "speaches-companion-clippy";
          doCheck = true;
          nativeBuildInputs = [
            pkgs.clippy
          ];
          buildPhase = ''
            runHook preBuild
            touch speaches-companion-clippy
            runHook postBuild
          '';
          checkPhase = ''
            runHook preCheck
            cargo clippy --offline --workspace --all-targets --features debug-recordings -- -D warnings
            runHook postCheck
          '';
          installPhase = ''
            runHook preInstall
            touch "$out"
            runHook postInstall
          '';
        });

      tests = pkgs.rustPlatform.buildRustPackage (commonArgs
        // {
          pname = "speaches-companion-test";
          doCheck = true;
          buildPhase = ''
            runHook preBuild
            touch speaches-companion-test
            runHook postBuild
          '';
          checkPhase = ''
            runHook preCheck
            cargo test --offline --quiet --features debug-recordings
            runHook postCheck
          '';
          installPhase = ''
            runHook preInstall
            touch "$out"
            runHook postInstall
          '';
        });

      mkSubcommandApp = subcommand: description: extraArgs: let
        app = pkgs.writeShellApplication {
          name = "speaches-companion-${subcommand}";
          text = ''
            exec ${speachesCompanion}/bin/speaches-companion ${subcommand} ${lib.escapeShellArgs extraArgs} "$@"
          '';
        };
      in {
        type = "app";
        program = "${app}/bin/speaches-companion-${subcommand}";
        meta.description = description;
      };
    in {
      packages = {
        default = speachesCompanion;
        speaches-companion = speachesCompanion;
        openwakeword-assets = openWakewordAssets;
      };

      checks = {
        default = speachesCompanion;
        fmt = fmt;
        clippy = clippy;
        test = tests;
      };

      apps = {
        default = {
          type = "app";
          program = "${speachesCompanion}/bin/speaches-companion";
          meta.description = "Run speaches-companion";
        };
        speaches-companion = {
          type = "app";
          program = "${speachesCompanion}/bin/speaches-companion";
          meta.description = "Run speaches-companion";
        };
        daemon = mkSubcommandApp "daemon" "Run the speaches-companion hotkey daemon" [];
        "dictate-live" = mkSubcommandApp "dictate-live" "Run realtime dictation" [];
        hotkey = mkSubcommandApp "hotkey" "Send a hotkey IPC command" [];
        inject = mkSubcommandApp "inject" "Type text into the focused desktop window" [];
        "read-aloud" = mkSubcommandApp "read-aloud" "Read selected text aloud through Speaches TTS" [];
        "realtime-check" = mkSubcommandApp "realtime-check" "Check Speaches realtime WebSocket readiness" [];
        smoke = mkSubcommandApp "smoke" "Run a microphone and STT smoke test" [];
        transcribe = mkSubcommandApp "transcribe" "Transcribe an audio file" [];
        wakeword =
          mkSubcommandApp
          "wakeword"
          "Run hands-free wake-word dictation"
          [
            "--assets-dir"
            "${openWakewordAssets}"
          ];
      };

      devShells.default = pkgs.mkShell {
        inputsFrom = [
          speachesCompanion
          clippy
        ];
        packages = with pkgs; [
          cargo
          ffmpeg
          pipewire
          pkg-config
          rust-analyzer
          rustc
          rustfmt
          sway
          wl-clipboard
          wtype
          xclip
          xdotool
        ];
        shellHook = ''
          export LIBRARY_PATH="${lib.makeLibraryPath [pkgs.xdotool]}''${LIBRARY_PATH:+:''${LIBRARY_PATH}}"
          export LD_LIBRARY_PATH="${lib.makeLibraryPath [pkgs.xdotool]}''${LD_LIBRARY_PATH:+:''${LD_LIBRARY_PATH}}"
        '';
      };

      formatter = pkgs.alejandra;
    };
  in {
    packages = eachSystem (system: (forSystem system).packages);
    checks = eachSystem (system: (forSystem system).checks);
    apps = eachSystem (system: (forSystem system).apps);
    devShells = eachSystem (system: (forSystem system).devShells);
    formatter = eachSystem (system: (forSystem system).formatter);
  };
}
