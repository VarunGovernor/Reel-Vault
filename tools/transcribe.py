#!/usr/bin/env python3
"""ReelVault local transcription helper (macOS: mlx-whisper).

Media always comes from a local file passed by the user or operator;
nothing here fetches content from Instagram. The whisper model is loaded
from the local Hugging Face cache or fetched by the runtime on first use;
an offline cache keeps this fully local.

Output: one JSON envelope on stdout (or --output file) with engine,
model, timestamped segments, and measured runtime. Nonzero exit on
failure with the error recorded in JSON.

Usage:
  tools/transcribe.py PATH [--model small] [--language en] [--output file]
"""
import argparse
import datetime
import json
import os
import sys
import time

ENGINE = "mlx-whisper"

# Unit tests replace this callable; None means "import the real engine lazily".
EXECUTOR = None


def default_model() -> str:
    return os.environ.get("REELVAULT_WHISPER_MODEL", "small")


def parse_args(argv=None):
    parser = argparse.ArgumentParser(
        prog="transcribe",
        description="Local transcription for ReelVault (local files only).",
    )
    parser.add_argument("path", help="local audio/video file to transcribe")
    parser.add_argument("--model", default=None,
                       help="whisper size or mlx-community repo id (default: %s)"
                            % default_model())
    parser.add_argument("--language", default=None,
                       help="language hint, e.g. en")
    parser.add_argument("--output", default=None,
                       help="write JSON here instead of stdout")
    args = parser.parse_args(argv)
    if not args.model:
        args.model = default_model()
    return args


def engine_version():
    try:
        from importlib.metadata import version  # Python 3.8+
        return version("mlx-whisper")
    except Exception:
        return None


def autosave_executor():
    """Resolve the mlx_whisper.transcribe callable lazily at call time."""
    global EXECUTOR
    if EXECUTOR is None:
        import mlx.whisper
        EXECUTOR = mlx.whisper.transcribe
    return EXECUTOR


def _usable_segments(segments):
    rows = []
    for seg in segments or []:
        start = seg.get("start", 0)
        end = seg.get("end", 0)
        text = (seg.get("text") or "").strip()
        if text:
            rows.append({"start": round(float(start), 3), "end": round(float(end), 3), "text": text})
    return rows


def transcribe_file(executor, path, model, language):
    kwargs = {"model": model}
    if language:
        kwargs["language"] = language
    started = time.time()
    result = executor(path, **kwargs)
    runtime = round(time.time() - started, 2)
    segments = _usable_segments(result.get("segments"))
    info = result.get("extra", {}) or {}
    return {
        "engine": ENGINE,
        "engine_version": engine_version(),
        "model": model,
        "source": path,
        "language": language or info.get("language"),
        "duration_s": round(float(info.get("duration") or 0), 2),
        "segment_count": len(segments),
        "segments": segments,
        "runtime_s": runtime,
        "created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        "errors": [],
    }


def write_report(report, output):
    payload = json.dumps(report, ensure_ascii=False, indent=2)
    if output:
        with open(output, "w", encoding="utf-8") as fh:
            fh.write(payload)
    else:
        sys.stdout.write(payload + "\n")


def fail(path, message, output=None):
    write_report({
        "engine": ENGINE,
        "engine_version": engine_version(),
        "source": path,
        "errors": [message],
        "created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    }, output)
    return 1


def main(argv=None):
    args = parse_args(argv)
    path = os.path.abspath(os.path.expanduser(args.path))
    if not os.path.isfile(path):
        return fail(path, "input file not found", args.output)
    try:
        executor = autosave_executor()
        report = transcribe_file(executor, path, args.model, args.language)
    except Exception as exc:  # surfaced verbatim, nothing about the result is claims.
        return fail(path, "{}: {}".format(type(exc).__name__, exc), args.output)
    write_report(report, args.output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
