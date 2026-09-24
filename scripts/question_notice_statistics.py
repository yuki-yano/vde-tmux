"""The plan's observed status-delivery p95 gate; no IO or runtime payloads.

Keep two complete ABBA cycles with 500 events per phase. Pool all observations
per condition rather than averaging phase percentiles. Statistical confidence
intervals and calibration are outside this feature's acceptance requirements.
"""
import math

PHASE_ORDER = tuple((f"{condition}-{index}", condition == "load")
                    for cycle in range(2)
                    for condition, index in (("baseline", 2 * cycle), ("load", 2 * cycle),
                                             ("load", 2 * cycle + 1), ("baseline", 2 * cycle + 1)))
PHASE_EVENTS = 500


def percentile95(values):
    return sorted(values)[int((len(values) - 1) * .95)]


def summarize(phases):
    expected = [name for name, _ in PHASE_ORDER]
    if [phase["phase"] for phase in phases] != expected:
        raise ValueError("two complete ABBA cycles in the declared order are required")
    values = {phase["phase"]: phase["both_ms"] for phase in phases}
    if any(len(sample) != PHASE_EVENTS for sample in values.values()):
        raise ValueError("each phase requires exactly 500 events")
    if any(not math.isfinite(value) or value < 0 for sample in values.values() for value in sample):
        raise ValueError("status-delivery observations must be finite and nonnegative")
    pooled = {condition: [value for name in expected if name.startswith(condition)
                          for value in values[name]] for condition in ("baseline", "load")}
    baseline, loaded = (percentile95(pooled[condition]) for condition in ("baseline", "load"))
    return {"method": "two ABBA cycles; condition-wise observed p95 of event-wise max(API, rail)",
            "limitation": "observed run only; not a confidence bound on population p95",
            "phase_events": PHASE_EVENTS,
            "samples_per_condition": len(pooled["baseline"]),
            "baseline_p95_ms": baseline, "load_p95_ms": loaded,
            "pooled_delta_ms": loaded - baseline}


def verdict(result, comparable_load):
    if not comparable_load:
        return "inconclusive"
    return "pass" if result["pooled_delta_ms"] <= 50 else "fail"
