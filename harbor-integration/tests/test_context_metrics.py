"""Unit tests for the token totals read out of the agent manifests.

The suite is stdlib-only: it must run without the ``harbor`` dependency,
which is why the module under test imports nothing from Harbor either.
"""

import json
import tempfile
import unittest
from pathlib import Path

from kallipai_harbor.metrics import agent_usage_totals
from kallipai_harbor.tagma import agents_dir


def write_manifest(
    agents: Path, name: str, usage: dict, stage: str = "active"
) -> None:
    agent_dir = agents / stage / name
    agent_dir.mkdir(parents=True)
    (agent_dir / "manifest.json").write_text(
        json.dumps({"cumulative_usage": usage}), encoding="utf-8"
    )


def agent_tree(tmp: str) -> Path:
    # The adapter hands the totals the tree tagma.agents_dir() names; using
    # the same helper keeps the fixture depth honest.
    return agents_dir(tmp, "bench-abc12345")


class AgentUsageTotalsTest(unittest.TestCase):
    def test_sums_every_agent_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            agents = agent_tree(tmp)
            write_manifest(
                agents,
                "root",
                {
                    "prompt_tokens": 100,
                    "completion_tokens": 40,
                    "cache_read_tokens": 7,
                },
            )
            write_manifest(
                agents,
                "child-a",
                {
                    "prompt_tokens": 20,
                    "completion_tokens": 5,
                    "cache_read_tokens": 3,
                },
            )
            write_manifest(agents, "child-b", {"prompt_tokens": 1})
            self.assertEqual(agent_usage_totals(agents), (121, 45, 10))

    def test_skips_a_manifest_that_does_not_parse(self):
        with tempfile.TemporaryDirectory() as tmp:
            agents = agent_tree(tmp)
            write_manifest(agents, "root", {"prompt_tokens": 5, "completion_tokens": 2})
            broken = agents / "active" / "child-a"
            broken.mkdir()
            (broken / "manifest.json").write_text("{ truncated", encoding="utf-8")
            self.assertEqual(agent_usage_totals(agents), (5, 2, 0))

    def test_ignores_a_manifest_one_layer_shallow(self):
        with tempfile.TemporaryDirectory() as tmp:
            agents = agent_tree(tmp)
            write_manifest(agents, "root", {"prompt_tokens": 4})
            stray = agents / "parked"
            stray.mkdir()
            (stray / "manifest.json").write_text(
                json.dumps({"cumulative_usage": {"prompt_tokens": 99}}),
                encoding="utf-8",
            )
            self.assertEqual(agent_usage_totals(agents), (4, 0, 0))

    def test_counts_an_archived_agent(self):
        with tempfile.TemporaryDirectory() as tmp:
            agents = agent_tree(tmp)
            write_manifest(agents, "root", {"prompt_tokens": 5})
            write_manifest(
                agents,
                "retired",
                {"prompt_tokens": 30, "completion_tokens": 2},
                stage="archived",
            )
            self.assertEqual(agent_usage_totals(agents), (35, 2, 0))

    def test_missing_tree_totals_to_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(agent_usage_totals(Path(tmp) / "absent"), (0, 0, 0))


if __name__ == "__main__":
    unittest.main()
