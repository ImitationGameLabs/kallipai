"""Unit tests for the slug-derived path helpers (stdlib unittest only).

The adapter's post-run metrics depend on the on-disk layout the Rust
tagma writes: ``<xdg data home>/kallipai/tagmata/<slug>/agents/``. It
also delivers the LLM profile to the tagma's config home, whose shape
is ``<xdg config home>/kallipai/tagmata/<slug>/profiles.toml``. These
tests pin both shapes so a resolver change on either side breaks loudly.
"""

import tempfile
import unittest
from pathlib import Path

from kallipai_harbor.tagma import CONFIG_HOME, LOGS_DIR, agents_dir, profiles_path


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


class ProfilesPathTest(unittest.TestCase):
    def test_mirrors_the_rust_config_dir_root(self):
        # kallipai_adk::persistence::config_dir_root resolves
        # <platform config dir>/kallipai/tagmata/<KALLIPAI_TAGMA_SLUG>;
        # the tagma reads profiles.toml directly under that root.
        self.assertEqual(
            profiles_path("/logs/agent/config", "bench-abc12345"),
            Path("/logs/agent/config/kallipai/tagmata/bench-abc12345/profiles.toml"),
        )

    def test_config_home_sits_beside_the_data_root(self):
        # Config root and data root are siblings, never nested: LOGS_DIR
        # carries <logs>/kallipai/tagmata/<slug>/agents, while the delivered
        # profiles.toml comes from profiles_path(CONFIG_HOME, <slug>). A
        # config root inside the data root would let the sandbox hide-hole
        # swallow the delivered profile along with the agent tree.
        delivered = profiles_path(CONFIG_HOME, "bench-abc12345")
        self.assertFalse(delivered.is_relative_to(LOGS_DIR / "kallipai"))


if __name__ == "__main__":
    unittest.main()
