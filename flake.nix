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

      commonArgs = {
        pname = "speaches-scribe";
        version = "0.3.0";
        src = self;
        cargoLock.lockFile = ./Cargo.lock;
        buildInputs = [
          pkgs.xdotool
        ];

        meta = with lib; {
          description = "Speaches twin for Linux desktop dictation";
          mainProgram = "speaches-scribe";
          platforms = platforms.linux;
        };
      };

      speachesScribe = pkgs.rustPlatform.buildRustPackage (commonArgs
        // {
          doCheck = false;
          nativeBuildInputs = [
            pkgs.makeWrapper
          ];
          postInstall = ''
            wrapProgram "$out/bin/speaches-scribe" \
              --prefix PATH : ${lib.makeBinPath [pkgs.pipewire pkgs.xclip]}
          '';
        });

      fmt =
        pkgs.runCommand "speaches-scribe-fmt-check" {
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
          pname = "speaches-scribe-clippy";
          doCheck = true;
          nativeBuildInputs = [
            pkgs.clippy
          ];
          buildPhase = ''
            runHook preBuild
            touch speaches-scribe-clippy
            runHook postBuild
          '';
          checkPhase = ''
            runHook preCheck
            cargo clippy --offline --workspace --all-targets -- -D warnings
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
          pname = "speaches-scribe-test";
          doCheck = true;
          buildPhase = ''
            runHook preBuild
            touch speaches-scribe-test
            runHook postBuild
          '';
          checkPhase = ''
            runHook preCheck
            cargo test --offline --quiet
            runHook postCheck
          '';
          installPhase = ''
            runHook preInstall
            touch "$out"
            runHook postInstall
          '';
        });

      mkSubcommandApp = subcommand: description: let
        app = pkgs.writeShellApplication {
          name = "speaches-scribe-${subcommand}";
          text = ''
            exec ${speachesScribe}/bin/speaches-scribe ${subcommand} "$@"
          '';
        };
      in {
        type = "app";
        program = "${app}/bin/speaches-scribe-${subcommand}";
        meta.description = description;
      };
    in {
      packages = {
        default = speachesScribe;
        speaches-scribe = speachesScribe;
      };

      checks = {
        default = speachesScribe;
        fmt = fmt;
        clippy = clippy;
        test = tests;
      };

      apps = {
        default = {
          type = "app";
          program = "${speachesScribe}/bin/speaches-scribe";
          meta.description = "Run speaches-scribe";
        };
        speaches-scribe = {
          type = "app";
          program = "${speachesScribe}/bin/speaches-scribe";
          meta.description = "Run speaches-scribe";
        };
        daemon = mkSubcommandApp "daemon" "Run the speaches-scribe hotkey daemon";
        "dictate-live" = mkSubcommandApp "dictate-live" "Run realtime dictation";
        hotkey = mkSubcommandApp "hotkey" "Send a hotkey IPC command";
        inject = mkSubcommandApp "inject" "Type text into the focused X11 window";
        "read-aloud" = mkSubcommandApp "read-aloud" "Read selected text aloud through Speaches TTS";
        "realtime-check" = mkSubcommandApp "realtime-check" "Check Speaches realtime WebSocket readiness";
        smoke = mkSubcommandApp "smoke" "Run a microphone and STT smoke test";
        transcribe = mkSubcommandApp "transcribe" "Transcribe an audio file";
      };

      devShells.default = pkgs.mkShell {
        inputsFrom = [
          speachesScribe
          clippy
        ];
        packages = with pkgs; [
          cargo
          pipewire
          pkg-config
          rust-analyzer
          rustc
          rustfmt
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
