"""Expanding-month, entry-causal one-versus-two-lot sizing diagnostic."""
from __future__ import annotations

import argparse
import calendar
import datetime as dt
import json
import math
import time
from pathlib import Path

import numpy as np
import polars as pl

WARMUP_END = dt.date(2024, 6, 30)
RIDGE_ALPHA = 10.0
UPSCALE_QUANTILE = 0.67
FEATURE_SETS = {
    "spot": ["spot_return_30m_pct", "momentum_fast_slow_logpct"],
    "spot_iv": [
        "spot_return_30m_pct",
        "momentum_fast_slow_logpct",
        "iv_pp",
        "iv_change_5m_pp",
        "observed_risk_reversal_moneyness_pp",
    ],
    "spot_iv_crowding": [
        "spot_return_30m_pct",
        "momentum_fast_slow_logpct",
        "iv_pp",
        "iv_change_5m_pp",
        "observed_risk_reversal_moneyness_pp",
        "source_unit_count",
        "source_expiry_count",
        "same_minute_family_count",
        "same_contract_family_count",
    ],
    "spot_iv_crowding_exposure": [
        "spot_return_30m_pct",
        "momentum_fast_slow_logpct",
        "iv_pp",
        "iv_change_5m_pp",
        "observed_risk_reversal_moneyness_pp",
        "source_unit_count",
        "source_expiry_count",
        "same_minute_family_count",
        "same_contract_family_count",
        "basket_event_delta",
        "basket_event_gamma",
        "basket_event_vega",
        "basket_event_theta",
        "portfolio_delta_before",
        "portfolio_vega_before",
        "moneyness",
        "calendar_dte",
    ],
    "full_with_price_confidence": [
        "spot_return_30m_pct",
        "momentum_fast_slow_logpct",
        "iv_pp",
        "iv_change_5m_pp",
        "observed_risk_reversal_moneyness_pp",
        "source_unit_count",
        "source_expiry_count",
        "same_minute_family_count",
        "same_contract_family_count",
        "basket_event_delta",
        "basket_event_gamma",
        "basket_event_vega",
        "basket_event_theta",
        "portfolio_delta_before",
        "portfolio_vega_before",
        "moneyness",
        "calendar_dte",
        "basket_event_observed_positive_volume",
    ],
}


def month_start(value: str) -> dt.date:
    date = dt.date.fromisoformat(value)
    return date.replace(day=1)


def matrix(
    rows: list[dict], features: list[str], fit: dict | None = None
) -> tuple[np.ndarray, dict]:
    raw = np.array(
        [
            [
                float(row[name])
                if row.get(name) is not None and math.isfinite(float(row[name]))
                else np.nan
                for name in features
            ]
            for row in rows
        ],
        dtype=float,
    )
    if fit is None:
        medians = np.nanmedian(raw, axis=0)
        medians = np.where(np.isfinite(medians), medians, 0.0)
        filled = np.where(np.isnan(raw), medians, raw)
        low = np.quantile(filled, 0.01, axis=0)
        high = np.quantile(filled, 0.99, axis=0)
        clipped = np.clip(filled, low, high)
        means = clipped.mean(axis=0)
        scales = clipped.std(axis=0)
        scales = np.where(scales > 1e-12, scales, 1.0)
        fit = {"medians": medians, "low": low, "high": high, "means": means, "scales": scales}
    missing = np.isnan(raw).astype(float)
    filled = np.where(np.isnan(raw), fit["medians"], raw)
    clipped = np.clip(filled, fit["low"], fit["high"])
    standardized = (clipped - fit["means"]) / fit["scales"]
    return np.column_stack([standardized, missing]), fit


def train(rows: list[dict], features: list[str]) -> dict:
    x, transform = matrix(rows, features)
    y = np.array([float(row["net_micro"]) / 1e6 for row in rows])
    lo, hi = np.quantile(y, [0.025, 0.975])
    y = np.clip(y, lo, hi)
    design = np.column_stack([np.ones(len(x)), x])
    penalty = np.eye(design.shape[1]) * RIDGE_ALPHA
    penalty[0, 0] = 0.0
    beta = np.linalg.solve(design.T @ design + penalty, design.T @ y)
    fitted = design @ beta
    return {
        "transform": transform,
        "beta": beta,
        "threshold": float(np.quantile(fitted, UPSCALE_QUANTILE)),
        "train_n": len(rows),
    }


def score(row: dict, features: list[str], model: dict) -> float:
    x, _ = matrix([row], features, model["transform"])
    return float(np.r_[1.0, x[0]] @ model["beta"])


def predict_month(train_rows: list[dict], test_rows: list[dict], features: list[str]) -> list[dict]:
    models = {}
    for book in sorted({row["book"] for row in test_rows}):
        history = [row for row in train_rows if row["book"] == book]
        if len(history) < 100:
            raise ValueError(f"insufficient prior trades for {book}: {len(history)}")
        models[book] = train(history, features)
    result = []
    active: list[dict] = []
    grouped: dict[tuple[str, int], list[dict]] = {}
    for row in test_rows:
        grouped.setdefault((row["date"], int(row["entry_minute"])), []).append(dict(row))
    for (date, minute), group in sorted(grouped.items()):
        active = [
            position
            for position in active
            if position["date"] == date and position["exit"] > minute
        ]
        delta_before = sum(position["delta"] * position["multiplier"] for position in active)
        vega_before = sum(position["vega"] * position["multiplier"] for position in active)
        decided = []
        for row in group:
            row["portfolio_delta_before"] = delta_before
            row["portfolio_vega_before"] = vega_before
            model = models[row["book"]]
            predicted = score(row, features, model)
            multiplier = 2 if predicted > model["threshold"] else 1
            row.update(
                predicted_net_rupees=predicted,
                multiplier=multiplier,
                train_n=model["train_n"],
                fold_month=date[:7],
            )
            decided.append(row)
        result.extend(decided)
        active.extend(
            {
                "date": row["date"],
                "exit": int(row["actual_exit_minute"]),
                "delta": float(row.get("basket_event_delta") or 0.0),
                "vega": float(row.get("basket_event_vega") or 0.0),
                "multiplier": row["multiplier"],
            }
            for row in decided
        )
    return result


def drawdown(values: list[float]) -> float:
    total = peak = worst = 0.0
    for value in values:
        total += value
        peak = max(peak, total)
        worst = max(worst, peak - total)
    return worst


def metrics(values: list[float]) -> dict:
    array = np.asarray(values, dtype=float)
    deviation = float(array.std(ddof=1)) if len(array) > 1 else 0.0
    return {
        "days": len(values),
        "net_rupees": float(array.sum()),
        "daily_sharpe": float(array.mean() / deviation * math.sqrt(252)) if deviation else None,
        "drawdown_rupees": drawdown(values),
        "worst_day_rupees": float(array.min()) if len(array) else None,
        "positive_days": int((array > 0).sum()),
        "negative_days": int((array < 0).sum()),
    }


def reduce_policy(rows: list[dict], dates: list[str], multiplier_field: str | None) -> dict:
    daily = {date: 0.0 for date in dates}
    for row in rows:
        multiplier = int(row[multiplier_field]) if multiplier_field else 1
        daily[row["date"]] += float(row["net_micro"]) / 1e6 * multiplier
    overall = metrics([daily[date] for date in dates])
    quarters = {}
    for date in dates:
        parsed = dt.date.fromisoformat(date)
        key = f"{parsed.year}Q{(parsed.month - 1) // 3 + 1}"
        quarters.setdefault(key, []).append(daily[date])
    overall["quarterly"] = {key: metrics(values) for key, values in quarters.items()}
    overall["average_multiplier"] = (
        sum(int(row[multiplier_field]) for row in rows) / len(rows) if multiplier_field else 1.0
    )
    overall["scaled_trade_count"] = (
        sum(int(row[multiplier_field]) == 2 for row in rows) if multiplier_field else 0
    )
    return overall


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    start = time.perf_counter()
    args.output.mkdir(parents=True, exist_ok=False)
    tape = pl.read_parquet(args.input / "accepted_features.parquet").sort(
        "date", "entry_minute", "book", "contract_id"
    )
    rows = tape.to_dicts()
    oos_rows = [row for row in rows if dt.date.fromisoformat(row["date"]) > WARMUP_END]
    if not oos_rows:
        raise ValueError("empty OOS population")
    predictions = {}
    for name, features in FEATURE_SETS.items():
        predicted = []
        months = sorted({month_start(row["date"]) for row in oos_rows})
        for month in months:
            last_day = calendar.monthrange(month.year, month.month)[1]
            end = month.replace(day=last_day)
            train_rows = [row for row in rows if dt.date.fromisoformat(row["date"]) < month]
            test_rows = [
                row
                for row in oos_rows
                if month <= dt.date.fromisoformat(row["date"]) <= end
            ]
            if test_rows:
                predicted.extend(predict_month(train_rows, test_rows, features))
        predictions[name] = predicted
    dates = sorted(
        {
            row["date"]
            for row in oos_rows
            if dt.date.fromisoformat(row["date"]) > WARMUP_END
        }
    )
    results = {
        "definition": {
            "warmup": "2024-01-02..2024-06-30",
            "oos_start": "2024-07-01",
            "fold": "expanding monthly; labels strictly before test month",
            "lots": "1 or 2; no skipping; same accepted sequential tape",
            "fee_sensitivity": "base engine net multiplied by lots; doubles fees conservatively",
            "ridge_alpha": RIDGE_ALPHA,
            "upscale_quantile": UPSCALE_QUANTILE,
        },
        "fixed_one_lot": reduce_policy(oos_rows, dates, None),
        "feature_sets": {},
    }
    prediction_frames = []
    for name, predicted in predictions.items():
        results["feature_sets"][name] = reduce_policy(predicted, dates, "multiplier")
        prediction_frames.append(pl.DataFrame(predicted).with_columns(pl.lit(name).alias("feature_set")))
    pl.concat(prediction_frames, how="diagonal_relaxed").write_parquet(
        args.output / "predictions.parquet", compression="zstd"
    )
    results["elapsed_seconds"] = time.perf_counter() - start
    (args.output / "results.json").write_text(json.dumps(results, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
