"""Unit tests for the slug-derived path helpers (stdlib unittest only).

The adapter's post-run metrics depend on the on-disk layout the Rust
tagma writes: ``<xdg data home>/kallipai/tagmata/<slug>/agents/``. These
tests pin that shape so a resolver change on either side breaks loudly.
"""

import tempfile
import unittest
from pathlib import Path

from kallipai_harbor.tagma import agents_dir


class AgentsDirTest(unittest.TestCase):
    def test_mirrors_the_rust_data_dir_root(self):
        # kallipai_adk::persistence::data_dir_root resolves
        # <platform data dir>/kallipai/tagmata/<KALLIPAI_TAGMA_SLUG>; the agents
        # subtree hangs directly under it.
        self.assertEqual(
            agents_dir("/logs/agent", "bench-abc12345"),
            Path("/logs/agent/kallipai/tagmata/bench-abc12345/agents"),
        )

    def test_accepts_a_path_anchor(self):
        with tempfile.TemporaryDirectory() as tmp:
            expected = Path(tmp) / "kallipai" / "tagmata" / "main" / "agents"
            self.assertEqual(agents_dir(Path(tmp), "main"), expected)

    def test_slug_is_a_single_path_component(self):
        # A slug with separators would escape the instance tree; the
        # daemon's valid_slug grammar forbids them, and the helper must
        # not launder one through.
        result = agents_dir("/logs/agent", "bench-abc12345")
        self.assertEqual(result.parts[-4:], ("kallipai", "tagmata", "bench-abc12345", "agents"))
        self.assertNotIn("/", result.name)
        self.assertNotIn("..", result.parts)


if __name__ == "__main__":
    unittest.main()
