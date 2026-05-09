#!/usr/bin/env python3

import argparse
import copy
import json
import logging
import os
import random
import re
import shutil
import subprocess
import sys
import unicodedata
import zipfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterable

import datasets
import numpy as np
import scipy.io.wavfile
import soundfile
import torch
import torchaudio
import yaml
from numpy.lib.format import open_memmap

import openwakeword
from openwakeword.train import Model as TrainModel
from openwakeword.data import filter_audio_paths, mmap_batch_generator
from openwakeword.utils import AudioFeatures


LOGGER = logging.getLogger("train-wakeword")

DEFAULT_MODEL_NAME = "default"
DEFAULT_TRAINING_BASE_DIR = "speaches-companion/openwakeword-train"
DEFAULT_WAKEWORD_ROOT_DIR = "speaches-companion/wakewords"
COMMON_VOICE_DATASET = "mozilla-foundation/common_voice_11_0"
COMMON_VOICE_LANG = "en"
PIPER_GENERATOR_MODEL_FILENAME = "en_US-libritts_r-medium.pt"

PRESETS = {
    "fast": {
        "n_samples": 750,
        "n_samples_val": 250,
        "steps": 3_000,
        "tts_batch_size": 28,
        "augmentation_batch_size": 8,
        "cv11_limit": 300,
        "background_duplication_rate": [1, 1, 1],
        "batch_n_per_class": {
            "sample_negative": 192,
            "adversarial_negative": 64,
            "positive": 64,
        },
        "max_negative_weight": 750,
        "target_false_positives_per_hour": 1.0,
    },
    "default": {
        "n_samples": 2_000,
        "n_samples_val": 500,
        "steps": 10_000,
        "tts_batch_size": 42,
        "augmentation_batch_size": 16,
        "cv11_limit": 1_000,
        "background_duplication_rate": [1, 1, 1],
        "batch_n_per_class": {
            "sample_negative": 256,
            "adversarial_negative": 64,
            "positive": 64,
        },
        "max_negative_weight": 1_500,
        "target_false_positives_per_hour": 0.5,
    },
    "quality": {
        "n_samples": 5_000,
        "n_samples_val": 1_000,
        "steps": 20_000,
        "tts_batch_size": 56,
        "augmentation_batch_size": 16,
        "cv11_limit": 2_500,
        "background_duplication_rate": [1, 1, 1],
        "batch_n_per_class": {
            "sample_negative": 384,
            "adversarial_negative": 96,
            "positive": 96,
        },
        "max_negative_weight": 1_500,
        "target_false_positives_per_hour": 0.2,
    },
}


@dataclass
class NegativeSet:
    train_paths: list[Path]
    train_durations: list[float]
    val_paths: list[Path]
    val_durations: list[float]


class IterDataset(torch.utils.data.IterableDataset):
    def __init__(self, generator):
        self.generator = generator

    def __iter__(self):
        return self.generator


def ensure_torchaudio_info_compat() -> None:
    if hasattr(torchaudio, "info"):
        return

    def _compat_info(path):
        info = soundfile.info(path)
        return type(
            "CompatAudioInfo",
            (),
            {"num_frames": info.frames, "sample_rate": info.samplerate},
        )()

    torchaudio.info = _compat_info


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Train and install a custom openWakeWord ONNX head for Speaches Companion."
    )
    parser.add_argument("phrase", help="Wake phrase text to synthesize and train on.")
    parser.add_argument(
        "--name",
        help="Override the filesystem-safe wakeword/profile name derived from the phrase.",
    )
    parser.add_argument(
        "--root-dir",
        type=Path,
        help="Override the Companion wakeword root directory.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        help="Keep the full training workspace at this path instead of creating a timestamped run directory.",
    )
    parser.add_argument(
        "--custom-negative-phrase",
        action="append",
        default=[],
        help="Additional phrase that should not trigger the model. Repeatable.",
    )
    parser.add_argument(
        "--preset",
        choices=sorted(PRESETS.keys()),
        default="default",
        help="Training preset. Default: %(default)s.",
    )
    parser.add_argument(
        "--log-level",
        default="INFO",
        choices=["DEBUG", "INFO", "WARNING", "ERROR"],
        help="Logging verbosity. Default: %(default)s.",
    )
    return parser.parse_args()


def configure_logging(level: str) -> None:
    logging.basicConfig(level=getattr(logging, level), format="%(message)s")


def env_path(name: str) -> Path:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(f"required environment variable {name} is unset")
    return Path(value)


def xdg_dir(env_name: str, fallback_suffix: str) -> Path:
    if env_name in os.environ and os.environ[env_name]:
        return Path(os.environ[env_name])
    return Path.home() / fallback_suffix


def default_root_dir() -> Path:
    return xdg_dir("XDG_DATA_HOME", ".local/share") / DEFAULT_WAKEWORD_ROOT_DIR


def default_training_base_dir() -> Path:
    return xdg_dir("XDG_DATA_HOME", ".local/share") / DEFAULT_TRAINING_BASE_DIR


def default_cache_dir() -> Path:
    return xdg_dir("XDG_CACHE_HOME", ".cache") / DEFAULT_TRAINING_BASE_DIR


def derive_name(phrase: str) -> str:
    normalized = unicodedata.normalize("NFKD", phrase).encode("ascii", "ignore").decode("ascii")
    normalized = normalized.lower()
    normalized = re.sub(r"\s+", "_", normalized.strip())
    normalized = re.sub(r"[^a-z0-9_-]", "", normalized)
    normalized = re.sub(r"_+", "_", normalized)
    normalized = normalized.strip("_-")
    if not normalized:
        raise ValueError("phrase does not produce a valid wakeword name after normalization")
    return normalized


def make_workspace(base_dir: Path, name: str) -> Path:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
    workspace = base_dir / f"{name}-{stamp}"
    workspace.mkdir(parents=True, exist_ok=False)
    return workspace


def stage_piper_generator(cache_dir: Path, source_dir: Path, voice_model_path: Path) -> Path:
    staged = cache_dir / f"{source_dir.name}-staged"
    staged.parent.mkdir(parents=True, exist_ok=True)
    if not staged.exists():
        shutil.copytree(source_dir, staged)
    generate_samples_py = staged / "generate_samples.py"
    os.chmod(generate_samples_py, generate_samples_py.stat().st_mode | 0o600)
    text = generate_samples_py.read_text(encoding="utf-8")
    patched = text.replace(
        "    model = torch.load(model_path)\n",
        "    model = torch.load(model_path, weights_only=False)\n",
    )
    if patched != text:
        generate_samples_py.write_text(patched, encoding="utf-8")
    model_dir = staged / "models"
    model_dir.mkdir(parents=True, exist_ok=True)
    os.chmod(model_dir, model_dir.stat().st_mode | 0o700)
    model_target = model_dir / PIPER_GENERATOR_MODEL_FILENAME
    if not model_target.exists():
        shutil.copy2(voice_model_path, model_target)
    return staged


def extract_zip_once(zip_path: Path, destination: Path) -> None:
    marker = destination / ".extracted"
    if marker.exists():
        return
    destination.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(zip_path) as archive:
        archive.extractall(destination)
    marker.write_text("\n")


def ensure_common_voice_sample(destination: Path, limit: int) -> None:
    wav_paths = sorted(destination.rglob("*.wav"))
    if len(wav_paths) >= limit:
        return

    if destination.exists():
        shutil.rmtree(destination)
    destination.mkdir(parents=True, exist_ok=True)

    dataset = datasets.load_dataset(
        COMMON_VOICE_DATASET,
        COMMON_VOICE_LANG,
        split="test",
        streaming=True,
    )
    dataset = dataset.cast_column("audio", datasets.Audio(sampling_rate=16000, mono=True))
    iterator = iter(dataset)

    for _ in range(limit):
        example = next(iterator)
        output = destination / f"{Path(example['path']).with_suffix('.wav')}"
        output.parent.mkdir(parents=True, exist_ok=True)
        wav_data = (example["audio"]["array"] * 32767).astype(np.int16)
        scipy.io.wavfile.write(output, 16000, wav_data)


def split_paths_and_durations(paths: list[str], durations: list[float], seed: int) -> tuple[list[Path], list[float], list[Path], list[float]]:
    pairs = list(zip((Path(path) for path in paths), durations, strict=False))
    rng = random.Random(seed)
    rng.shuffle(pairs)

    if len(pairs) < 2:
        raise RuntimeError("need at least two negative clips to create train/validation splits")

    split_at = max(1, int(len(pairs) * 0.2))
    split_at = min(split_at, len(pairs) - 1)

    val_pairs = pairs[:split_at]
    train_pairs = pairs[split_at:]
    train_paths, train_durations = zip(*train_pairs, strict=False)
    val_paths, val_durations = zip(*val_pairs, strict=False)
    return list(train_paths), list(train_durations), list(val_paths), list(val_durations)


def collect_negative_sets(sources: Iterable[Path]) -> NegativeSet:
    all_paths: list[str] = []
    all_durations: list[float] = []

    for source in sources:
        paths, durations = filter_audio_paths(
            [str(source)],
            min_length_secs=1.0,
            max_length_secs=60 * 30,
            duration_method="header",
            glob_filter="**/*.wav",
        )
        all_paths.extend(list(paths))
        all_durations.extend(list(durations))

    train_paths, train_durations, val_paths, val_durations = split_paths_and_durations(
        all_paths,
        all_durations,
        0,
    )

    return NegativeSet(
        train_paths=train_paths,
        train_durations=train_durations,
        val_paths=val_paths,
        val_durations=val_durations,
    )


def load_template_config(template_path: Path) -> dict:
    with template_path.open("r", encoding="utf-8") as handle:
        return yaml.safe_load(handle)


def positive_clip_dirs(workspace: Path, model_name: str) -> tuple[Path, Path]:
    model_dir = workspace / model_name
    return model_dir / "positive_train", model_dir / "positive_test"


def generated_feature_paths(workspace: Path, model_name: str) -> dict[str, Path]:
    model_dir = workspace / model_name
    return {
        "positive_train": model_dir / "positive_features_train.npy",
        "positive_test": model_dir / "positive_features_test.npy",
        "negative_train": model_dir / "negative_features_train.npy",
        "negative_test": model_dir / "negative_features_test.npy",
    }


def determine_total_length_samples(positive_test_dir: Path) -> int:
    positive_clips = sorted(positive_test_dir.glob("*.wav"))
    if len(positive_clips) < 1:
        raise RuntimeError(f"no generated positive clips found in {positive_test_dir}")

    sample_size = min(50, len(positive_clips))
    rng = random.Random(0)
    durations = []
    for path in rng.sample(positive_clips, sample_size):
        _, data = scipy.io.wavfile.read(path)
        durations.append(len(data))

    total_length = int(round(np.median(durations) / 1000) * 1000) + 12_000
    if total_length < 32_000:
        total_length = 32_000
    elif abs(total_length - 32_000) <= 4_000:
        total_length = 32_000
    return total_length


def compute_negative_feature_file(
    output_file: Path,
    audio_paths: list[Path],
    audio_durations: list[float],
    clip_size_secs: float,
    feature_extractor: AudioFeatures,
) -> None:
    if output_file.exists():
        return

    audio_dataset = datasets.Dataset.from_dict({"audio": [str(path) for path in audio_paths]})
    audio_dataset = audio_dataset.cast_column("audio", datasets.Audio(sampling_rate=16000))

    batch_size = 64
    total_rows = int(sum(audio_durations) // clip_size_secs)
    if total_rows < 1:
        raise RuntimeError(f"negative dataset for {output_file} is too small for {clip_size_secs:.2f}s clips")

    feature_shape = feature_extractor.get_embedding_shape(clip_size_secs)
    mmap = open_memmap(
        output_file,
        mode="w+",
        dtype=np.float32,
        shape=(total_rows, feature_shape[0], feature_shape[1]),
    )

    row_counter = 0
    clip_size_samples = int(16_000 * clip_size_secs)
    for index in range(0, audio_dataset.num_rows, batch_size):
        wav_data = [
            (entry["array"] * 32767).astype(np.int16)
            for entry in audio_dataset[index : index + batch_size]["audio"]
        ]
        stacked = openwakeword.data.stack_clips(wav_data, clip_size=clip_size_samples).astype(np.int16)
        features = feature_extractor.embed_clips(x=stacked, batch_size=256, ncpu=4)
        next_row = min(row_counter + features.shape[0], total_rows)
        mmap[row_counter:next_row, :, :] = features[: next_row - row_counter]
        row_counter = next_row
        mmap.flush()
        if row_counter >= total_rows:
            break

    openwakeword.data.trim_mmap(str(output_file))


def build_balanced_validation_loader(feature_paths: dict[str, Path]) -> torch.utils.data.DataLoader:
    positive = np.load(feature_paths["positive_test"])
    negative = np.load(feature_paths["negative_test"])
    labels = np.hstack((np.ones(len(positive)), np.zeros(len(negative)))).astype(np.float32)
    dataset = torch.utils.data.TensorDataset(
        torch.from_numpy(np.vstack((positive, negative))),
        torch.from_numpy(labels),
    )
    return torch.utils.data.DataLoader(dataset, batch_size=len(labels))


def build_false_positive_loader(
    false_positive_features_path: Path,
    clip_size_secs: float,
) -> tuple[torch.utils.data.DataLoader, float]:
    features = np.load(false_positive_features_path)
    hours = (features.shape[0] * clip_size_secs) / 3600.0
    labels = np.zeros(features.shape[0], dtype=np.float32)
    dataset = torch.utils.data.TensorDataset(torch.from_numpy(features), torch.from_numpy(labels))
    return torch.utils.data.DataLoader(dataset, batch_size=min(512, len(labels))), max(hours, 1e-6)


def build_training_loader(
    config: dict,
    feature_paths: dict[str, Path],
    generic_negative_features: Path,
    input_frames: int,
) -> torch.utils.data.DataLoader:
    def reshape_negative_batch(batch: np.ndarray, n_frames: int = input_frames) -> np.ndarray:
        if n_frames != batch.shape[1]:
            flattened = np.vstack(batch)
            return np.array([flattened[i : i + n_frames, :] for i in range(0, flattened.shape[0] - n_frames, n_frames)])
        return batch

    config = dict(config)
    config["feature_data_files"] = {
        "sample_negative": str(generic_negative_features),
        "positive": str(feature_paths["positive_train"]),
        "adversarial_negative": str(feature_paths["negative_train"]),
    }

    data_transforms = {"sample_negative": reshape_negative_batch}
    label_transforms = {
        "sample_negative": lambda rows: [0 for _ in rows],
        "positive": lambda rows: [1 for _ in rows],
        "adversarial_negative": lambda rows: [0 for _ in rows],
    }

    generator = mmap_batch_generator(
        config["feature_data_files"],
        n_per_class=config["batch_n_per_class"],
        data_transform_funcs=data_transforms,
        label_transform_funcs=label_transforms,
    )

    workers = max((os.cpu_count() or 1) // 2, 1)
    return torch.utils.data.DataLoader(
        IterDataset(generator),
        batch_size=None,
        num_workers=workers,
        prefetch_factor=16,
    )


def auto_train_with_hours(
    trainer: TrainModel,
    train_loader: torch.utils.data.DataLoader,
    validation_loader: torch.utils.data.DataLoader,
    false_positive_loader: torch.utils.data.DataLoader,
    *,
    steps: int,
    max_negative_weight: int,
    target_fp_per_hour: float,
    val_set_hours: float,
):
    sequences = [
        (steps, 0.0001),
        (max(steps // 10, 100), 0.00001),
        (max(steps // 10, 100), 0.000001),
    ]

    for sequence_index, (sequence_steps, learning_rate) in enumerate(sequences, start=1):
        LOGGER.info("%s", "#" * 50)
        LOGGER.info("Starting training sequence %s...", sequence_index)
        LOGGER.info("%s", "#" * 50)

        if sequence_index > 1 and trainer.best_val_fp > target_fp_per_hour:
            max_negative_weight *= 2
            LOGGER.info("Increasing weight on negative examples to reduce false positives...")

        weights = np.linspace(1, max_negative_weight, int(sequence_steps)).tolist()
        start = sequence_steps - int(sequence_steps * 0.25) if sequence_index == 1 else 1
        val_steps = np.linspace(start, sequence_steps, 20).astype(np.int64)
        trainer.train_model(
            X=train_loader,
            X_val=validation_loader,
            false_positive_val_data=false_positive_loader,
            max_steps=sequence_steps,
            negative_weight_schedule=weights,
            val_steps=val_steps,
            warmup_steps=max(sequence_steps // 5, 1),
            hold_steps=max(sequence_steps // 3, 1),
            lr=learning_rate,
            val_set_hrs=val_set_hours,
        )

    LOGGER.info("Merging checkpoints above the 90th percentile into a single model...")
    accuracy_percentile = np.percentile(trainer.history["val_accuracy"], 90)
    recall_percentile = np.percentile(trainer.history["val_recall"], 90)
    fp_percentile = np.percentile(trainer.history["val_fp_per_hr"], 10)

    scored_models = list(zip(trainer.best_models, trainer.best_model_scores, strict=False))
    models = []
    for model, score in scored_models:
        if (
            score["val_accuracy"] >= accuracy_percentile
            and score["val_recall"] >= recall_percentile
            and score["val_fp_per_hr"] <= fp_percentile
        ):
            models.append(model)

    if models:
        combined_model = trainer.average_models(models=models)
        LOGGER.info("Averaging %s high-percentile checkpoints", len(models))
    elif scored_models:
        within_budget = [
            (model, score)
            for model, score in scored_models
            if score["val_fp_per_hr"] <= target_fp_per_hour
        ]
        candidate_models = within_budget or scored_models
        combined_model, selected_score = max(
            candidate_models,
            key=lambda item: (
                float(item[1]["val_recall"]),
                float(item[1]["val_accuracy"]),
                -float(item[1]["val_fp_per_hr"]),
            ),
        )
        LOGGER.info(
            "Selected single checkpoint with recall=%s accuracy=%s fp/hr=%s",
            selected_score["val_recall"],
            selected_score["val_accuracy"],
            selected_score["val_fp_per_hr"],
        )
    else:
        combined_model = trainer.model
        LOGGER.info("No scored checkpoints available; falling back to final in-memory model")

    with torch.no_grad():
        for batch in validation_loader:
            x_val, y_val = batch[0].to(trainer.device), batch[1].to(trainer.device)
            val_predictions = combined_model(x_val)

        combined_recall = trainer.recall(val_predictions, y_val[..., None]).detach().cpu().numpy()
        combined_accuracy = trainer.accuracy(
            val_predictions,
            y_val[..., None].to(torch.int64),
        ).detach().cpu().numpy()

        combined_fp = 0
        for batch in false_positive_loader:
            x_val, y_val = batch[0].to(trainer.device), batch[1].to(trainer.device)
            val_predictions = combined_model(x_val)
            combined_fp += trainer.fp(val_predictions, y_val[..., None])

        combined_fp_per_hour = (combined_fp / val_set_hours).detach().cpu().numpy()

    LOGGER.info("")
    LOGGER.info("################")
    LOGGER.info("Final Model Accuracy: %s", combined_accuracy)
    LOGGER.info("Final Model Recall: %s", combined_recall)
    LOGGER.info("Final Model False Positives per Hour: %s", combined_fp_per_hour)
    LOGGER.info("################")
    LOGGER.info("")
    return combined_model


def write_training_metadata(destination: Path, *, phrase: str, name: str, preset: str, workspace: Path, installed_model: Path) -> None:
    metadata = {
        "phrase": phrase,
        "name": name,
        "preset": preset,
        "workspace": str(workspace),
        "installed_model": str(installed_model),
        "trained_at": datetime.now(timezone.utc).isoformat(),
    }
    destination.write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")


def export_keyword_head(trainer: TrainModel, model: torch.nn.Module, model_name: str, workspace: Path) -> Path:
    exported_model = workspace / f"{model_name}.onnx"
    model_to_save = copy.deepcopy(model).to("cpu").eval()
    input_tensor = torch.rand(trainer.input_shape)[None, :]
    LOGGER.info("####")
    LOGGER.info("Saving ONNX model as '%s'", exported_model)
    torch.onnx.export(
        model_to_save,
        (input_tensor,),
        exported_model,
        opset_version=18,
        external_data=False,
        training=torch.onnx.TrainingMode.EVAL,
    )
    return exported_model


def install_model(workspace: Path, model_name: str, root_dir: Path, phrase: str, preset: str) -> Path:
    exported_model = workspace / f"{model_name}.onnx"
    if not exported_model.exists():
        raise RuntimeError(f"expected trained ONNX model at {exported_model}")
    exported_external_data = workspace / f"{model_name}.onnx.data"

    install_dir = root_dir / model_name
    install_dir.mkdir(parents=True, exist_ok=True)
    installed_model = install_dir / "model.onnx"
    shutil.copy2(exported_model, installed_model)
    installed_external_data = install_dir / exported_external_data.name
    if exported_external_data.exists():
        shutil.copy2(exported_external_data, installed_external_data)
    else:
        stale_external_data = install_dir / f"{model_name}.onnx.data"
        if stale_external_data.exists():
            stale_external_data.unlink()
    legacy_external_data = install_dir / "model.onnx.data"
    if legacy_external_data.exists() and legacy_external_data != installed_external_data:
        legacy_external_data.unlink()
    write_training_metadata(
        install_dir / "training-metadata.json",
        phrase=phrase,
        name=model_name,
        preset=preset,
        workspace=workspace,
        installed_model=installed_model,
    )
    return installed_model


def ensure_training_shims(workspace: Path) -> Path:
    shim_dir = workspace / "_python_shims"
    shim_dir.mkdir(parents=True, exist_ok=True)
    sitecustomize = shim_dir / "sitecustomize.py"
    sitecustomize.write_text(
        """
from types import SimpleNamespace

import soundfile
import torchaudio


if not hasattr(torchaudio, "info"):
    def _compat_info(path):
        info = soundfile.info(path)
        return SimpleNamespace(num_frames=info.frames, sample_rate=info.samplerate)

    torchaudio.info = _compat_info
""".lstrip(),
        encoding="utf-8",
    )
    return shim_dir


def run_generate_and_augment(training_config: Path, workspace: Path) -> None:
    config = load_template_config(training_config)
    model_name = config["model_name"]
    positive_train_dir, positive_test_dir = positive_clip_dirs(workspace, model_name)
    feature_paths = generated_feature_paths(workspace, model_name)

    if all(path.exists() for path in feature_paths.values()):
        LOGGER.info("Reusing generated openWakeWord feature files in %s", workspace / model_name)
        return

    python_executable = Path(sys.executable)
    shim_dir = ensure_training_shims(workspace)
    env = os.environ.copy()
    env["PYTHONPATH"] = (
        str(shim_dir)
        if "PYTHONPATH" not in env or not env["PYTHONPATH"]
        else f"{shim_dir}:{env['PYTHONPATH']}"
    )

    phases = []
    if not positive_train_dir.exists() or not positive_test_dir.exists():
        phases.append("--generate_clips")
    elif not any(positive_train_dir.glob("*.wav")) or not any(positive_test_dir.glob("*.wav")):
        phases.append("--generate_clips")
    else:
        LOGGER.info("Reusing generated positive clips in %s", workspace / model_name)

    phases.append("--augment_clips")

    for phase in phases:
        command = [
            str(python_executable),
            "-m",
            "openwakeword.train",
            "--training_config",
            str(training_config),
            phase,
        ]
        LOGGER.info("Running %s", " ".join(command))
        subprocess.run(command, check=True, env=env)


def write_yaml(path: Path, data: dict) -> None:
    with path.open("w", encoding="utf-8") as handle:
        yaml.safe_dump(data, handle, sort_keys=False)


def ensure_negative_resources(cache_dir: Path, preset: dict) -> list[Path]:
    fma_dir = cache_dir / "fma_sample"
    fsd_dir = cache_dir / "fsd50k_sample"
    cv_dir = cache_dir / "cv11_test_clips"
    extract_zip_once(env_path("OPENWAKEWORD_TRAIN_FMA_SAMPLE_ZIP"), fma_dir)
    extract_zip_once(env_path("OPENWAKEWORD_TRAIN_FSD50K_SAMPLE_ZIP"), fsd_dir)
    background_dirs = [fma_dir, fsd_dir]
    try:
        ensure_common_voice_sample(cv_dir, preset["cv11_limit"])
        background_dirs.append(cv_dir)
    except Exception as error:
        LOGGER.warning(
            "skipping Common Voice sample data because it could not be loaded: %s",
            error,
        )
    return background_dirs


def prepare_config(
    *,
    phrase: str,
    model_name: str,
    workspace: Path,
    preset: dict,
    custom_negative_phrases: list[str],
    piper_path: Path,
    background_dirs: Iterable[Path],
) -> dict:
    config = load_template_config(env_path("OPENWAKEWORD_CUSTOM_MODEL_TEMPLATE"))
    config["model_name"] = model_name
    config["target_phrase"] = [phrase]
    config["custom_negative_phrases"] = list(custom_negative_phrases)
    config["n_samples"] = preset["n_samples"]
    config["n_samples_val"] = preset["n_samples_val"]
    config["tts_batch_size"] = preset["tts_batch_size"]
    config["augmentation_batch_size"] = preset["augmentation_batch_size"]
    config["piper_sample_generator_path"] = str(piper_path)
    config["output_dir"] = str(workspace)
    config["rir_paths"] = []
    config["background_paths"] = [str(path) for path in background_dirs]
    config["background_paths_duplication_rate"] = list(preset["background_duplication_rate"])
    config["feature_data_files"] = {"sample_negative": str(workspace / "negative_sample_features_train.npy")}
    config["batch_n_per_class"] = dict(preset["batch_n_per_class"])
    config["steps"] = preset["steps"]
    config["max_negative_weight"] = preset["max_negative_weight"]
    config["target_false_positives_per_hour"] = preset["target_false_positives_per_hour"]
    return config


def main() -> int:
    args = parse_args()
    configure_logging(args.log_level)
    ensure_torchaudio_info_compat()

    phrase = args.phrase.strip()
    if not phrase:
        raise SystemExit("phrase must not be empty")

    name = args.name or derive_name(phrase)
    root_dir = args.root_dir or default_root_dir()
    training_base_dir = default_training_base_dir()
    cache_dir = default_cache_dir()
    cache_dir.mkdir(parents=True, exist_ok=True)
    training_base_dir.mkdir(parents=True, exist_ok=True)

    workspace = args.output_dir or make_workspace(training_base_dir, name)
    workspace.mkdir(parents=True, exist_ok=True)
    resource_cache_dir = cache_dir / "resources"
    resource_cache_dir.mkdir(parents=True, exist_ok=True)

    preset = PRESETS[args.preset]
    piper_path = stage_piper_generator(
        cache_dir / "piper-sample-generator",
        env_path("PIPER_SAMPLE_GENERATOR_SOURCE"),
        env_path("PIPER_SAMPLE_VOICE_MODEL"),
    )

    background_dirs = ensure_negative_resources(resource_cache_dir, preset)
    config = prepare_config(
        phrase=phrase,
        model_name=name,
        workspace=workspace,
        preset=preset,
        custom_negative_phrases=args.custom_negative_phrase,
        piper_path=piper_path,
        background_dirs=background_dirs,
    )

    training_config_path = workspace / "training-config.yaml"
    write_yaml(training_config_path, config)
    run_generate_and_augment(training_config_path, workspace)

    _, positive_test_dir = positive_clip_dirs(workspace, name)
    total_length_samples = determine_total_length_samples(positive_test_dir)
    clip_size_secs = total_length_samples / 16_000.0
    LOGGER.info("Using %.2fs clips for training", clip_size_secs)

    negative_sets = collect_negative_sets(background_dirs)
    feature_extractor = AudioFeatures()
    generic_negative_features = workspace / "negative_sample_features_train.npy"
    false_positive_features = workspace / "negative_sample_features_val.npy"
    compute_negative_feature_file(
        generic_negative_features,
        negative_sets.train_paths,
        negative_sets.train_durations,
        clip_size_secs,
        feature_extractor,
    )
    compute_negative_feature_file(
        false_positive_features,
        negative_sets.val_paths,
        negative_sets.val_durations,
        clip_size_secs,
        feature_extractor,
    )

    input_shape = feature_extractor.get_embedding_shape(clip_size_secs)
    feature_paths = generated_feature_paths(workspace, name)
    train_loader = build_training_loader(
        config,
        feature_paths,
        generic_negative_features,
        input_shape[0],
    )
    validation_loader = build_balanced_validation_loader(feature_paths)
    false_positive_loader, val_set_hours = build_false_positive_loader(
        false_positive_features,
        clip_size_secs,
    )

    trainer = TrainModel(
        n_classes=1,
        input_shape=input_shape,
        model_type=config["model_type"],
        layer_dim=config["layer_size"],
        seconds_per_example=1280 * input_shape[0] / 16000,
    )
    best_model = auto_train_with_hours(
        trainer,
        train_loader,
        validation_loader,
        false_positive_loader,
        steps=config["steps"],
        max_negative_weight=config["max_negative_weight"],
        target_fp_per_hour=config["target_false_positives_per_hour"],
        val_set_hours=val_set_hours,
    )
    export_keyword_head(trainer, best_model, name, workspace)

    installed_model = install_model(workspace, name, root_dir, phrase, args.preset)
    LOGGER.info("Installed trained wakeword to %s", installed_model)
    LOGGER.info("Next: speaches-companion wakeword %s", name)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
