{
  description = "Linux realtime dictation hotkey client";

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
        pname = "trec";
        version = "0.2.0";
        src = self;
        cargoLock.lockFile = ./Cargo.lock;
        buildInputs = [
          pkgs.xdotool
          pkgs.libx11
        ];

        meta = with lib; {
          description = "Linux realtime dictation hotkey client";
          mainProgram = "trec";
          platforms = platforms.linux;
        };
      };

      trec = pkgs.rustPlatform.buildRustPackage (commonArgs
        // {
          doCheck = false;
          nativeBuildInputs = [
            pkgs.makeWrapper
          ];
          postInstall = ''
            wrapProgram "$out/bin/trec" \
              --prefix PATH : ${lib.makeBinPath [pkgs.pipewire]}
          '';
        });

      fmt =
        pkgs.runCommand "trec-fmt-check" {
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
          pname = "trec-clippy";
          doCheck = true;
          nativeBuildInputs = [
            pkgs.clippy
          ];
          buildPhase = ''
            runHook preBuild
            touch trec-clippy
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
          pname = "trec-test";
          doCheck = true;
          buildPhase = ''
            runHook preBuild
            touch trec-test
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
          name = "trec-${subcommand}";
          text = ''
            exec ${trec}/bin/trec ${subcommand} "$@"
          '';
        };
      in {
        type = "app";
        program = "${app}/bin/trec-${subcommand}";
        meta.description = description;
      };
    in {
      packages = {
        default = trec;
        trec = trec;
      };

      checks = {
        default = trec;
        fmt = fmt;
        clippy = clippy;
        test = tests;
      };

      apps = {
        default = {
          type = "app";
          program = "${trec}/bin/trec";
          meta.description = "Run trec";
        };
        trec = {
          type = "app";
          program = "${trec}/bin/trec";
          meta.description = "Run trec";
        };
        daemon = mkSubcommandApp "daemon" "Run the trec hotkey daemon";
        "dictate-live" = mkSubcommandApp "dictate-live" "Run realtime dictation";
        hotkey = mkSubcommandApp "hotkey" "Send a hotkey IPC command";
        inject = mkSubcommandApp "inject" "Type text into the focused X11 window";
        smoke = mkSubcommandApp "smoke" "Run a microphone and STT smoke test";
        transcribe = mkSubcommandApp "transcribe" "Transcribe an audio file";
      };

      devShells.default = pkgs.mkShell {
        inputsFrom = [
          trec
          clippy
        ];
        packages = with pkgs; [
          cargo
          pipewire
          pkg-config
          rust-analyzer
          rustc
          rustfmt
          xdotool
        ];
        shellHook = ''
          export LIBRARY_PATH="${lib.makeLibraryPath [pkgs.xdotool pkgs.libx11]}''${LIBRARY_PATH:+:''${LIBRARY_PATH}}"
          export LD_LIBRARY_PATH="${lib.makeLibraryPath [pkgs.xdotool pkgs.libx11]}''${LD_LIBRARY_PATH:+:''${LD_LIBRARY_PATH}}"
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
