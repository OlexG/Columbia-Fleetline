#include <iostream>
#include <fstream>
#include <sstream>
#include <vector>
#include <string>
#include <unordered_map>
#include <algorithm>
#include <ctime>
#include <iomanip>
#include <cmath>
#include <limits>
#include <set>
#include <cctype>
#include <climits>
using namespace std;

// Simple CSV reader (handles quoted fields)
static vector<string> split_csv_line(const string &line) {
    vector<string> out;
    string cur;
    bool in_quotes = false;
    for (size_t i = 0; i < line.size(); ++i) {
        char c = line[i];
        if (in_quotes) {
            if (c == '"') {
                if (i + 1 < line.size() && line[i+1] == '"') {
                    cur.push_back('"');
                    ++i;
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push_back(c);
            }
        } else {
            if (c == '"') {
                in_quotes = true;
            } else if (c == ',') {
                out.push_back(cur);
                cur.clear();
            } else {
                cur.push_back(c);
            }
        }
    }
    out.push_back(cur);
    return out;
}

// Parse date string to days since epoch. Supports M/D/YY H:MM and variants.
static bool parse_date_to_days(const string &s_in, long &out_days) {
    if (s_in.empty()) return false;
    string s = s_in;
    while (!s.empty() && isspace((unsigned char)s.back())) s.pop_back();
    size_t p = 0; while (p < s.size() && isspace((unsigned char)s[p])) ++p; if (p) s = s.substr(p);

    std::tm tm{};
    const vector<string> fmts = {
        "%m/%d/%y %H:%M",
        "%m/%d/%y %H:%M:%S",
        "%m/%d/%y",
        "%m/%d/%Y %H:%M",
        "%m/%d/%Y %H:%M:%S",
        "%m/%d/%Y",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d"
    };
    for (const auto &fmt : fmts) {
        std::istringstream ss(s);
        ss >> std::get_time(&tm, fmt.c_str());
        if (!ss.fail()) {
            tm.tm_isdst = -1;
            time_t t = mktime(&tm);
            if (t == -1) return false;
            out_days = t / 86400;
            return true;
        }
    }
    return false;
}

// Tokenize a string: lowercase alphanumeric tokens only
static set<string> tokenize(const string &s) {
    set<string> tokens;
    string tok;
    for (char c : s) {
        if (isalnum((unsigned char)c)) {
            tok.push_back(tolower((unsigned char)c));
        } else {
            if (!tok.empty()) { tokens.insert(tok); tok.clear(); }
        }
    }
    if (!tok.empty()) tokens.insert(tok);
    return tokens;
}

// Jaccard similarity on token sets.
// Both empty -> 1.0 (identical); one empty -> 0.0.
static double jaccard(const set<string> &a, const set<string> &b) {
    if (a.empty() && b.empty()) return 1.0;
    if (a.empty() || b.empty()) return 0.0;
    int inter = 0;
    for (const auto &t : a) if (b.count(t)) ++inter;
    int uni = (int)(a.size() + b.size() - inter);
    return (double)inter / uni;
}

// Escape a string for JSON output
static string json_escape(const string &s) {
    string r;
    for (char c : s) {
        if (c == '"')  r += "\\\"";
        else if (c == '\\') r += "\\\\";
        else if (c == '\n') r += "\\n";
        else if (c == '\r') r += "\\r";
        else if (c == '\t') r += "\\t";
        else r.push_back(c);
    }
    return r;
}

int main(int argc, char** argv) {
    string bt_path = "bank_transactions.csv";
    string gl_path = "general_ledger.csv";
    if (argc >= 3) { bt_path = argv[1]; gl_path = argv[2]; }

    auto read_csv = [&](const string &path) {
        ifstream f(path);
        if (!f) { cerr << "Failed to open " << path << "\n"; exit(1); }
        string header;
        if (!std::getline(f, header)) return vector<vector<string>>();
        vector<string> cols = split_csv_line(header);
        vector<vector<string>> rows;
        string line;
        while (std::getline(f, line)) {
            rows.push_back(split_csv_line(line));
            if (rows.back().size() < cols.size()) rows.back().resize(cols.size());
        }
        rows.insert(rows.begin(), cols);
        return rows;
    };

    auto bt_rows = read_csv(bt_path);
    auto gl_rows = read_csv(gl_path);
    if (bt_rows.empty() || gl_rows.empty()) { cerr << "Empty input\n"; return 1; }

    vector<string> bt_header = bt_rows[0];
    vector<string> gl_header = gl_rows[0];
    unordered_map<string,int> bt_col, gl_col;
    for (size_t i = 0; i < bt_header.size(); ++i) bt_col[bt_header[i]] = i;
    for (size_t i = 0; i < gl_header.size(); ++i) gl_col[gl_header[i]] = i;

    struct Bank { int idx; long day; double amount; string desc; string datetime_raw; };
    struct GL   { string journal_id; long day; double amount; string desc; string datetime_raw; };

    // --- Parse bank transactions ---
    vector<Bank> banks;
    for (size_t r = 1; r < bt_rows.size(); ++r) {
        auto &row = bt_rows[r];
        Bank b;
        b.idx = (int)(r - 1);
        string dt = bt_col.count("datetime") ? row[bt_col["datetime"]] : string("");
        b.datetime_raw = dt;
        long day = 0;
        b.day = parse_date_to_days(dt, day) ? day : LONG_MIN / 4;
        string amt_s = bt_col.count("amount") ? row[bt_col["amount"]] : string("");
        try { b.amount = stod(amt_s); } catch (...) { b.amount = numeric_limits<double>::quiet_NaN(); }
        b.desc = bt_col.count("description") ? row[bt_col["description"]] : string("");
        banks.push_back(move(b));
    }

    // --- Parse and aggregate GL by journal_entry_id ---
    // entry_datetime : first line's datetime
    // entry_amount   : sum of all line amounts
    // entry_description : space-separated non-empty descriptions from all lines
    unordered_map<string,int> seen_idx;
    vector<GL> gl_entries;
    for (size_t r = 1; r < gl_rows.size(); ++r) {
        auto &row = gl_rows[r];
        string jid = gl_col.count("journal_entry_id") ? row[gl_col["journal_entry_id"]] : to_string(r - 1);
        string line_desc = gl_col.count("description") ? row[gl_col["description"]] : string("");
        string amt_s    = gl_col.count("amount")       ? row[gl_col["amount"]]       : string("");
        double v = 0;
        try { v = stod(amt_s); } catch (...) { v = 0; }

        if (!seen_idx.count(jid)) {
            GL g;
            g.journal_id = jid;
            string dt = gl_col.count("datetime") ? row[gl_col["datetime"]] : string("");
            g.datetime_raw = dt;
            long day = 0;
            g.day = parse_date_to_days(dt, day) ? day : LONG_MIN / 4;
            g.amount = v;
            g.desc   = line_desc;
            seen_idx[jid] = (int)gl_entries.size();
            gl_entries.push_back(move(g));
        } else {
            int idx = seen_idx[jid];
            gl_entries[idx].amount += v;
            // concatenate non-empty descriptions
            if (!line_desc.empty()) {
                if (gl_entries[idx].desc.empty())
                    gl_entries[idx].desc = line_desc;
                else
                    gl_entries[idx].desc += " " + line_desc;
            }
        }
    }

    int n_bt = (int)banks.size();
    int n_gl = (int)gl_entries.size();

    // --- Build candidate pairs ---
    // Constraints:
    //   date window  : |bank_day - gl_day| <= 15
    //   amount policy: absolute — |abs(bank_amount) - abs(gl_amount)| <= 0.1
    // Score: Jaccard token similarity of descriptions
    const int    day_window = 15;
    const double tol        = 0.1;

    struct Candidate { int bank_idx; int gl_idx; double score; };
    vector<Candidate> candidates;

    for (int i = 0; i < n_bt; ++i) {
        if (banks[i].day <= LONG_MIN / 10 || isnan(banks[i].amount)) continue;
        auto bank_tok = tokenize(banks[i].desc);
        for (int j = 0; j < n_gl; ++j) {
            if (gl_entries[j].day <= LONG_MIN / 10 || isnan(gl_entries[j].amount)) continue;
            if (llabs(banks[i].day - gl_entries[j].day) > day_window) continue;
            if (fabs(fabs(banks[i].amount) - fabs(gl_entries[j].amount)) > tol) continue;
            auto gl_tok = tokenize(gl_entries[j].desc);
            double score = jaccard(bank_tok, gl_tok);
            candidates.push_back({i, j, score});
        }
    }

    // --- Score-greedy one-to-one matching ---
    // Primary objective  : maximize total similarity score
    // Secondary objective: maximize number of matches
    // Tie-break          : lower bank_idx, then lower gl_idx (deterministic)
    sort(candidates.begin(), candidates.end(), [](const Candidate &a, const Candidate &b) {
        if (a.score != b.score) return a.score > b.score;
        if (a.bank_idx != b.bank_idx) return a.bank_idx < b.bank_idx;
        return a.gl_idx < b.gl_idx;
    });

    vector<bool> bank_used(n_bt, false);
    vector<bool> gl_used(n_gl, false);

    struct Match {
        string journal_id, gl_datetime, gl_desc;
        double gl_amount;
        int    bank_index;
        string bank_datetime, bank_desc;
        double bank_amount, score;
    };
    vector<Match> matches;

    for (const auto &c : candidates) {
        if (bank_used[c.bank_idx] || gl_used[c.gl_idx]) continue;
        bank_used[c.bank_idx] = true;
        gl_used[c.gl_idx]     = true;
        const auto &b = banks[c.bank_idx];
        const auto &g = gl_entries[c.gl_idx];
        matches.push_back({g.journal_id, g.datetime_raw, g.desc, g.amount,
                           b.idx, b.datetime_raw, b.desc, b.amount, c.score});
    }

    // --- Write matches.json ---
    ofstream out("matches.json");
    out << "[\n";
    for (size_t k = 0; k < matches.size(); ++k) {
        const auto &m = matches[k];
        out << "  {\n";
        out << "    \"journal_entry_id\": \""  << json_escape(m.journal_id)   << "\",\n";
        out << "    \"gl_datetime\": "
            << (m.gl_datetime.empty() ? "null" : "\"" + json_escape(m.gl_datetime) + "\"") << ",\n";
        out << "    \"gl_amount\": "           << fixed << setprecision(2) << m.gl_amount   << ",\n";
        out << "    \"gl_description\": "
            << (m.gl_desc.empty() ? "null" : "\"" + json_escape(m.gl_desc) + "\"") << ",\n";
        out << "    \"bank_index\": "          << m.bank_index                              << ",\n";
        out << "    \"bank_datetime\": "
            << (m.bank_datetime.empty() ? "null" : "\"" + json_escape(m.bank_datetime) + "\"") << ",\n";
        out << "    \"bank_amount\": "         << fixed << setprecision(2) << m.bank_amount << ",\n";
        out << "    \"bank_description\": "
            << (m.bank_desc.empty() ? "null" : "\"" + json_escape(m.bank_desc) + "\"") << ",\n";
        out << "    \"score\": "               << fixed << setprecision(6) << m.score       << "\n";
        out << "  }" << (k + 1 < matches.size() ? "," : "") << "\n";
    }
    out << "]\n";
    out.close();

    // --- Summary ---
    int n_matched = (int)matches.size();
    cout << "GL entries:        " << n_gl    << "\n";
    cout << "Bank transactions: " << n_bt    << "\n";
    cout << "Matched:           " << n_matched << "\n";
    cout << "Bank match rate:   " << fixed << setprecision(1)
         << (n_bt > 0 ? 100.0 * n_matched / n_bt : 0.0) << "%\n";
    cout << "GL match rate:     " << fixed << setprecision(1)
         << (n_gl > 0 ? 100.0 * n_matched / n_gl : 0.0) << "%\n";

    return 0;
}
