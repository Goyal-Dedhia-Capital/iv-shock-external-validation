"""Export and reconcile the fitted EXP021 monthly ridge models.

The governed research repository retains this deterministic exporter, while
the fitted bundle remains an external immutable artifact.  The exporter
re-fits every promoted family/month fold and proves that its scores and 1x/2x
decisions exactly reproduce the prediction tape consumed by the funded replay.
"""
from __future__ import annotations

import argparse
import calendar
import datetime as dt
import hashlib
import json
from pathlib import Path

import numpy as np
import polars as pl
import run_walkforward as walk

FEATURE_SET = "spot_iv_crowding"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def encode_model(month: dt.date, book: str, model: dict, features: list[str]) -> dict:
    transform = model["transform"]
    return {
        "fold_month": month.isoformat()[:7],
        "book": book,
        "features": features,
        "ridge_alpha": walk.RIDGE_ALPHA,
        "upscale_quantile": walk.UPSCALE_QUANTILE,
        "train_n": int(model["train_n"]),
        "threshold": float(model["threshold"]),
        "beta": np.asarray(model["beta"], dtype=float).tolist(),
        "transform": {
            name: np.asarray(transform[name], dtype=float).tolist()
            for name in ("medians", "low", "high", "means", "scales")
        },
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", required=True, type=Path)
    parser.add_argument("--predictions", required=True, type=Path)
    parser.add_argument("--multipliers", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)

    features = walk.FEATURE_SETS[FEATURE_SET]
    rows = (
        pl.read_parquet(args.input / "accepted_features.parquet")
        .sort("date", "entry_minute", "book", "contract_id")
        .to_dicts()
    )
    expected = (
        pl.read_parquet(args.predictions)
        .filter(pl.col("feature_set") == FEATURE_SET)
        .sort("date", "entry_minute", "book", "contract_id")
        .to_dicts()
    )
    expected_by_id = {row["strategy_position_id"]: row for row in expected}
    if len(expected_by_id) != len(expected):
        raise ValueError("prediction tape has duplicate strategy_position_id values")

    models = []
    checked = 0
    months = sorted(
        {
            walk.month_start(row["date"])
            for row in rows
            if dt.date.fromisoformat(row["date"]) > walk.WARMUP_END
        }
    )
    for month in months:
        last_day = calendar.monthrange(month.year, month.month)[1]
        end = month.replace(day=last_day)
        train_rows = [row for row in rows if dt.date.fromisoformat(row["date"]) < month]
        test_rows = [
            row
            for row in rows
            if month <= dt.date.fromisoformat(row["date"]) <= end
        ]
        for book in sorted({row["book"] for row in test_rows}):
            history = [row for row in train_rows if row["book"] == book]
            model = walk.train(history, features)
            models.append(encode_model(month, book, model, features))
            for row in (value for value in test_rows if value["book"] == book):
                expected_row = expected_by_id[row["strategy_position_id"]]
                predicted = walk.score(row, features, model)
                multiplier = 2 if predicted > model["threshold"] else 1
                if not np.isclose(
                    predicted,
                    float(expected_row["predicted_net_rupees"]),
                    rtol=0.0,
                    atol=1e-10,
                ):
                    raise ValueError(f"score drift for {row['strategy_position_id']}")
                if multiplier != int(expected_row["multiplier"]):
                    raise ValueError(f"multiplier drift for {row['strategy_position_id']}")
                checked += 1

    sparse_multipliers = {
        row["strategy_position_id"]: 2
        for row in expected
        if int(row["multiplier"]) == 2
    }
    if sparse_multipliers != json.loads(args.multipliers.read_text()):
        raise ValueError("exported decisions do not reproduce funded multiplier tape")

    bundle = {
        "schema": "gdc.exp021.walkforward-ridge-bundle.v1",
        "feature_set": FEATURE_SET,
        "warmup_end": walk.WARMUP_END.isoformat(),
        "models": models,
    }
    bundle_path = args.output / "model-bundle.json"
    bundle_path.write_text(json.dumps(bundle, indent=2, sort_keys=True) + "\n")
    manifest = {
        "schema": "gdc.exp021.walkforward-ridge-export.v1",
        "feature_set": FEATURE_SET,
        "model_count": len(models),
        "prediction_rows_checked": checked,
        "model_bundle_sha256": sha256(bundle_path),
        "accepted_features_sha256": sha256(args.input / "accepted_features.parquet"),
        "predictions_sha256": sha256(args.predictions),
        "funded_multiplier_tape_sha256": sha256(args.multipliers),
        "exporter_sha256": sha256(Path(__file__)),
        "walkforward_source_sha256": sha256(Path(walk.__file__)),
        "parity": {
            "scores": True,
            "multipliers": True,
            "funded_tape": True,
        },
    }
    (args.output / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    main()
