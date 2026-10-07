#!/usr/bin/env python3
"""Spool daemon for `[karaoke] backend = "spool"`: separates whole tracks
with a Roformer model (python-audio-separator) and analyses them (Essentia:
tempo, key, danceability, moods, genres), keeping every model loaded
between tracks.

stemsd writes `in/<key>.wav` (float32 stereo 44.1 kHz) and `in/<key>.json`;
this daemon claims the job by moving the json to `work/`, writes
`out/<key>.flac` (the instrumental, exactly as long as the mix) and
`out/<key>.json` last. See crates/stemsd/src/spool.rs.

    separator.py --spool DIR --models DIR [--model NAME] [--dummy]

`--dummy` skips the models (instrumental = half the mix, no analysis) to
test the plumbing on a machine without a GPU.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import signal
import sys
import tempfile
import time
import traceback
from pathlib import Path

import numpy as np
import soundfile as sf

RATE = 44_100
DEFAULT_MODEL = "model_bs_roformer_ep_317_sdr_12.9755.ckpt"
log = logging.getLogger("separator")


class RoformerSeparator:
    """python-audio-separator with output scaling defeated.

    audio-separator scales an output whose peak exceeds
    `normalization_threshold`. stems computes vocals = mix − instrumental,
    which only works if the instrumental keeps the mix's exact scale, so the
    mix goes in at half level with the threshold at 1.0 (never reached) and
    the result is doubled again.
    """

    HEADROOM = 0.5

    def __init__(self, model: str, model_dir: Path):
        from audio_separator.separator import Separator

        self.tmp = Path(tempfile.mkdtemp(prefix="stems-sep-"))
        self.model = model
        self.sep = Separator(
            model_file_dir=str(model_dir),
            output_dir=str(self.tmp),
            output_format="FLAC",
            normalization_threshold=1.0,
            amplification_threshold=0.0,
            sample_rate=RATE,
            log_level=logging.WARNING,
        )
        self.sep.load_model(model_filename=model)
        log.info("loaded %s", model)

    def run(self, mix: np.ndarray) -> np.ndarray:
        for f in self.tmp.iterdir():
            f.unlink()
        src = self.tmp / "mix.wav"
        sf.write(src, mix * self.HEADROOM, RATE, subtype="FLOAT")
        outputs = [self.tmp / Path(p).name for p in self.sep.separate(str(src))]
        by_stem = {p.name.lower(): p for p in outputs}
        inst_path = next((p for n, p in by_stem.items() if "instrumental" in n or "(other)" in n), None)
        voc_path = next((p for n, p in by_stem.items() if "vocals" in n), None)
        if inst_path is not None:
            inst, sr = sf.read(inst_path, dtype="float32", always_2d=True)
        elif voc_path is not None:
            voc, sr = sf.read(voc_path, dtype="float32", always_2d=True)
            inst = mix * self.HEADROOM - fit(voc, len(mix))
        else:
            raise RuntimeError(f"no instrumental among {[p.name for p in outputs]}")
        if sr != RATE:
            raise RuntimeError(f"separator wrote {sr} Hz")
        inst = fit(inst, len(mix)) / self.HEADROOM
        if voc_path is not None and inst_path is not None:
            voc, _ = sf.read(voc_path, dtype="float32", always_2d=True)
            residual = mix - inst - fit(voc, len(mix)) / self.HEADROOM
            ratio = rms(residual) / max(rms(mix), 1e-9)
            level = 20 * np.log10(max(ratio, 1e-9))
            # the two stems should add back up to the mix
            (log.warning if level > -30 else log.debug)("stems sum to the mix within %.1f dB", level)
        return inst


class DummySeparator:
    model = "dummy-half-mix"

    def run(self, mix: np.ndarray) -> np.ndarray:
        return mix * 0.5


class Analyzer:
    """Essentia: tempo and key from DSP, the rest from Discogs-EffNet
    embeddings with classification heads (see setup.sh for the files)."""

    HEADS = {
        "danceability": "danceability-discogs-effnet-1",
        "happy": "mood_happy-discogs-effnet-1",
        "sad": "mood_sad-discogs-effnet-1",
        "party": "mood_party-discogs-effnet-1",
        "relaxed": "mood_relaxed-discogs-effnet-1",
        "aggressive": "mood_aggressive-discogs-effnet-1",
        "acoustic": "mood_acoustic-discogs-effnet-1",
        "electronic": "mood_electronic-discogs-effnet-1",
        "voice": "voice_instrumental-discogs-effnet-1",
    }

    def __init__(self, models: Path):
        import essentia
        import essentia.standard as es

        essentia.log.warningActive = False
        self.es = es
        self.rhythm = es.RhythmExtractor2013(method="multifeature")
        self.key = es.KeyExtractor(profileType="edma", sampleRate=RATE)
        self.resample = es.Resample(inputSampleRate=RATE, outputSampleRate=16_000, quality=4)
        self.embed = None
        self.heads = {}
        try:
            self._load_models(models)
            self.version = f"essentia {essentia.__version__} + discogs-effnet"
        except Exception as e:  # noqa: BLE001 - tempo and key still work
            log.warning("genre/mood models unavailable (%s); analysing tempo and key only", e)
            self.version = f"essentia {essentia.__version__} (tempo, key)"
        log.info("analysis ready: %s, %d heads", self.version, len(self.heads))

    def _load_models(self, models: Path) -> None:
        es = self.es
        if not hasattr(es, "TensorflowPredictEffnetDiscogs"):
            raise RuntimeError("this Essentia build has no TensorFlow (install essentia-tensorflow)")
        self.genre = es.TensorflowPredict2D(
            graphFilename=str(models / "genre_discogs400-discogs-effnet-1.pb"),
            input="serving_default_model_Placeholder",
            output="PartitionedCall:0",
        )
        self.genre_labels = json.loads((models / "genre_discogs400-discogs-effnet-1.json").read_text())["classes"]
        for name, file in self.HEADS.items():
            pb = models / f"{file}.pb"
            if not pb.is_file():
                log.warning("analysis head %s missing; skipped", file)
                continue
            classes = json.loads((models / f"{file}.json").read_text())["classes"]
            positive = next(
                i for i, c in enumerate(classes) if not c.startswith(("non_", "not_")) and c != "instrumental"
            )
            self.heads[name] = (es.TensorflowPredict2D(graphFilename=str(pb), output="model/Softmax"), positive)
        self.embed = es.TensorflowPredictEffnetDiscogs(
            graphFilename=str(models / "discogs-effnet-bs64-1.pb"), output="PartitionedCall:1"
        )

    def run(self, mix: np.ndarray) -> dict:
        mono = np.ascontiguousarray(mix.mean(axis=1), dtype=np.float32)
        bpm, _beats, confidence, _, _ = self.rhythm(mono)
        key, scale, strength = self.key(mono)
        out: dict = {
            "bpm": round(float(bpm), 1),
            "bpm_confidence": round(float(confidence), 2),
            "key": f"{key} {scale}",
            "key_strength": round(float(strength), 2),
            "analyzer": self.version,
        }
        if self.embed is None:
            return out
        emb = self.embed(self.resample(mono))
        g = np.asarray(self.genre(emb)).mean(axis=0)
        top = np.argsort(g)[::-1][:5]
        out["genres"] = [
            {"name": self.genre_labels[i].replace("---", " / "), "score": round(float(g[i]), 3)} for i in top
        ]
        moods = {}
        for name, (model, positive) in self.heads.items():
            p = float(np.asarray(model(emb)).mean(axis=0)[positive])
            if name == "danceability":
                out["danceability"] = round(p, 3)
            elif name == "voice":
                out["vocals"] = round(p, 3)
            else:
                moods[name] = round(p, 3)
        if moods:
            out["moods"] = moods
        return out


def fit(x: np.ndarray, frames: int) -> np.ndarray:
    """Pad with silence or trim to exactly `frames`."""
    if len(x) >= frames:
        return x[:frames]
    return np.concatenate([x, np.zeros((frames - len(x), x.shape[1]), dtype=x.dtype)])


def rms(x: np.ndarray) -> float:
    return float(np.sqrt(np.mean(np.square(x, dtype=np.float64))))


def write_json(path: Path, value: dict) -> None:
    tmp = path.with_suffix(".json.part")
    tmp.write_text(json.dumps(value, indent=2))
    os.replace(tmp, path)


def process(job_path: Path, spool: Path, sep, analyzer, analyzer_error: str | None) -> None:
    key = job_path.stem
    job = json.loads(job_path.read_text())
    started = time.monotonic()
    result: dict = {"model": sep.model}
    try:
        mix, sr = sf.read(spool / "in" / job["wav"], dtype="float32", always_2d=True)
        if sr != RATE or mix.shape[1] != 2:
            raise ValueError(f"expected stereo {RATE} Hz, got {mix.shape[1]}ch {sr} Hz")
        if len(mix) != job["frames"]:
            raise ValueError(f"wav has {len(mix)} frames, job says {job['frames']}")
        seconds = len(mix) / RATE
        if job.get("want_instrumental"):
            inst = sep.run(mix)
            assert len(inst) == len(mix)
            out = spool / "out" / f"{key}.flac"
            tmp = out.with_suffix(".flac.part")
            # PCM_16 like stems' own WAVs; clip first (libsndfile wraps)
            sf.write(tmp, np.clip(inst, -1.0, 32767 / 32768), RATE, format="FLAC", subtype="PCM_16")
            os.replace(tmp, out)
            result["instrumental"] = out.name
        if job.get("want_analysis"):
            if analyzer is None:
                result["analysis_error"] = analyzer_error or "analysis is not available"
            else:
                try:
                    result["analysis"] = analyzer.run(mix)
                except Exception as e:  # noqa: BLE001 - reported per track
                    log.exception("analysis failed for %s", key)
                    result["analysis_error"] = f"{type(e).__name__}: {e}"
        took = time.monotonic() - started
        log.info(
            "%s — %s: %.0f s of audio in %.1f s (%.1fx)",
            ", ".join(job.get("artists") or []),
            job.get("title", key),
            seconds,
            took,
            seconds / max(took, 1e-3),
        )
    except Exception as e:  # noqa: BLE001 - reported to stemsd
        log.error("job %s failed: %s", key, traceback.format_exc())
        result = {"error": f"{type(e).__name__}: {e}"}
    write_json(spool / "out" / f"{key}.json", result)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--spool", type=Path, required=True)
    ap.add_argument("--models", type=Path, required=True, help="model folder (separator + essentia)")
    ap.add_argument("--model", default=os.environ.get("STEMS_FARM_MODEL", DEFAULT_MODEL))
    ap.add_argument("--dummy", action="store_true", help="no models: test the plumbing")
    ap.add_argument("--once", action="store_true", help="exit when the spool is empty")
    args = ap.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s", stream=sys.stdout)

    for sub in ("in", "work", "out"):
        (args.spool / sub).mkdir(parents=True, exist_ok=True)
    # jobs claimed by a previous run that died: queue them again
    for stale in (args.spool / "work").glob("*.json"):
        os.replace(stale, args.spool / "in" / stale.name)
        log.info("requeued %s", stale.stem)

    if args.dummy:
        sep, analyzer, analyzer_error = DummySeparator(), None, "dummy mode"
    else:
        sep = RoformerSeparator(args.model, args.models)
        try:
            analyzer, analyzer_error = Analyzer(args.models / "essentia"), None
        except Exception as e:  # noqa: BLE001 - separation still works
            log.exception("analysis unavailable")
            analyzer, analyzer_error = None, f"analysis unavailable: {e}"

    stop = False

    def on_signal(*_):
        nonlocal stop
        stop = True
        log.info("stopping after the current job")

    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)
    log.info("watching %s", args.spool)
    while not stop:
        jobs = sorted((args.spool / "in").glob("*.json"), key=lambda p: p.stat().st_mtime)
        if not jobs:
            if args.once:
                break
            time.sleep(0.5)
            continue
        claimed = args.spool / "work" / jobs[0].name
        try:
            os.replace(jobs[0], claimed)
        except FileNotFoundError:
            continue  # stemsd gave up on it
        process(claimed, args.spool, sep, analyzer, analyzer_error)


if __name__ == "__main__":
    main()
