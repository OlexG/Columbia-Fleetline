Bank ↔ General Ledger Reconciliation
You are given two CSV exports for the same bank account:
* `bank_transactions.csv`: the bank statement feed (one row per bank transaction)
* `general_ledger.csv`: accounting system general-ledger (one row per GL line item; multiple rows can belong to the same journal entry)
Your task is to reconcile the two sources by matching each aggregated journal entry to at most one bank transaction.
Data formats
`bank_transactions.csv`
Columns:
* `datetime`: a timestamp in the format `M/D/YY H:MM` (example: `3/14/23 0:00`)
* `amount`: a signed decimal number (example: `223333.33`)
* `description`: free text
`general_ledger.csv`
Columns:
* `datetime`: a timestamp in the same format `M/D/YY H:MM`
* `amount`: a signed decimal number (one GL line amount)
* `description`: free text (may be blank)
* `journal_entry_id`: identifier shared by multiple GL lines that belong to the same journal entry
Goal
Produce a set of matches:
* Each GL journal entry (after aggregation) may match 0 or 1 bank transaction.
* Each bank transaction may match 0 or 1 GL journal entry.
* Unmatched items are allowed.
Aggregating the general ledger
Before matching, convert GL line items into journal entries by grouping on `journal_entry_id` and computing:
* `entry_datetime`: choose the first line's `datetime` (or another reasonable rule, but be consistent)
* `entry_amount`: sum of `amount` across all lines in the group
* `entry_description`: concatenation of non-empty line `description` values (space-separated)
* `num_lines`: number of GL lines in the group
Candidate match rules
You decide whether a bank transaction is a candidate for a given journal entry, but the starter code assumes:
* Date window: candidate if `abs(bank_date - entry_date) <= 15 days`
* Amount compatibility: candidate if amounts match under a chosen policy (see below)
Amount policy (important)
In real datasets, signs may differ across systems (e.g., bank debits are negative while GL may record the corresponding expense with the opposite sign).
Implement an amount matching policy such as one of:
* Exact: `bank_amount ≈ entry_amount`
* Opposite-sign: `bank_amount ≈ -entry_amount`
* Absolute: `abs(bank_amount) ≈ abs(entry_amount)`
Use a small tolerance (e.g., `0.01`) to handle rounding.
Scoring candidates (description similarity)
For each candidate pair, compute a score in `[0, 1]` based on how similar the descriptions are.
Examples of reasonable approaches:
* token overlap / Jaccard similarity
* normalized edit distance
* TF-IDF cosine similarity
Selecting final matches (one-to-one constraint)
From all scored candidates, select final matches that satisfy the one-to-one constraint.
Objective (pick one, but document it):
* maximize total similarity score across all chosen matches, then
* maximize the number of matches, then
* break ties deterministically
Output
Write `matches.json` containing:
* one record per matched journal entry
* the chosen bank transaction
* the score
Suggested shape:

```json
[
  {
    "journal_entry_id": "…",
    "gl_datetime": "…",
    "gl_amount": 123.45,
    "gl_description": "…",
    "bank_index": 42,
    "bank_datetime": "…",
    "bank_amount": -123.45,
    "bank_description": "…",
    "score": 0.83
  }
]


```

Also print a brief summary:
* number of GL entries
* number of bank transactions
* number matched
* match rate (% of bank transactions matched and % of GL entries matched)
What we care about
* correctness of the matching constraints
* clear, explainable matching/scoring logic
* reasonable handling of messy text and timestamps
* code quality (readability, structure, tests if you choose)

can you explain the statement more easiliy?