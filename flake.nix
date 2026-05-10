{
  description = "Speaches Companion: type what you speak and read what you mark";

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
      openWakewordAssetReleaseVersion = "v0.5.1";
      openWakewordTrainingVersion = "v0.6.0";
      piperSampleGeneratorVersion = "v2.0.0";
      openWakewordTrainingSource = pkgs.fetchFromGitHub {
        owner = "dscripka";
        repo = "openWakeWord";
        rev = openWakewordTrainingVersion;
        hash = "sha256-QsXV9REAHdP0Y0fVZuU+Gt9+gcPMB60bc3DOMDYuaDM=";
      };
      piperSampleGeneratorSource = pkgs.fetchFromGitHub {
        owner = "rhasspy";
        repo = "piper-sample-generator";
        rev = piperSampleGeneratorVersion;
        hash = "sha256-/si5216HBpQzC5/PFkzi+yPmzErrhTCAIwcgMMoOfJY=";
      };
      piperSampleVoiceModel = pkgs.fetchurl {
        url = "https://github.com/rhasspy/piper-sample-generator/releases/download/${piperSampleGeneratorVersion}/en_US-libritts_r-medium.pt";
        hash = "sha256-6V7lN3C/WYw1Sm5tv8lcyyWa7rUB01qGvop2dCmrD/Y=";
      };
      openWakewordTrainingFmaSample = pkgs.fetchurl {
        url = "https://f002.backblazeb2.com/file/openwakeword-resources/data/fma_sample.zip";
        hash = "sha256-vPa3mieeywLzId+vY1TydJVcTHcM/lthAQFmBLa/8aU=";
      };
      openWakewordTrainingFsd50kSample = pkgs.fetchurl {
        url = "https://f002.backblazeb2.com/file/openwakeword-resources/data/fsd50k_sample.zip";
        hash = "sha256-+u8Gt9Trw+WbCDoRWDN/ddt0Za881i3dPvGSZtIL8DY=";
      };
      openWakewordSharedAssets = [
        {
          name = "melspectrogram.onnx";
          hash = "sha256-uisOD4t7h1NposicsTNg/1O6xDbyiVzO2fR5+mXrF28=";
        }
        {
          name = "embedding_model.onnx";
          hash = "sha256-cNFkKQwdCV0dTuFJvF4AVDJQpzFrWfMdBWz/e9MHXB8=";
        }
      ];
      openWakewordStockModelAssets = {
        alexa = {
          name = "alexa_v0.1.onnx";
          hash = "sha256-b/VmoB0SZw6NnjxZ2jJlHbFXXRcnKmAbf4o5KD37rj4=";
        };
        timer = {
          name = "timer_v0.1.onnx";
          hash = "sha256-Nx5EU1RwopJIs7jxu7uvJSXIZBf9j3XGf88CrguWJt8=";
        };
        weather = {
          name = "weather_v0.1.onnx";
          hash = "sha256-hEHajnRomejZaVKNW61WUc3VYwecBZYniPd3UwQfYOc=";
        };
      };
      fetchOpenWakewordAsset = name: hash:
        pkgs.fetchurl {
          url = "https://github.com/dscripka/openWakeWord/releases/download/${openWakewordAssetReleaseVersion}/${name}";
          inherit hash;
        };
      mkOpenWakewordAssets = models:
        pkgs.linkFarm "speaches-companion-openwakeword-assets"
        (map
          (asset: {
            name = asset.name;
            path = fetchOpenWakewordAsset asset.name asset.hash;
          })
          (openWakewordSharedAssets
            ++ map (model: openWakewordStockModelAssets.${model}) models));
      openWakewordAssets = mkOpenWakewordAssets [
        "alexa"
        "timer"
        "weather"
      ];
      openWakewordTrainInterpreter = pkgs.python3.override {
        packageOverrides = final: prev: {
          pronouncing = prev.buildPythonPackage rec {
            pname = "pronouncing";
            version = "0.3.0";
            pyproject = true;
            src = pkgs.fetchPypi {
              inherit pname version;
              hash = "sha256-NJZkBF0Fn0QDLl/aYmIQSQsYCOSWkZSpWN0gXFJul4o=";
            };
            build-system = [
              prev.setuptools
            ];
            dependencies = [
              prev.cmudict
            ];
            pythonImportsCheck = ["pronouncing"];
          };

          audiomentations = prev.buildPythonPackage rec {
            pname = "audiomentations";
            version = "0.36.0";
            format = "setuptools";
            src = pkgs.fetchPypi {
              inherit pname version;
              hash = "sha256-7WlzwWdEw3aA7/2j04W9anbOrVbmzPR9PdP8R3L2m2U=";
            };
            nativeBuildInputs = [
              prev.setuptools
            ];
            dependencies = [
              prev.librosa
              prev.numpy
              prev.scipy
              prev.soxr
            ];
            pythonImportsCheck = ["audiomentations"];
          };

          openwakeword = prev.buildPythonPackage rec {
            pname = "openwakeword";
            version = "0.6.0";
            pyproject = true;
            src = pkgs.fetchPypi {
              inherit pname version;
              hash = "sha256-NoWNkPEYPjB0hVl6kSpOPDOEsU6pkj+D/q/658FWVWU=";
            };
            build-system = [
              prev.setuptools
            ];
            postPatch = ''
              substituteInPlace setup.py \
                --replace-fail "'tflite-runtime>=2.8.0,<3; platform_system == \"Linux\"'," ""
              python - <<'PY'
from pathlib import Path

path = Path("openwakeword/data.py")
text = path.read_text()
text = text.replace("import acoustics\n", "")
text = text.replace(
    "import mutagen\n\n\n# Load audio clips and structure into clips of the same length\n",
    """import mutagen


def _colored_noise(size, color):
    noise = np.random.normal(0.0, 1.0, size)
    if color == "white":
        return noise

    freqs = np.fft.rfftfreq(size)
    freqs[0] = 1.0
    spectrum = np.fft.rfft(noise)
    scales = {
        "pink": np.sqrt(freqs),
        "brown": freqs,
        "blue": 1.0 / np.sqrt(freqs),
        "violet": 1.0 / freqs,
    }
    spectrum /= scales.get(color, 1.0)
    colored = np.fft.irfft(spectrum, n=size)
    peak = np.max(np.abs(colored))
    if peak > 0:
        colored = colored / peak
    return colored


# Load audio clips and structure into clips of the same length
""",
)
text = text.replace(
    '                noise_clip = acoustics.generator.noise(combined_size, color=np.random.choice(noise_color))',
    '                noise_clip = _colored_noise(combined_size, np.random.choice(noise_color))',
)
text = text.replace(
    'output_names=[class_mapping])',
    'output_names=[class_mapping], dynamo=False)',
)
text = text.replace(
    'opset_version=13)',
    'opset_version=13, dynamo=False)',
)
path.write_text(text)
PY
            '';
            postInstall = ''
              package_dir=$(find "$out" -path '*/site-packages/openwakeword' -type d | head -n1)
              if [ -z "$package_dir" ]; then
                echo "failed to find installed openwakeword package dir under $out" >&2
                exit 1
              fi
              model_dir="$package_dir/resources/models"
              mkdir -p "$model_dir"
              cp ${fetchOpenWakewordAsset "melspectrogram.onnx" "sha256-uisOD4t7h1NposicsTNg/1O6xDbyiVzO2fR5+mXrF28="} \
                "$model_dir/melspectrogram.onnx"
              cp ${fetchOpenWakewordAsset "embedding_model.onnx" "sha256-cNFkKQwdCV0dTuFJvF4AVDJQpzFrWfMdBWz/e9MHXB8="} \
                "$model_dir/embedding_model.onnx"
            '';
            dependencies = [
              prev.onnxruntime
              prev.requests
              prev."scikit-learn"
              prev.scipy
              prev.tqdm
            ];
            pythonImportsCheck = ["openwakeword"];
          };
        };
      };
      openWakewordTrainPython = openWakewordTrainInterpreter.withPackages (ps: [
        ps.audiomentations
        ps.datasets
        ps.ipykernel
        ps.jupyterlab
        ps."piper-phonemize"
        ps.matplotlib
        ps.mutagen
        ps.numpy
        ps.onnxscript
        ps.openwakeword
        ps.pronouncing
        ps.requests
        ps.scipy
        ps."scikit-learn"
        ps.speechbrain
        ps.torchaudio
        ps."torch-audiomentations"
        ps.torchinfo
        ps.torchmetrics
        ps.torch
        ps.tqdm
        ps.pyyaml
        ps.webrtcvad
      ]);
      openWakewordTrain = pkgs.writeShellApplication {
        name = "openwakeword-train";
        runtimeInputs = [
          openWakewordTrainPython
          pkgs.ffmpeg
        ];
        text = ''
                    set -euo pipefail

                    workdir="''${OPENWAKEWORD_TRAIN_DIR:-''${XDG_DATA_HOME:-$HOME/.local/share}/speaches-companion/openwakeword-train}"
                    notebook_src="${openWakewordTrainingSource}/notebooks/training_models.ipynb"
                    notebook_dst="$workdir/training_models.ipynb"

                    mkdir -p "$workdir"

                    if [ ! -e "$notebook_dst" ]; then
                      cp "$notebook_src" "$notebook_dst"
                    fi

                    "${openWakewordTrainPython}/bin/python3" - "$notebook_dst" <<'PY'
          import json
          import sys
          from pathlib import Path

          path = Path(sys.argv[1])
          nb = json.loads(path.read_text())
          changed = False

          for cell in nb.get("cells", []):
              if cell.get("cell_type") != "code":
                  continue
              src = "".join(cell.get("source", []))
              if "oww = openwakeword.Model(" not in src or 'inference_framework="onnx"' in src:
                  continue
              if "    vad_threshold=0.5,\n" in src:
                  src = src.replace(
                      "    vad_threshold=0.5,\n",
                      "    vad_threshold=0.5,\n    inference_framework=\"onnx\",\n",
                      1,
                  )
              else:
                  src = src.replace(
                      "    vad_threshold=0.5\n",
                      "    vad_threshold=0.5,\n    inference_framework=\"onnx\"\n",
                      1,
                  )
              cell["source"] = src.splitlines(keepends=True)
              changed = True

          if changed:
              path.write_text(json.dumps(nb, indent=1))
          PY

                    exec ${openWakewordTrainPython}/bin/python -m jupyter lab "$notebook_dst" "$@"
        '';
      };

      trainWakeword = pkgs.writeShellApplication {
        name = "train-wakeword";
        runtimeInputs = [
          openWakewordTrainPython
          pkgs.ffmpeg
        ];
        text = ''
          export OPENWAKEWORD_CUSTOM_MODEL_TEMPLATE="${openWakewordTrainingSource}/examples/custom_model.yml"
          export PIPER_SAMPLE_GENERATOR_SOURCE="${piperSampleGeneratorSource}"
          export PIPER_SAMPLE_VOICE_MODEL="${piperSampleVoiceModel}"
          export OPENWAKEWORD_TRAIN_FMA_SAMPLE_ZIP="${openWakewordTrainingFmaSample}"
          export OPENWAKEWORD_TRAIN_FSD50K_SAMPLE_ZIP="${openWakewordTrainingFsd50kSample}"
          exec ${openWakewordTrainPython}/bin/python ${./scripts/train_wakeword.py} "$@"
        '';
      };

      commonArgs = {
        pname = "speaches-companion";
        version = "0.4.0";
        src = self;
        cargoLock.lockFile = ./Cargo.lock;
        buildFeatures = [
          "debug-recordings"
        ];
        buildInputs = [
          pkgs.xdotool
        ];

        meta = with lib; {
          description = "Type what you speak and read what you mark through Speaches";
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
              --prefix PATH : ${lib.makeBinPath [pkgs.coreutils pkgs.ffmpeg pkgs.pipewire pkgs.sway pkgs.wl-clipboard pkgs.wtype pkgs.xclip]}
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
        openwakeword-train = openWakewordTrain;
        train-wakeword = trainWakeword;
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
        "openwakeword-train" = {
          type = "app";
          program = "${openWakewordTrain}/bin/openwakeword-train";
          meta.description = "Launch the upstream openWakeWord training notebook in a cached local environment";
        };
        "train-wakeword" = {
          type = "app";
          program = "${trainWakeword}/bin/train-wakeword";
          meta.description = "Train and install a custom openWakeWord ONNX head for Speaches Companion";
        };
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
