"""Pin the operator-facing LLM profile template and the guard over it.

The adapter ships `localprofile.example.toml` for operators to copy beside
it as `.localprofile.toml`, and the tagma reads that copy as its own
`profiles.toml`. This test parses the template with the stdlib parser and
runs the delivered-file guard over it, so a field the model requires
cannot quietly go missing from either. It imports stdlib-only modules: the
template stays checkable without the Harbor virtualenv.
"""

import tempfile
import tomllib
import unittest
from pathlib import Path

from kallipai_harbor.profiles import (
    REQUIRED_PROFILE_FIELDS,
    resolve_local_profile,
)

TEMPLATE = Path(__file__).resolve().parents[1] / "localprofile.example.toml"


class LocalProfileExampleTest(unittest.TestCase):
    def test_parses_and_carries_every_required_profile_field(self):
        with TEMPLATE.open("rb") as handle:
            config = tomllib.load(handle)

        sets = config["sets"]
        # The root agent binds to the config's default set at boot.
        self.assertIn(config["default"], sets)
        profiles = [p for entry in sets.values() for p in entry["profiles"]]
        # Without this, a template holding no profiles would pass silently.
        self.assertTrue(profiles)
        for profile in profiles:
            for field in REQUIRED_PROFILE_FIELDS:
                self.assertIn(field, profile)

    def test_the_template_passes_the_delivered_file_guard(self):
        # The adapter runs this guard during install, before the tagma starts,
        # so a template that fails it would fail every run.
        self.assertEqual(resolve_local_profile(TEMPLATE), TEMPLATE)


class DeliveredShapeTest(unittest.TestCase):
    """The guard's refusals, one shape at a time."""

    def _refusal(self, body: str) -> str:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / ".localprofile.toml"
            path.write_text(body, encoding="utf-8")
            with self.assertRaises(ValueError) as caught:
                resolve_local_profile(path)
        return str(caught.exception)

    def test_a_missing_file_names_every_required_field(self):
        with tempfile.TemporaryDirectory() as tmp:
            missing = Path(tmp) / ".localprofile.toml"
            with self.assertRaises(FileNotFoundError) as caught:
                resolve_local_profile(missing)
        for field in REQUIRED_PROFILE_FIELDS:
            self.assertIn(field, str(caught.exception))

    def test_a_profile_without_a_required_field_is_refused(self):
        message = self._refusal(
            '[endpoints.ds]\nfamily = "deepseek"\napi_key = "k"\n'
            "[sets.bench]\n"
            '[[sets.bench.profiles]]\nid = "main"\nendpoint = "ds"\nmodel = "m"\n'
        )
        self.assertIn("max_context_window", message)

    def test_a_default_naming_no_set_is_refused(self):
        message = self._refusal(
            'default = "nope"\n'
            "[sets.bench]\n"
            '[[sets.bench.profiles]]\nid = "main"\nendpoint = "ds"\n'
            'model = "m"\nmax_context_window = 1000\n'
        )
        self.assertIn("nope", message)

    def test_a_file_without_sets_is_refused(self):
        self.assertIn("sets", self._refusal('[endpoints.ds]\nfamily = "deepseek"\n'))


if __name__ == "__main__":
    unittest.main()
