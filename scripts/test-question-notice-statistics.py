#!/usr/bin/env python3
"""Checks for the plan's observed p95 gate and measurement validity."""
import unittest

from question_notice_statistics import PHASE_ORDER, percentile95, summarize, verdict


class StatusStatisticsTests(unittest.TestCase):
    def phases(self, load_delta):
        return [{"phase": name, "both_ms": [100 + (load_delta if resolve else 0)] * 500}
                for name, resolve in PHASE_ORDER]

    def test_above_budget_fails(self):
        result = summarize(self.phases(51))
        self.assertEqual(result["pooled_delta_ms"], 51)
        self.assertEqual(result["samples_per_condition"], 2000)
        self.assertEqual(verdict(result, True), "fail")

    def test_boundary_and_incomparable_load(self):
        result = summarize(self.phases(50))
        self.assertEqual(verdict(result, True), "pass")
        self.assertEqual(verdict(result, False), "inconclusive")
        self.assertEqual(verdict(summarize(self.phases(-10)), True), "pass")

    def test_incomplete_or_reordered_phases_are_rejected(self):
        phases = self.phases(0)
        with self.assertRaises(ValueError):
            summarize(phases[:-1])
        phases[0], phases[1] = phases[1], phases[0]
        with self.assertRaises(ValueError):
            summarize(phases)
        phases = self.phases(0)
        phases[0]["both_ms"].pop()
        with self.assertRaises(ValueError):
            summarize(phases)

    def test_pooled_p95_does_not_average_away_a_slow_phase(self):
        phases = self.phases(0)
        next(phase for phase in phases if phase["phase"] == "load-3")["both_ms"] = [200] * 500
        result = summarize(phases)
        # Averaging the four phase deltas would yield only 25ms and hide this.
        self.assertEqual(result["pooled_delta_ms"], 100)
        self.assertEqual(verdict(result, True), "fail")

    def test_p95_uses_the_declared_rank(self):
        self.assertEqual(percentile95(list(reversed(range(100)))), 94)

    def test_invalid_observations_are_rejected(self):
        for invalid in [float("nan"), float("inf"), -float("inf"), -1]:
            phases = self.phases(0)
            phases[0]["both_ms"][0] = invalid
            with self.assertRaises(ValueError):
                summarize(phases)


if __name__ == "__main__":
    unittest.main()
