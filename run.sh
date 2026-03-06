#!/bin/bash
set -e
cd "$(dirname "$0")"
echo "Compiling..."
g++ -std=c++17 -O2 -o solve solve.cpp
echo "Running..."
./solve data/bank_transactions.csv data/general_ledger.csv
echo "Done. Output: matches.json"
