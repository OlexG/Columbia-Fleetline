# Columbia-Fleetline

This small C++ program reproduces the matching logic from `test.ipynb` and writes `matches.json`.

Build (requires g++ or CMake):

Using g++ directly:

```bash
g++ -std=c++17 -O3 -march=native main.cpp -o match
```

Or with CMake:

```bash
mkdir -p build && cd build
cmake ..
cmake --build . --config Release
```

Run:

```bash
./match bank_transactions.csv general_ledger.csv
```

If no arguments provided the program expects `bank_transactions.csv` and `general_ledger.csv` in the current directory.

Notes:
- Implements the same candidate rules: 15-day date window, absolute-amount tolerance 0.01, and requires both descriptions present.
- Uses a Hopcroft-Karp max-matching (fast). Levenshtein distance is computed for score output.
- The program aims to produce the same matching pairs as the notebook (weights were not used by the original Ford-Fulkerson matching). Score calculation uses a simple ratio derived from Levenshtein distance.
