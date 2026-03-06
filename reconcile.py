#!/usr/bin/env python3

from __future__ import annotations

import argparse
import csv
import json
import re
from collections import OrderedDict
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

import numpy as np
from scipy.optimize import linear_sum_assignment

DATE_FMT = "%m/%d/%y %H:%M"
TOKEN_RE = re.compile(r"[^a-z0-9]+")


@dataclass(frozen=True)
class BankTransaction:
    idx: int
    dt: datetime
    amount: float
    description: str
    token_set: frozenset[str]


@dataclass(frozen=True)
class JournalEntry:
    journal_entry_id: str
    dt: datetime
    amount: float
    description: str
    num_lines: int
    token_set: frozenset[str]


def parse_datetime(value: str) -> datetime:
    return datetime.strptime(value.strip(), DATE_FMT)


def normalize_tokens(text: str) -> frozenset[str]:
    cleaned = TOKEN_RE.sub(" ", text.lower()).strip()
    if not cleaned:
        return frozenset()
    return frozenset(cleaned.split())


def jaccard_similarity(a: frozenset[str], b: frozenset[str]) -> float:
    if not a and not b:
        return 0.0
    union = a | b
    if not union:
        return 0.0
    return len(a & b) / len(union)


def load_bank_transactions(path: Path) -> list[BankTransaction]:
    rows: list[BankTransaction] = []
    with path.open(newline="", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        for idx, row in enumerate(reader):
            description = (row.get("description") or "").strip()
            rows.append(
                BankTransaction(
                    idx=idx,
                    dt=parse_datetime(row["datetime"]),
                    amount=float(row["amount"]),
                    description=description,
                    token_set=normalize_tokens(description),
                )
            )
    return rows


def aggregate_gl_entries(path: Path) -> list[JournalEntry]:
    grouped: OrderedDict[str, dict] = OrderedDict()
    with path.open(newline="", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        for row in reader:
            je_id = row["journal_entry_id"]
            dt = parse_datetime(row["datetime"])
            amount = float(row["amount"])
            description = (row.get("description") or "").strip()

            if je_id not in grouped:
                grouped[je_id] = {
                    "dt": dt,
                    "amount": 0.0,
                    "descriptions": [],
                    "num_lines": 0,
                }

            grouped[je_id]["amount"] += amount
            grouped[je_id]["num_lines"] += 1
            if description:
                grouped[je_id]["descriptions"].append(description)

    entries: list[JournalEntry] = []
    for je_id, payload in grouped.items():
        description = " ".join(payload["descriptions"]).strip()
        entries.append(
            JournalEntry(
                journal_entry_id=je_id,
                dt=payload["dt"],
                amount=payload["amount"],
                description=description,
                num_lines=payload["num_lines"],
                token_set=normalize_tokens(description),
            )
        )
    return entries


def amounts_compatible(
    bank_amount: float, entry_amount: float, policy: str, tolerance: float
) -> bool:
    if policy == "exact":
        return abs(bank_amount - entry_amount) <= tolerance
    if policy == "opposite":
        return abs(bank_amount + entry_amount) <= tolerance
    if policy == "absolute":
        return abs(abs(bank_amount) - abs(entry_amount)) <= tolerance
    raise ValueError(f"Unsupported amount policy: {policy}")


def build_weight_matrix(
    gl_entries: list[JournalEntry],
    bank_transactions: list[BankTransaction],
    date_window_days: int,
    amount_policy: str,
    amount_tolerance: float,
) -> tuple[np.ndarray, np.ndarray, int]:
    n_gl = len(gl_entries)
    n_bank = len(bank_transactions)

    forbidden = -10**12
    score_scale = 1_000_000
    # Prioritize score first, then number of matches.
    base = n_gl + 1

    weights_real = np.full((n_gl, n_bank), forbidden, dtype=np.int64)

    for gl_idx, entry in enumerate(gl_entries):
        for bank_idx, bank_txn in enumerate(bank_transactions):
            date_delta_days = abs((bank_txn.dt - entry.dt).total_seconds()) / 86400.0
            if date_delta_days > date_window_days:
                continue
            if not amounts_compatible(
                bank_txn.amount, entry.amount, amount_policy, amount_tolerance
            ):
                continue

            score = jaccard_similarity(entry.token_set, bank_txn.token_set)
            score_scaled = int(round(score * score_scale))
            weight = score_scaled * base + 1
            weights_real[gl_idx, bank_idx] = weight

    dummy_cols = np.zeros((n_gl, n_gl), dtype=np.int64)
    full_weights = np.concatenate([weights_real, dummy_cols], axis=1)
    return full_weights, weights_real, base


def reconcile(
    bank_csv: Path,
    gl_csv: Path,
    out_json: Path,
    date_window_days: int,
    amount_policy: str,
    amount_tolerance: float,
) -> None:
    bank_transactions = load_bank_transactions(bank_csv)
    gl_entries = aggregate_gl_entries(gl_csv)

    full_weights, weights_real, base = build_weight_matrix(
        gl_entries=gl_entries,
        bank_transactions=bank_transactions,
        date_window_days=date_window_days,
        amount_policy=amount_policy,
        amount_tolerance=amount_tolerance,
    )

    row_ind, col_ind = linear_sum_assignment(full_weights, maximize=True)

    score_scale = 1_000_000
    n_bank = len(bank_transactions)
    matches: list[dict] = []

    for gl_idx, assigned_col in zip(row_ind.tolist(), col_ind.tolist()):
        if assigned_col >= n_bank:
            continue

        weight = int(weights_real[gl_idx, assigned_col])
        if weight <= 0:
            continue

        entry = gl_entries[gl_idx]
        bank_txn = bank_transactions[assigned_col]
        score = ((weight - 1) // base) / score_scale

        matches.append(
            {
                "journal_entry_id": entry.journal_entry_id,
                "entry_datetime": entry.dt.isoformat(),
                "entry_amount": round(entry.amount, 2),
                "entry_description": entry.description,
                "num_lines": entry.num_lines,
                "bank_transaction": {
                    "row_index": bank_txn.idx,
                    "datetime": bank_txn.dt.isoformat(),
                    "amount": round(bank_txn.amount, 2),
                    "description": bank_txn.description,
                },
                "score": round(score, 6),
            }
        )

    matches.sort(key=lambda r: (r["journal_entry_id"], r["bank_transaction"]["datetime"]))

    with out_json.open("w", encoding="utf-8") as f:
        json.dump(matches, f, indent=2)

    total_gl = len(gl_entries)
    total_bank = len(bank_transactions)
    total_matched = len(matches)

    gl_rate = (total_matched / total_gl * 100) if total_gl else 0.0
    bank_rate = (total_matched / total_bank * 100) if total_bank else 0.0

    print(f"Aggregated GL entries: {total_gl}")
    print(f"Bank transactions: {total_bank}")
    print(f"Matched: {total_matched}")
    print(f"GL match rate: {gl_rate:.2f}%")
    print(f"Bank match rate: {bank_rate:.2f}%")
    print(f"Amount policy: {amount_policy}")
    print(f"Date window: {date_window_days} days")
    print(f"Output: {out_json}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Reconcile bank transactions to aggregated GL entries with one-to-one matching."
    )
    parser.add_argument(
        "--bank-csv",
        type=Path,
        default=Path("bank_transactions.csv"),
        help="Path to bank transactions CSV.",
    )
    parser.add_argument(
        "--gl-csv",
        type=Path,
        default=Path("general_ledger.csv"),
        help="Path to general ledger CSV.",
    )
    parser.add_argument(
        "--out-json",
        type=Path,
        default=Path("matches.json"),
        help="Path to output JSON file.",
    )
    parser.add_argument(
        "--date-window-days",
        type=int,
        default=15,
        help="Maximum allowed absolute date difference in days.",
    )
    parser.add_argument(
        "--amount-policy",
        choices=["exact", "opposite", "absolute"],
        default="absolute",
        help="Amount matching policy.",
    )
    parser.add_argument(
        "--amount-tolerance",
        type=float,
        default=0.01,
        help="Decimal tolerance for amount comparison.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    reconcile(
        bank_csv=args.bank_csv,
        gl_csv=args.gl_csv,
        out_json=args.out_json,
        date_window_days=args.date_window_days,
        amount_policy=args.amount_policy,
        amount_tolerance=args.amount_tolerance,
    )


if __name__ == "__main__":
    main()
