# Bank ↔ General Ledger Reconciliation

## Overview
This project reconciles bank transactions against general-ledger journal entries for the same bank account.

You are given two CSV exports:

- `bank_transactions.csv`: one row per bank transaction from the bank statement feed
- `general_ledger.csv`: one row per general-ledger line item, where multiple rows may belong to the same journal entry

The goal is to match each **aggregated GL journal entry** to **at most one** bank transaction, while also ensuring that each bank transaction is matched to **at most one** journal entry.

Unmatched items are allowed on both sides.

---

## Input Files

### `bank_transactions.csv`
Expected columns:

- `datetime`: timestamp in `M/D/YY H:MM` format  
  Example: `3/14/23 0:00`
- `amount`: signed decimal amount  
  Example: `223333.33`
- `description`: free-text transaction description

### `general_ledger.csv`
Expected columns:

- `datetime`: timestamp in the same `M/D/YY H:MM` format
- `amount`: signed decimal amount for one GL line item
- `description`: free text, may be blank
- `journal_entry_id`: identifier shared by all GL lines belonging to the same journal entry

---

## Reconciliation Goal

Produce a set of matches such that:

- each aggregated GL journal entry matches **0 or 1** bank transaction
- each bank transaction matches **0 or 1** GL journal entry
- unmatched journal entries and unmatched bank transactions are allowed

This is a **one-to-one matching** problem with candidate filtering and similarity scoring.

---

## Step 1: Aggregate the General Ledger

Before matching, GL line items must be grouped by `journal_entry_id` to form journal entries.

For each journal entry, compute:

- `entry_datetime`: use the first line’s datetime (or another consistent rule)
- `entry_amount`: sum of `amount` across all lines in the journal entry
- `entry_description`: concatenate all non-empty descriptions, separated by spaces
- `num_lines`: number of GL lines in the journal entry

This converts the ledger from line-item level to journal-entry level.

---

## Step 2: Generate Candidate Matches

A bank transaction is considered a candidate for a journal entry if it passes both:

### Date window
A pair is eligible if:

```text
abs(bank_date - entry_date) <= 15 days
```

### Amount compatibility
Because accounting systems and bank feeds may use different sign conventions, amount matching should support one of the following policies:

- **Exact:** `bank_amount ≈ entry_amount`
- **Opposite-sign:** `bank_amount ≈ -entry_amount`
- **Absolute:** `abs(bank_amount) ≈ abs(entry_amount)`

Use a small tolerance such as `0.01` for decimal comparisons.

Document clearly which policy is used.

---

## Step 3: Score Candidate Pairs

For every candidate bank/journal-entry pair, compute a similarity score in the range `[0, 1]` based on description similarity.

Reasonable approaches include:

- token overlap / Jaccard similarity
- normalized edit distance
- TF-IDF cosine similarity

A good implementation should also handle messy text reasonably well. For example, it may help to:

- lowercase descriptions
- strip punctuation
- normalize repeated whitespace
- ignore empty descriptions safely

---

## Step 4: Select Final Matches

From all scored candidates, choose final matches under the one-to-one constraint:

- one journal entry can match at most one bank transaction
- one bank transaction can match at most one journal entry

### Suggested optimization objective
Choose a deterministic objective and document it. A recommended version is:

1. maximize total similarity score across all selected matches
2. maximize number of matches
3. break ties deterministically

You may implement this using a maximum-weight bipartite matching approach or another correct deterministic method.

---

## Output

Write a file named `matches.json` containing one record per matched journal entry, including:

- the journal entry information
- the chosen bank transaction
- the similarity score

A suggested shape is:

```json
[
  {
    "journal_entry_id": "JE123",
    "entry_datetime": "2023-03-14T00:00:00",
    "entry_amount": 223333.33,
    "entry_description": "wire transfer vendor payment",
    "num_lines": 3,
    "bank_transaction": {
      "datetime": "2023-03-15T00:00:00",
      "amount": -223333.33,
      "description": "wire vendor payment"
    },
    "score": 0.91
  }
]
```

Also print a short summary to stdout with:

- number of aggregated GL entries
- number of bank transactions
- number matched
- match rate as a percentage of GL entries matched
- match rate as a percentage of bank transactions matched

---

## What Matters Most

The main evaluation criteria are:

- correctness of the one-to-one matching constraints
- clear and explainable matching logic
- reasonable handling of timestamps, signs, and messy text
- code quality, readability, and structure
- tests, if included

---

## Recommended Project Structure

```text
.
├── bank_transactions.csv
├── general_ledger.csv
├── src/
│   ├── load_data.py
│   ├── aggregate_gl.py
│   ├── candidate_generation.py
│   ├── scoring.py
│   ├── matching.py
│   └── main.py
├── tests/
└── matches.json
```

This is only a suggestion. Any clean, readable structure is fine.

---

## Implementation Notes

A strong solution will usually include:

- robust CSV parsing
- explicit datetime parsing
- configurable tolerance for amount comparison
- configurable date window
- deterministic tie-breaking
- clear separation between:
  - aggregation
  - candidate generation
  - scoring
  - final assignment

If you make assumptions, document them in code comments or here in the README.

---

## Example Workflow

1. Load both CSV files
2. Aggregate GL rows into journal entries by `journal_entry_id`
3. Generate candidate bank/journal-entry pairs using date and amount filters
4. Score each candidate based on description similarity
5. Solve the one-to-one matching problem
6. Write `matches.json`
7. Print reconciliation summary

---

## Optional Enhancements

Possible improvements include:

- configurable amount-matching policy via CLI flag
- support for multiple similarity metrics
- detailed unmatched-item reports
- audit logs explaining why a pair was or was not considered
- unit tests for aggregation, amount matching, and final assignment

---

## Summary

This project is a constrained reconciliation task between two noisy financial data sources. The core challenge is balancing:

- strict one-to-one matching rules
- tolerance for real-world sign/date inconsistencies
- explainable text-based scoring

A good solution should be correct, transparent, and easy to reason about.

edit distance
max score
