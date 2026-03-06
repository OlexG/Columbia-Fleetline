import csv, json
from datetime import datetime, timedelta
from collections import defaultdict

# ---------- helpers ----------

def parse_dt(s):
    return datetime.strptime(s.strip(), "%m/%d/%y %H:%M")

def tokenize(s):
    return set(s.lower().split())

def jaccard(s1, s2):
    a, b = tokenize(s1), tokenize(s2)
    if not a and not b:
        return 1.0
    if not a or not b:
        return 0.0
    return len(a & b) / len(a | b)

AMT_TOL = 0.01
DATE_WINDOW = timedelta(days=15)

# ---------- load bank ----------

bank = []
with open("bank_transactions.csv") as f:
    for row in csv.DictReader(f):
        bank.append({
            "datetime": parse_dt(row["datetime"]),
            "amount": float(row["amount"]),
            "description": row["description"].strip(),
        })

# ---------- load & aggregate GL ----------

gl_lines = defaultdict(list)
with open("general_ledger.csv") as f:
    for row in csv.DictReader(f):
        gl_lines[row["journal_entry_id"]].append(row)

journals = []
for jid, lines in gl_lines.items():
    journals.append({
        "journal_entry_id": jid,
        "datetime": parse_dt(lines[0]["datetime"]),
        "amount": round(sum(float(l["amount"]) for l in lines), 2),
        "description": " ".join(l["description"].strip() for l in lines if l["description"].strip()),
        "num_lines": len(lines),
    })

# ---------- build candidate edges (amount + date filter) ----------

# Amount policy: absolute match  (|bank| ≈ |gl|)
edges = []  # (bank_idx, journal_idx, score)
for ji, j in enumerate(journals):
    j_abs = abs(j["amount"])
    j_dt = j["datetime"]
    for bi, b in enumerate(bank):
        if abs(abs(b["amount"]) - j_abs) > AMT_TOL:
            continue
        if abs((b["datetime"] - j_dt).total_seconds()) > DATE_WINDOW.total_seconds():
            continue
        score = jaccard(b["description"], j["description"])
        edges.append((bi, ji, score))

# ---------- optimal 1-to-1 matching via Hungarian algorithm ----------

if edges:
    # Build cost matrix for only the nodes that appear in edges
    bank_ids = sorted({e[0] for e in edges})
    journal_ids = sorted({e[1] for e in edges})
    b_map = {b: i for i, b in enumerate(bank_ids)}
    j_map = {j: i for i, j in enumerate(journal_ids)}

    import numpy as np
    from scipy.optimize import linear_sum_assignment

    # Large cost = no edge; we maximise score so use (1 - score) as cost
    BIG = 2.0
    cost = np.full((len(bank_ids), len(journal_ids)), BIG)
    for bi, ji, sc in edges:
        r, c = b_map[bi], j_map[ji]
        # keep best score if multiple edges between same pair (shouldn't happen)
        cost[r, c] = min(cost[r, c], 1.0 - sc)

    row_ind, col_ind = linear_sum_assignment(cost)

    matches = []
    for r, c in zip(row_ind, col_ind):
        if cost[r, c] >= BIG:
            continue
        bi = bank_ids[r]
        ji = journal_ids[c]
        sc = 1.0 - cost[r, c]
        b = bank[bi]
        j = journals[ji]
        matches.append({
            "journal_entry_id": j["journal_entry_id"],
            "gl_datetime": j["datetime"].strftime("%Y-%m-%d %H:%M"),
            "gl_amount": j["amount"],
            "gl_description": j["description"],
            "bank_index": bi,
            "bank_datetime": b["datetime"].strftime("%Y-%m-%d %H:%M"),
            "bank_amount": b["amount"],
            "bank_description": b["description"],
            "score": round(sc, 4),
        })
else:
    matches = []

# ---------- output ----------

with open("matches.json", "w") as f:
    json.dump(matches, f, indent=2)

n_gl = len(journals)
n_bank = len(bank)
n_matched = len(matches)
print(f"GL entries:      {n_gl}")
print(f"Bank txns:       {n_bank}")
print(f"Matched:         {n_matched}")
print(f"GL match rate:   {n_matched/n_gl*100:.1f}%")
print(f"Bank match rate: {n_matched/n_bank*100:.1f}%")