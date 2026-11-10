"""Unit tests for the ReelVault transcription CLI contract (no mlx import)."""
import io
import json
import os
import sys
import unittest
import tempfile

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "tools"))

import transcribe  # noqa: E402


class FakeExecutor:
    def __init__(self):
        self.calls = []

    def __call__(self, path, **kwargs):
        self.calls.append((path, kwargs))
        return {
            "segments": [
                {"start": 0.0, "end": 1.5, "text": " Open the terminal. "},
                {"start": 1.6, "end": 3.0, "text": ""},
                {"start": 3.0, "end": 4.5, "text": " pip install requests"},
            ],
            "extra": {"language": "en", "duration": 4.5},
        }


class TranscribeCliTests(unittest.TestCase):
    def test_defaults(self):
        args = transcribe.parse_args(["clip.m4a"])
        self.assertEqual(args.model, "small")
        self.assertIsNone(args.language)
        self.assertIsNone(args.output)

    def test_env_model_override(self):
        old = os.environ.get("REELVAULT_WHISPER_MODEL")
        os.environ["REELVAULT_WHISPER_MODEL"] = "tiny"
        try:
            args = transcribe.parse_args(["clip.m4a"])
            self.assertEqual(args.model, "tiny")
        finally:
            if old is None:
                os.environ.pop("REELVAULT_WHISPER_MODEL", None)
            else:
                os.environ["REELVAULT_WHISPER_MODEL"] = old

    def test_report_schema_and_segment_filtering(self):
        fake = FakeExecutor()
        report = transcribe.transcribe_file(fake, "/tmp/clip.m4a", "small", "en")
        self.assertEqual(report["engine"], "mlx-whisper")
        self.assertEqual(report["model"], "small")
        self.assertEqual(report["language"], "en")
        self.assertEqual(report["duration_s"], 4.5)
        self.assertEqual(report["segment_count"], 2)
        self.assertEqual(len(report["segments"]), 2)
        self.assertEqual(report["segments"][0]["text"], "Open the terminal.")
        self.assertGreater(report["runtime_s"], -1)
        self.assertEqual(report["errors"], [])
        path, kwargs = fake.calls[0]
        self.assertEqual(path, "/tmp/clip.m4a")
        self.assertEqual(kwargs, {"model": "small", "language": "en"})

    def test_report_json_roundtrip(self):
        fake = FakeExecutor()
        report = transcribe.transcribe_file(fake, "/tmp/x.m4a", "base", None)
        buf = io.StringIO()
        stdout = sys.stdout
        sys.stdout = buf
        try:
            transcribe.write_report(report, None)
        finally:
            sys.stdout = stdout
        parsed = json.loads(buf.getvalue())
        self.assertEqual(parsed["segments"][1]["text"], "pip install requests")
        self.assertIn("created_at", parsed)

    def test_write_report_to_file(self):
        fake = FakeExecutor()
        report = transcribe.transcribe_file(fake, "/tmp/y.m4a", "small", "en")
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "r.json")
            transcribe.write_report(report, out)
            with open(out, encoding="utf-8") as fh:
                self.assertEqual(json.load(fh)["segment_count"], 2)

    def test_missing_input_fails_cleanly(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "err.json")
            code = transcribe.main(["/definitely/not/here.mp4", "--output", out])
            self.assertEqual(code, 1)
            with open(out, encoding="utf-8") as fh:
                data = json.load(fh)
            self.assertIn("input file not found", data["errors"][0])

    def test_engine_failure_is_reported(self):
        def boom(path, **kwargs):
            raise RuntimeError("model cache missing")

        old = transcribe.EXECUTOR
        transcribe.EXECUTOR = boom
        try:
            with tempfile.TemporaryDirectory() as tmp:
                out = os.path.join(tmp, "err.json")
                code = transcribe.main([__file__, "--output", out])
                self.assertEqual(code, 1)
                with open(out, encoding="utf-8") as fh:
                    data = json.load(fh)
                self.assertIn("RuntimeError", data["errors"][0])
        finally:
            transcribe.EXECUTOR = old


if __name__ == "__main__":
    unittest.main()
