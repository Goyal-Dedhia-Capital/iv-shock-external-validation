"""Compact per-family distribution report from funded-runner lifecycle evidence."""

from __future__ import annotations

import json
import math
import statistics
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any


def _quantile(values: list[int], probability: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    location = (len(ordered) - 1) * probability
    left = math.floor(location)
    right = math.ceil(location)
    if left == right:
        return float(ordered[left])
    return ordered[left] + (ordered[right] - ordered[left]) * (location - left)


def _profit_factor(values: list[int]) -> float | None:
    gains = sum(value for value in values if value > 0)
    losses = -sum(value for value in values if value < 0)
    if losses == 0:
        return None
    return gains / losses


def _max_drawdown(values: list[int]) -> int:
    equity = peak = 0
    drawdown = 0
    for value in values:
        equity += value
        peak = max(peak, equity)
        drawdown = max(drawdown, peak - equity)
    return drawdown


def _date(action: dict[str, Any]) -> str:
    source = action.get("lineage", {}).get("source_event_id", action.get("decision_id", ""))
    fields = source.split("|")
    return fields[1] if len(fields) > 2 and fields[0] == "iv-shock-source" else "unknown"


def _stats(values: list[int]) -> dict[str, Any]:
    return {
        "n": len(values),
        "mean_micro": statistics.fmean(values) if values else None,
        "median_micro": statistics.median(values) if values else None,
        "p05_micro": _quantile(values, 0.05),
        "p25_micro": _quantile(values, 0.25),
        "p75_micro": _quantile(values, 0.75),
        "p95_micro": _quantile(values, 0.95),
        "win_rate": sum(value > 0 for value in values) / len(values) if values else None,
        "zero_rate": sum(value == 0 for value in values) / len(values) if values else None,
        "profit_factor": _profit_factor(values),
        "max_drawdown_micro": _max_drawdown(values),
    }


def _period_totals(trades: list[dict[str, Any]], width: int) -> dict[str, int]:
    totals = defaultdict(int)
    for trade in trades:
        date = trade["date"]
        key = date[:width] if date != "unknown" else date
        totals[key] += trade["net_micro"]
    return dict(sorted(totals.items()))


def summarize_steps(steps_path: Path, summary_path: Path) -> dict[str, Any]:
    positions: dict[str, dict[str, Any]] = {}
    counts: dict[str, Counter[str]] = defaultdict(Counter)
    rejects: dict[str, Counter[str]] = defaultdict(Counter)
    completed: dict[str, list[dict[str, Any]]] = defaultdict(list)
    with steps_path.open() as stream:
        for line_number, line in enumerate(stream, start=1):
            if not line.strip():
                continue
            record = json.loads(line)
            actions = {action["intent_id"]: action for action in record["response"]["actions"]}
            for outcome in record["lifecycle"]["outcomes"]:
                action = actions.get(outcome["intent_id"])
                if action is None:
                    raise ValueError(f"line {line_number}: outcome has no matching action")
                book = action.get("lineage", {}).get("book", "unknown")
                intent_action = action["action"].lower()
                status = outcome["status"].lower()
                counts[book][f"{intent_action}_{status}"] += 1
                if status != "filled":
                    rejects[book][outcome.get("reason") or status] += 1
                    continue
                position_id = outcome["strategy_position_id"]
                position = positions.setdefault(
                    position_id,
                    {"book": book, "date": _date(action), "gross_micro": 0, "fees_micro": 0},
                )
                for fill in outcome["fills"]:
                    signed_cash = fill["price"] * fill["quantity"]
                    if fill["side"] == "BUY":
                        signed_cash = -signed_cash
                    position["gross_micro"] += signed_cash
                    position["fees_micro"] += fill["fee"]
                if intent_action in {"close", "flatten"}:
                    position["net_micro"] = position["gross_micro"] - position["fees_micro"]
                    completed[book].append(position)
                    del positions[position_id]

    engine_summary = json.loads(summary_path.read_text())
    books = sorted(set(counts) | set(completed) | {p["book"] for p in positions.values()})
    report: dict[str, Any] = {
        "schema_version": "gdc.iv-shock.external-funded-report.v1",
        "run_summary": engine_summary,
        "families": {},
    }
    all_completed: list[dict[str, Any]] = []
    for book in books:
        trades = completed[book]
        all_completed.extend(trades)
        gross = [trade["gross_micro"] for trade in trades]
        net = [trade["net_micro"] for trade in trades]
        daily = defaultdict(int)
        for trade in trades:
            daily[trade["date"]] += trade["net_micro"]
        positive_total = sum(max(value, 0) for value in daily.values())
        report["families"][book] = {
            "lifecycle_counts": dict(counts[book]),
            "rejection_reasons": dict(rejects[book]),
            "completed_trades": len(trades),
            "unresolved_positions": sum(p["book"] == book for p in positions.values()),
            "gross": _stats(gross),
            "fees_total_micro": sum(trade["fees_micro"] for trade in trades),
            "net": _stats(net),
            "largest_positive_date_share": (
                max((max(value, 0) for value in daily.values()), default=0) / positive_total
                if positive_total
                else None
            ),
            "daily_net_micro": dict(sorted(daily.items())),
            "monthly_net_micro": _period_totals(trades, 7),
            "yearly_net_micro": _period_totals(trades, 4),
        }
    all_net = [trade["net_micro"] for trade in all_completed]
    report["combined"] = {
        "completed_trades": len(all_completed),
        "unresolved_positions": len(positions),
        "net": _stats(all_net),
        "gross": _stats([trade["gross_micro"] for trade in all_completed]),
        "fees_total_micro": sum(trade["fees_micro"] for trade in all_completed),
        "monthly_net_micro": _period_totals(all_completed, 7),
        "yearly_net_micro": _period_totals(all_completed, 4),
    }
    return report


def write_report(steps_path: Path, summary_path: Path, output_path: Path) -> dict[str, Any]:
    if output_path.exists():
        raise FileExistsError(f"refusing to overwrite report: {output_path}")
    result = summarize_steps(steps_path, summary_path)
    temporary = output_path.with_name(f".{output_path.name}.tmp")
    temporary.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    temporary.replace(output_path)
    return result
