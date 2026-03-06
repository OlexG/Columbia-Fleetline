use std::cmp::Ordering;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::env;
use std::error::Error;
use std::fs::File;
use std::io::{self, BufWriter};

use csv::ReaderBuilder;
use rustc_hash::FxHashMap;
use serde::Serialize;

const DEFAULT_BANK_PATH: &str = "bank_transactions.csv";
const DEFAULT_GL_PATH: &str = "general_ledger.csv";
const DEFAULT_OUTPUT_PATH: &str = "matches.json";
const MINUTES_PER_DAY: i64 = 24 * 60;
const DATE_WINDOW_MINUTES: i64 = 15 * MINUTES_PER_DAY;
const AMOUNT_TOLERANCE_CENTS: i64 = 1;
const SCORE_SCALE: f64 = 1_000_000.0;
const MAX_SCORE_PPM: i64 = SCORE_SCALE as i64;
const SCORE_COST_WEIGHT: i64 = 100_000;
const GAP_COST_WEIGHT: i64 = 1;
const MIN_TEXT_SCORE_PPM: u32 = 50_000;
const STRONG_TEXT_SCORE_PPM: u32 = 200_000;
const NEAR_DATE_WINDOW_MINUTES: i64 = MINUTES_PER_DAY;
const WEAK_TEXT_DATE_WINDOW_MINUTES: i64 = 3 * MINUTES_PER_DAY;

const STOPWORDS: &[&str] = &[
    "a", "at", "bank", "by", "com", "dda", "for", "from", "in", "inc", "ll", "llc", "on",
    "svc", "tax", "to", "transferred", "usataxpymt",
];

type DynResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Algorithm {
    Heuristic,
    MaxFlow,
}

impl Algorithm {
    fn parse(raw: &str) -> DynResult<Self> {
        match raw {
            "heuristic" => Ok(Self::Heuristic),
            "maxflow" => Ok(Self::MaxFlow),
            _ => Err(invalid_data(format!("unknown algorithm: {raw}")).into()),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Heuristic => "heuristic",
            Self::MaxFlow => "maxflow",
        }
    }

    fn selection_strategy(self) -> &'static str {
        match self {
            Self::Heuristic => "sparse indexed candidates + deterministic augmenting greedy matcher",
            Self::MaxFlow => "max-cardinality flow network + min-cost flow refinement on candidate graph",
        }
    }
}

struct CliOptions {
    algorithm: Algorithm,
    bank_path: String,
    gl_path: String,
    output_path: String,
}

#[derive(Clone, Copy)]
struct GapStats {
    best_gap: i64,
    best_count: usize,
    second_best_gap: i64,
}

#[derive(Clone)]
struct BankTransaction {
    index: usize,
    datetime_raw: String,
    datetime_key: i64,
    amount_cents: i64,
    abs_amount_cents: i64,
    description_raw: String,
    tokens: Vec<u32>,
    token_weight_sum: f64,
}

#[derive(Clone)]
struct JournalEntry {
    journal_entry_id: String,
    datetime_raw: String,
    datetime_key: i64,
    amount_cents: i64,
    abs_amount_cents: i64,
    description_raw: String,
    num_lines: usize,
    tokens: Vec<u32>,
    token_weight_sum: f64,
}

struct JournalEntryBuilder {
    journal_entry_id: String,
    datetime_raw: String,
    datetime_key: i64,
    amount_cents: i64,
    description_raw: String,
    num_lines: usize,
}

#[derive(Clone, Copy)]
struct Candidate {
    bank_idx: usize,
    score_ppm: u32,
    minute_gap: i64,
}

#[derive(Clone, Copy)]
struct Assignment {
    bank_idx: usize,
    score_ppm: u32,
    minute_gap: i64,
}

#[derive(Serialize)]
struct MatchRecord<'a> {
    journal_entry_id: &'a str,
    gl_datetime: &'a str,
    gl_amount: f64,
    gl_description: &'a str,
    num_lines: usize,
    bank_index: usize,
    bank_datetime: &'a str,
    bank_amount: f64,
    bank_description: &'a str,
    score: f64,
}

#[derive(Default)]
struct TokenInterner {
    ids: FxHashMap<String, u32>,
}

impl TokenInterner {
    fn tokenize(&mut self, text: &str) -> Vec<u32> {
        let mut ids = Vec::<u32>::new();
        let mut token = String::with_capacity(16);

        for byte in text.bytes() {
            if byte.is_ascii_alphanumeric() {
                token.push((byte as char).to_ascii_lowercase());
            } else {
                self.flush_token(&mut token, &mut ids);
            }
        }
        self.flush_token(&mut token, &mut ids);
        self.add_synthetic_tokens(text, &mut ids);

        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn flush_token(&mut self, token: &mut String, ids: &mut Vec<u32>) {
        if token.is_empty() {
            return;
        }

        if let Some(normalized) = canonicalize_token(token) {
            ids.push(self.intern_token(normalized));
        }

        token.clear();
    }

    fn add_synthetic_tokens(&mut self, text: &str, ids: &mut Vec<u32>) {
        let lower = text.to_ascii_lowercase();

        if lower.contains("quickbooks") || lower.contains("intuit") {
            ids.push(self.intern_token("intuit"));
        }
        if (lower.contains("quickbooks") || lower.contains("intuit"))
            && (lower.contains("payment") || lower.contains("deposit"))
        {
            ids.push(self.intern_token("intuit_payment"));
        }
        if lower.contains("bill.com") || lower.contains("bill com") {
            ids.push(self.intern_token("billcom"));
        }
        if lower.contains("mobile deposit") {
            ids.push(self.intern_token("mobile_deposit"));
        }
        if lower.contains("incoming wire") {
            ids.push(self.intern_token("incoming_wire"));
        }
        if lower.contains("ach credit") {
            ids.push(self.intern_token("ach_credit"));
        }
        if lower.contains("ach debit") {
            ids.push(self.intern_token("ach_debit"));
        }
        if lower.contains("internet transfer") {
            ids.push(self.intern_token("internet_transfer"));
        }
        if lower.contains("apple.com/bill") || lower.contains("apple.com/bil") {
            ids.push(self.intern_token("apple_bill"));
        }
    }

    fn intern_token(&mut self, token: &str) -> u32 {
        if let Some(&id) = self.ids.get(token) {
            return id;
        }

        let id = self.ids.len() as u32;
        self.ids.insert(token.to_owned(), id);
        id
    }

    fn len(&self) -> usize {
        self.ids.len()
    }
}

fn main() -> DynResult<()> {
    let options = parse_args(env::args().skip(1))?;

    let mut interner = TokenInterner::default();
    let mut bank_transactions = load_bank_transactions(&options.bank_path, &mut interner)?;
    let mut journal_entries = load_journal_entries(&options.gl_path, &mut interner)?;

    let token_weights = build_token_weights(&bank_transactions, &journal_entries, interner.len());
    apply_token_weights(&mut bank_transactions, &token_weights);
    apply_token_weights(&mut journal_entries, &token_weights);

    let gl_candidates = build_candidates(&bank_transactions, &journal_entries, &token_weights);
    let assignments = match options.algorithm {
        Algorithm::Heuristic => match_heuristic(&gl_candidates, bank_transactions.len()),
        Algorithm::MaxFlow => match_maxflow(&gl_candidates, bank_transactions.len()),
    };
    let matches = build_match_records(&assignments, &bank_transactions, &journal_entries);

    let file = File::create(&options.output_path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, &matches)?;

    print_summary(
        options.algorithm,
        journal_entries.len(),
        bank_transactions.len(),
        matches.len(),
        &gl_candidates,
    );

    Ok(())
}

fn parse_args<I>(args: I) -> DynResult<CliOptions>
where
    I: IntoIterator<Item = String>,
{
    let mut algorithm = Algorithm::Heuristic;
    let mut positionals = Vec::new();
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--algorithm" => {
                let value = iter
                    .next()
                    .ok_or_else(|| invalid_data("missing value after --algorithm"))?;
                algorithm = Algorithm::parse(&value)?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: comp [--algorithm heuristic|maxflow] [bank_transactions.csv] [general_ledger.csv] [matches.json]"
                );
                std::process::exit(0);
            }
            _ if arg.starts_with("--algorithm=") => {
                let value = arg.split_once('=').map(|(_, value)| value).unwrap_or_default();
                algorithm = Algorithm::parse(value)?;
            }
            _ if arg.starts_with('-') => {
                return Err(invalid_data(format!("unknown flag: {arg}")).into());
            }
            _ => positionals.push(arg),
        }
    }

    let bank_path = positionals
        .first()
        .cloned()
        .unwrap_or_else(|| DEFAULT_BANK_PATH.to_owned());
    let gl_path = positionals
        .get(1)
        .cloned()
        .unwrap_or_else(|| DEFAULT_GL_PATH.to_owned());
    let output_path = positionals
        .get(2)
        .cloned()
        .unwrap_or_else(|| DEFAULT_OUTPUT_PATH.to_owned());

    Ok(CliOptions {
        algorithm,
        bank_path,
        gl_path,
        output_path,
    })
}

fn load_bank_transactions(path: &str, interner: &mut TokenInterner) -> DynResult<Vec<BankTransaction>> {
    let mut rdr = ReaderBuilder::new().trim(csv::Trim::All).from_path(path)?;
    let mut rows = Vec::new();

    for (index, record) in rdr.records().enumerate() {
        let record = record?;
        let datetime_raw = required_field(&record, 0, "datetime")?.trim().to_owned();
        let amount_raw = required_field(&record, 1, "amount")?.trim();
        let description_raw = required_field(&record, 2, "description")?.trim().to_owned();

        let datetime_key = parse_datetime_key(&datetime_raw)?;
        let amount_cents = parse_amount_cents(amount_raw)?;
        let tokens = interner.tokenize(&description_raw);

        rows.push(BankTransaction {
            index,
            datetime_raw,
            datetime_key,
            amount_cents,
            abs_amount_cents: amount_cents.abs(),
            description_raw,
            tokens,
            token_weight_sum: 0.0,
        });
    }

    Ok(rows)
}

fn load_journal_entries(path: &str, interner: &mut TokenInterner) -> DynResult<Vec<JournalEntry>> {
    let mut rdr = ReaderBuilder::new().trim(csv::Trim::All).from_path(path)?;
    let mut builders = Vec::<JournalEntryBuilder>::new();
    let mut positions = FxHashMap::<String, usize>::default();

    for record in rdr.records() {
        let record = record?;
        let datetime_raw = required_field(&record, 0, "datetime")?.trim().to_owned();
        let amount_raw = required_field(&record, 1, "amount")?.trim();
        let description_raw = required_field(&record, 2, "description")?.trim();
        let journal_entry_id = required_field(&record, 3, "journal_entry_id")?.trim().to_owned();

        let datetime_key = parse_datetime_key(&datetime_raw)?;
        let amount_cents = parse_amount_cents(amount_raw)?;

        let idx = if let Some(&idx) = positions.get(&journal_entry_id) {
            idx
        } else {
            let idx = builders.len();
            positions.insert(journal_entry_id.clone(), idx);
            builders.push(JournalEntryBuilder {
                journal_entry_id,
                datetime_raw,
                datetime_key,
                amount_cents: 0,
                description_raw: String::new(),
                num_lines: 0,
            });
            idx
        };

        let builder = &mut builders[idx];
        builder.amount_cents += amount_cents;
        builder.num_lines += 1;

        if !description_raw.is_empty() {
            if !builder.description_raw.is_empty() {
                builder.description_raw.push(' ');
            }
            builder.description_raw.push_str(description_raw);
        }
    }

    let mut entries = Vec::with_capacity(builders.len());
    for builder in builders {
        let tokens = interner.tokenize(&builder.description_raw);
        entries.push(JournalEntry {
            journal_entry_id: builder.journal_entry_id,
            datetime_raw: builder.datetime_raw,
            datetime_key: builder.datetime_key,
            amount_cents: builder.amount_cents,
            abs_amount_cents: builder.amount_cents.abs(),
            description_raw: builder.description_raw,
            num_lines: builder.num_lines,
            tokens,
            token_weight_sum: 0.0,
        });
    }

    Ok(entries)
}

fn build_token_weights(
    bank_transactions: &[BankTransaction],
    journal_entries: &[JournalEntry],
    token_count: usize,
) -> Vec<f64> {
    let total_docs = (bank_transactions.len() + journal_entries.len()) as f64;
    let mut doc_freq = vec![0u32; token_count];

    for bank in bank_transactions {
        for &token in &bank.tokens {
            doc_freq[token as usize] += 1;
        }
    }

    for entry in journal_entries {
        for &token in &entry.tokens {
            doc_freq[token as usize] += 1;
        }
    }

    doc_freq
        .into_iter()
        .map(|df| ((total_docs + 1.0) / (df as f64 + 1.0)).ln() + 1.0)
        .collect()
}

fn apply_token_weights<T>(items: &mut [T], token_weights: &[f64])
where
    T: HasTokens,
{
    for item in items {
        let sum = item
            .tokens()
            .iter()
            .map(|&token| token_weights[token as usize])
            .sum();
        item.set_token_weight_sum(sum);
    }
}

trait HasTokens {
    fn tokens(&self) -> &[u32];
    fn set_token_weight_sum(&mut self, value: f64);
}

impl HasTokens for BankTransaction {
    fn tokens(&self) -> &[u32] {
        &self.tokens
    }

    fn set_token_weight_sum(&mut self, value: f64) {
        self.token_weight_sum = value;
    }
}

impl HasTokens for JournalEntry {
    fn tokens(&self) -> &[u32] {
        &self.tokens
    }

    fn set_token_weight_sum(&mut self, value: f64) {
        self.token_weight_sum = value;
    }
}

fn build_candidates(
    bank_transactions: &[BankTransaction],
    journal_entries: &[JournalEntry],
    token_weights: &[f64],
) -> Vec<Vec<Candidate>> {
    let mut bank_amount_index = FxHashMap::<i64, Vec<usize>>::default();
    for (idx, bank) in bank_transactions.iter().enumerate() {
        bank_amount_index.entry(bank.abs_amount_cents).or_default().push(idx);
    }

    for bucket in bank_amount_index.values_mut() {
        bucket.sort_unstable_by_key(|&idx| bank_transactions[idx].datetime_key);
    }

    let mut gl_candidates = vec![Vec::<Candidate>::new(); journal_entries.len()];
    for (gl_idx, entry) in journal_entries.iter().enumerate() {
        for amount in amount_bucket_range(entry.abs_amount_cents) {
            let Some(bucket) = bank_amount_index.get(&amount) else {
                continue;
            };

            let min_time = entry.datetime_key - DATE_WINDOW_MINUTES;
            let max_time = entry.datetime_key + DATE_WINDOW_MINUTES;
            let start = bucket.partition_point(|&idx| bank_transactions[idx].datetime_key < min_time);
            let end = bucket.partition_point(|&idx| bank_transactions[idx].datetime_key <= max_time);

            for &bank_idx in &bucket[start..end] {
                let bank = &bank_transactions[bank_idx];
                if abs_diff_i64(bank.abs_amount_cents, entry.abs_amount_cents) > AMOUNT_TOLERANCE_CENTS {
                    continue;
                }

                let minute_gap = abs_diff_i64(bank.datetime_key, entry.datetime_key);
                let score_ppm = weighted_jaccard_ppm(
                    &entry.tokens,
                    entry.token_weight_sum,
                    &bank.tokens,
                    bank.token_weight_sum,
                    token_weights,
                );

                gl_candidates[gl_idx].push(Candidate {
                    bank_idx,
                    score_ppm,
                    minute_gap,
                });
            }
        }
    }

    gl_candidates
}

fn match_heuristic(gl_candidates: &[Vec<Candidate>], bank_count: usize) -> Vec<Option<Assignment>> {
    let mut bank_candidate_counts = vec![0usize; bank_count];
    for candidates in gl_candidates {
        for candidate in candidates {
            bank_candidate_counts[candidate.bank_idx] += 1;
        }
    }
    let gl_candidate_counts: Vec<usize> = gl_candidates.iter().map(Vec::len).collect();
    let gap_stats: Vec<GapStats> = gl_candidates.iter().map(|candidates| compute_gap_stats(candidates)).collect();
    let mut filtered_candidates = Vec::with_capacity(gl_candidates.len());
    let mut filtered_bank_candidate_counts = vec![0usize; bank_count];
    for (gl_idx, original) in gl_candidates.iter().enumerate() {
        let mut filtered = Vec::with_capacity(original.len());
        for &candidate in original {
            if heuristic_candidate_allowed(
                gl_idx,
                candidate,
                gap_stats[gl_idx],
                &gl_candidate_counts,
                &bank_candidate_counts,
            ) {
                filtered_bank_candidate_counts[candidate.bank_idx] += 1;
                filtered.push(candidate);
            }
        }
        filtered_candidates.push(filtered);
    }
    let filtered_gl_candidate_counts: Vec<usize> = filtered_candidates.iter().map(Vec::len).collect();

    for candidates in &mut filtered_candidates {
        if candidates.len() > 1 {
            candidates.sort_unstable_by(|left, right| {
                compare_gl_candidates(*left, *right, &filtered_bank_candidate_counts)
            });
        }
    }

    let mut gl_order: Vec<usize> = (0..filtered_candidates.len()).collect();
    gl_order.sort_unstable_by(|&left_idx, &right_idx| {
        compare_gl_order(
            left_idx,
            right_idx,
            &filtered_candidates,
            &filtered_gl_candidate_counts,
            &filtered_bank_candidate_counts,
        )
    });

    let mut assignments = vec![None; filtered_candidates.len()];
    let mut bank_owners = vec![None; bank_count];
    let mut seen_banks = vec![false; bank_count];

    for _ in 0..2 {
        for &gl_idx in &gl_order {
            if assignments[gl_idx].is_some() || filtered_candidates[gl_idx].is_empty() {
                continue;
            }

            seen_banks.fill(false);
            let _ = try_assign(
                gl_idx,
                &filtered_candidates,
                &filtered_gl_candidate_counts,
                &filtered_bank_candidate_counts,
                &mut assignments,
                &mut bank_owners,
                &mut seen_banks,
            );
        }
    }

    assignments
}

fn heuristic_candidate_allowed(
    gl_idx: usize,
    candidate: Candidate,
    gap_stats: GapStats,
    gl_candidate_counts: &[usize],
    bank_candidate_counts: &[usize],
) -> bool {
    if candidate.score_ppm >= STRONG_TEXT_SCORE_PPM {
        return true;
    }

    let unique_gl = gl_candidate_counts[gl_idx] == 1;
    let unique_bank = bank_candidate_counts[candidate.bank_idx] == 1;
    let exact_date = candidate.minute_gap == 0;
    let near_date = candidate.minute_gap <= NEAR_DATE_WINDOW_MINUTES;
    let second_best_gap = next_best_gap(gap_stats, candidate.minute_gap);
    let decisively_closest = candidate.minute_gap + NEAR_DATE_WINDOW_MINUTES < second_best_gap;

    if candidate.score_ppm >= MIN_TEXT_SCORE_PPM {
        return exact_date
            || (candidate.minute_gap <= WEAK_TEXT_DATE_WINDOW_MINUTES
                && (unique_gl || unique_bank || decisively_closest));
    }

    if candidate.score_ppm > 0 {
        return exact_date || (near_date && (unique_gl || unique_bank || decisively_closest));
    }

    (exact_date && unique_gl) || (near_date && unique_gl && unique_bank) || (exact_date && unique_bank && decisively_closest)
}

fn compute_gap_stats(candidates: &[Candidate]) -> GapStats {
    let mut best_gap = i64::MAX;
    let mut best_count = 0usize;
    let mut second_best_gap = i64::MAX;

    for candidate in candidates {
        let gap = candidate.minute_gap;
        if gap < best_gap {
            second_best_gap = best_gap;
            best_gap = gap;
            best_count = 1;
        } else if gap == best_gap {
            best_count += 1;
        } else if gap < second_best_gap {
            second_best_gap = gap;
        }
    }

    GapStats {
        best_gap,
        best_count,
        second_best_gap,
    }
}

fn next_best_gap(gap_stats: GapStats, candidate_gap: i64) -> i64 {
    if candidate_gap == gap_stats.best_gap && gap_stats.best_count == 1 {
        gap_stats.second_best_gap
    } else {
        gap_stats.best_gap
    }
}

fn try_assign(
    gl_idx: usize,
    gl_candidates: &[Vec<Candidate>],
    gl_candidate_counts: &[usize],
    bank_candidate_counts: &[usize],
    assignments: &mut [Option<Assignment>],
    bank_owners: &mut [Option<usize>],
    seen_banks: &mut [bool],
) -> bool {
    for candidate in &gl_candidates[gl_idx] {
        if seen_banks[candidate.bank_idx] {
            continue;
        }
        seen_banks[candidate.bank_idx] = true;

        match bank_owners[candidate.bank_idx] {
            None => {
                assignments[gl_idx] = Some(Assignment {
                    bank_idx: candidate.bank_idx,
                    score_ppm: candidate.score_ppm,
                    minute_gap: candidate.minute_gap,
                });
                bank_owners[candidate.bank_idx] = Some(gl_idx);
                return true;
            }
            Some(owner_idx) => {
                let incumbent = assignments[owner_idx].expect("occupied bank must have an assignment");
                if !bank_prefers(
                    gl_idx,
                    *candidate,
                    owner_idx,
                    incumbent,
                    gl_candidate_counts,
                    bank_candidate_counts,
                ) {
                    continue;
                }

                assignments[owner_idx] = None;
                assignments[gl_idx] = Some(Assignment {
                    bank_idx: candidate.bank_idx,
                    score_ppm: candidate.score_ppm,
                    minute_gap: candidate.minute_gap,
                });
                bank_owners[candidate.bank_idx] = Some(gl_idx);

                if try_assign(
                    owner_idx,
                    gl_candidates,
                    gl_candidate_counts,
                    bank_candidate_counts,
                    assignments,
                    bank_owners,
                    seen_banks,
                ) {
                    return true;
                }

                assignments[gl_idx] = None;
                assignments[owner_idx] = Some(incumbent);
                bank_owners[candidate.bank_idx] = Some(owner_idx);
            }
        }
    }

    false
}

fn match_maxflow(gl_candidates: &[Vec<Candidate>], bank_count: usize) -> Vec<Option<Assignment>> {
    let max_cardinality = max_cardinality_flow(gl_candidates, bank_count);
    min_cost_flow_assign(gl_candidates, bank_count, max_cardinality)
}

fn max_cardinality_flow(gl_candidates: &[Vec<Candidate>], bank_count: usize) -> usize {
    let gl_count = gl_candidates.len();
    let source = 0usize;
    let gl_offset = 1usize;
    let bank_offset = gl_offset + gl_count;
    let sink = bank_offset + bank_count;
    let mut dinic = Dinic::new(sink + 1);

    for gl_idx in 0..gl_count {
        dinic.add_edge(source, gl_offset + gl_idx, 1);
    }
    for bank_idx in 0..bank_count {
        dinic.add_edge(bank_offset + bank_idx, sink, 1);
    }
    for (gl_idx, candidates) in gl_candidates.iter().enumerate() {
        for candidate in candidates {
            dinic.add_edge(gl_offset + gl_idx, bank_offset + candidate.bank_idx, 1);
        }
    }

    dinic.max_flow(source, sink) as usize
}

fn min_cost_flow_assign(
    gl_candidates: &[Vec<Candidate>],
    bank_count: usize,
    target_flow: usize,
) -> Vec<Option<Assignment>> {
    let gl_count = gl_candidates.len();
    let source = 0usize;
    let gl_offset = 1usize;
    let bank_offset = gl_offset + gl_count;
    let sink = bank_offset + bank_count;
    let mut mcmf = MinCostMaxFlow::new(sink + 1);
    let mut edge_slots = vec![Vec::<(usize, Candidate)>::new(); gl_count];

    for gl_idx in 0..gl_count {
        mcmf.add_edge(source, gl_offset + gl_idx, 1, 0);
    }
    for bank_idx in 0..bank_count {
        mcmf.add_edge(bank_offset + bank_idx, sink, 1, 0);
    }
    for (gl_idx, candidates) in gl_candidates.iter().enumerate() {
        for &candidate in candidates {
            let cost = candidate_cost(candidate);
            let edge_idx = mcmf.add_edge(gl_offset + gl_idx, bank_offset + candidate.bank_idx, 1, cost);
            edge_slots[gl_idx].push((edge_idx, candidate));
        }
    }

    let (flow, _) = mcmf.min_cost_flow(source, sink, target_flow as i32);
    debug_assert_eq!(flow as usize, target_flow);

    let mut assignments = vec![None; gl_count];
    for gl_idx in 0..gl_count {
        let node = gl_offset + gl_idx;
        for &(edge_idx, candidate) in &edge_slots[gl_idx] {
            if mcmf.graph[node][edge_idx].cap == 0 {
                assignments[gl_idx] = Some(Assignment {
                    bank_idx: candidate.bank_idx,
                    score_ppm: candidate.score_ppm,
                    minute_gap: candidate.minute_gap,
                });
                break;
            }
        }
    }

    assignments
}

fn candidate_cost(candidate: Candidate) -> i64 {
    (MAX_SCORE_PPM - i64::from(candidate.score_ppm)) * SCORE_COST_WEIGHT
        + candidate.minute_gap * GAP_COST_WEIGHT
        + candidate.bank_idx as i64
}

struct Dinic {
    graph: Vec<Vec<DinicEdge>>,
    level: Vec<i32>,
    iters: Vec<usize>,
}

#[derive(Clone)]
struct DinicEdge {
    to: usize,
    rev: usize,
    cap: i32,
}

impl Dinic {
    fn new(node_count: usize) -> Self {
        Self {
            graph: vec![Vec::new(); node_count],
            level: vec![0; node_count],
            iters: vec![0; node_count],
        }
    }

    fn add_edge(&mut self, from: usize, to: usize, cap: i32) {
        let rev_to = self.graph[to].len();
        let rev_from = self.graph[from].len();
        self.graph[from].push(DinicEdge {
            to,
            rev: rev_to,
            cap,
        });
        self.graph[to].push(DinicEdge {
            to: from,
            rev: rev_from,
            cap: 0,
        });
    }

    fn max_flow(&mut self, source: usize, sink: usize) -> i32 {
        let mut flow = 0;
        while self.bfs(source, sink) {
            self.iters.fill(0);
            loop {
                let pushed = self.dfs(source, sink, i32::MAX);
                if pushed == 0 {
                    break;
                }
                flow += pushed;
            }
        }
        flow
    }

    fn bfs(&mut self, source: usize, sink: usize) -> bool {
        self.level.fill(-1);
        let mut queue = std::collections::VecDeque::new();
        self.level[source] = 0;
        queue.push_back(source);

        while let Some(node) = queue.pop_front() {
            for edge in &self.graph[node] {
                if edge.cap <= 0 || self.level[edge.to] >= 0 {
                    continue;
                }
                self.level[edge.to] = self.level[node] + 1;
                if edge.to == sink {
                    return true;
                }
                queue.push_back(edge.to);
            }
        }

        self.level[sink] >= 0
    }

    fn dfs(&mut self, node: usize, sink: usize, flow: i32) -> i32 {
        if node == sink {
            return flow;
        }

        while self.iters[node] < self.graph[node].len() {
            let edge_idx = self.iters[node];
            let edge = self.graph[node][edge_idx].clone();

            if edge.cap > 0 && self.level[node] < self.level[edge.to] {
                let pushed = self.dfs(edge.to, sink, flow.min(edge.cap));
                if pushed > 0 {
                    self.graph[node][edge_idx].cap -= pushed;
                    let rev = edge.rev;
                    self.graph[edge.to][rev].cap += pushed;
                    return pushed;
                }
            }

            self.iters[node] += 1;
        }

        0
    }
}

struct MinCostMaxFlow {
    graph: Vec<Vec<CostEdge>>,
}

#[derive(Clone)]
struct CostEdge {
    to: usize,
    rev: usize,
    cap: i32,
    cost: i64,
}

impl MinCostMaxFlow {
    fn new(node_count: usize) -> Self {
        Self {
            graph: vec![Vec::new(); node_count],
        }
    }

    fn add_edge(&mut self, from: usize, to: usize, cap: i32, cost: i64) -> usize {
        let rev_to = self.graph[to].len();
        let rev_from = self.graph[from].len();
        self.graph[from].push(CostEdge {
            to,
            rev: rev_to,
            cap,
            cost,
        });
        self.graph[to].push(CostEdge {
            to: from,
            rev: rev_from,
            cap: 0,
            cost: -cost,
        });
        rev_from
    }

    fn min_cost_flow(&mut self, source: usize, sink: usize, target_flow: i32) -> (i32, i64) {
        let node_count = self.graph.len();
        let mut flow = 0i32;
        let mut cost = 0i64;
        let mut potential = vec![0i64; node_count];
        let mut dist = vec![0i64; node_count];
        let mut prev_node = vec![0usize; node_count];
        let mut prev_edge = vec![0usize; node_count];

        while flow < target_flow {
            dist.fill(i64::MAX / 4);
            dist[source] = 0;
            let mut heap = BinaryHeap::new();
            heap.push((Reverse(0i64), source));

            while let Some((Reverse(cur_dist), node)) = heap.pop() {
                if cur_dist != dist[node] {
                    continue;
                }

                for (edge_idx, edge) in self.graph[node].iter().enumerate() {
                    if edge.cap <= 0 {
                        continue;
                    }
                    let next_cost = cur_dist + edge.cost + potential[node] - potential[edge.to];
                    if next_cost < dist[edge.to] {
                        dist[edge.to] = next_cost;
                        prev_node[edge.to] = node;
                        prev_edge[edge.to] = edge_idx;
                        heap.push((Reverse(next_cost), edge.to));
                    }
                }
            }

            if dist[sink] == i64::MAX / 4 {
                break;
            }

            for node in 0..node_count {
                if dist[node] < i64::MAX / 4 {
                    potential[node] += dist[node];
                }
            }

            let mut add_flow = target_flow - flow;
            let mut node = sink;
            while node != source {
                let prev = prev_node[node];
                let edge = &self.graph[prev][prev_edge[node]];
                add_flow = add_flow.min(edge.cap);
                node = prev;
            }

            node = sink;
            while node != source {
                let prev = prev_node[node];
                let edge_idx = prev_edge[node];
                let rev = self.graph[prev][edge_idx].rev;
                self.graph[prev][edge_idx].cap -= add_flow;
                self.graph[node][rev].cap += add_flow;
                node = prev;
            }

            flow += add_flow;
            cost += i64::from(add_flow) * potential[sink];
        }

        (flow, cost)
    }
}

fn build_match_records<'a>(
    assignments: &'a [Option<Assignment>],
    bank_transactions: &'a [BankTransaction],
    journal_entries: &'a [JournalEntry],
) -> Vec<MatchRecord<'a>> {
    let mut matches = Vec::new();

    for (gl_idx, assignment) in assignments.iter().enumerate() {
        let Some(assignment) = assignment else {
            continue;
        };

        let gl = &journal_entries[gl_idx];
        let bank = &bank_transactions[assignment.bank_idx];

        matches.push(MatchRecord {
            journal_entry_id: &gl.journal_entry_id,
            gl_datetime: &gl.datetime_raw,
            gl_amount: cents_to_f64(gl.amount_cents),
            gl_description: &gl.description_raw,
            num_lines: gl.num_lines,
            bank_index: bank.index,
            bank_datetime: &bank.datetime_raw,
            bank_amount: cents_to_f64(bank.amount_cents),
            bank_description: &bank.description_raw,
            score: assignment.score_ppm as f64 / SCORE_SCALE,
        });
    }

    matches
}

fn print_summary(
    algorithm: Algorithm,
    gl_count: usize,
    bank_count: usize,
    matched_count: usize,
    gl_candidates: &[Vec<Candidate>],
) {
    let gl_rate = percentage(matched_count, gl_count);
    let bank_rate = percentage(matched_count, bank_count);
    let candidate_free_entries = gl_candidates.iter().filter(|candidates| candidates.is_empty()).count();

    println!("Algorithm: {}", algorithm.label());
    println!("GL entries: {gl_count}");
    println!("Bank transactions: {bank_count}");
    println!("Matched: {matched_count}");
    println!("GL match rate: {gl_rate:.2}%");
    println!("Bank match rate: {bank_rate:.2}%");
    println!("GL entries with no candidates: {candidate_free_entries}");
    println!("Amount policy: absolute-value cents within {AMOUNT_TOLERANCE_CENTS} cent");
    println!("Selection strategy: {}", algorithm.selection_strategy());
}

fn compare_gl_candidates(
    left: Candidate,
    right: Candidate,
    bank_candidate_counts: &[usize],
) -> Ordering {
    right
        .score_ppm
        .cmp(&left.score_ppm)
        .then_with(|| left.minute_gap.cmp(&right.minute_gap))
        .then_with(|| bank_candidate_counts[left.bank_idx].cmp(&bank_candidate_counts[right.bank_idx]))
        .then_with(|| left.bank_idx.cmp(&right.bank_idx))
}

fn compare_gl_order(
    left_idx: usize,
    right_idx: usize,
    gl_candidates: &[Vec<Candidate>],
    gl_candidate_counts: &[usize],
    bank_candidate_counts: &[usize],
) -> Ordering {
    let left_candidates = &gl_candidates[left_idx];
    let right_candidates = &gl_candidates[right_idx];
    let left_best = left_candidates.first().copied();
    let right_best = right_candidates.first().copied();
    let left_margin = score_margin(left_candidates);
    let right_margin = score_margin(right_candidates);
    let left_bank_pressure = left_best
        .map(|candidate| bank_candidate_counts[candidate.bank_idx])
        .unwrap_or(usize::MAX);
    let right_bank_pressure = right_best
        .map(|candidate| bank_candidate_counts[candidate.bank_idx])
        .unwrap_or(usize::MAX);

    gl_candidate_counts[left_idx]
        .cmp(&gl_candidate_counts[right_idx])
        .then_with(|| best_score(right_best).cmp(&best_score(left_best)))
        .then_with(|| right_margin.cmp(&left_margin))
        .then_with(|| best_gap(left_best).cmp(&best_gap(right_best)))
        .then_with(|| left_bank_pressure.cmp(&right_bank_pressure))
        .then_with(|| left_idx.cmp(&right_idx))
}

fn bank_prefers(
    challenger_gl_idx: usize,
    challenger: Candidate,
    incumbent_gl_idx: usize,
    incumbent: Assignment,
    gl_candidate_counts: &[usize],
    bank_candidate_counts: &[usize],
) -> bool {
    compare_bank_edges(
        challenger_gl_idx,
        challenger,
        incumbent_gl_idx,
        incumbent,
        gl_candidate_counts,
        bank_candidate_counts,
    ) == Ordering::Less
}

fn compare_bank_edges(
    left_gl_idx: usize,
    left: Candidate,
    right_gl_idx: usize,
    right: Assignment,
    gl_candidate_counts: &[usize],
    bank_candidate_counts: &[usize],
) -> Ordering {
    right
        .score_ppm
        .cmp(&left.score_ppm)
        .then_with(|| left.minute_gap.cmp(&right.minute_gap))
        .then_with(|| gl_candidate_counts[left_gl_idx].cmp(&gl_candidate_counts[right_gl_idx]))
        .then_with(|| bank_candidate_counts[left.bank_idx].cmp(&bank_candidate_counts[right.bank_idx]))
        .then_with(|| left_gl_idx.cmp(&right_gl_idx))
}

fn best_score(candidate: Option<Candidate>) -> u32 {
    candidate.map(|candidate| candidate.score_ppm).unwrap_or(0)
}

fn best_gap(candidate: Option<Candidate>) -> i64 {
    candidate
        .map(|candidate| candidate.minute_gap)
        .unwrap_or(i64::MAX)
}

fn score_margin(candidates: &[Candidate]) -> i64 {
    if candidates.len() < 2 {
        return i64::from(candidates.first().map(|candidate| candidate.score_ppm).unwrap_or(0));
    }

    i64::from(candidates[0].score_ppm) - i64::from(candidates[1].score_ppm)
}

fn weighted_jaccard_ppm(
    left_tokens: &[u32],
    left_sum: f64,
    right_tokens: &[u32],
    right_sum: f64,
    token_weights: &[f64],
) -> u32 {
    if left_tokens.is_empty() || right_tokens.is_empty() {
        return 0;
    }

    let mut i = 0usize;
    let mut j = 0usize;
    let mut intersection = 0.0;

    while i < left_tokens.len() && j < right_tokens.len() {
        match left_tokens[i].cmp(&right_tokens[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                intersection += token_weights[left_tokens[i] as usize];
                i += 1;
                j += 1;
            }
        }
    }

    let union = left_sum + right_sum - intersection;
    if union <= 0.0 {
        return 0;
    }

    ((intersection / union) * SCORE_SCALE).round() as u32
}
fn amount_bucket_range(amount_cents: i64) -> [i64; 3] {
    [
        amount_cents.saturating_sub(AMOUNT_TOLERANCE_CENTS),
        amount_cents,
        amount_cents.saturating_add(AMOUNT_TOLERANCE_CENTS),
    ]
}

fn required_field<'a>(record: &'a csv::StringRecord, index: usize, name: &str) -> DynResult<&'a str> {
    record.get(index).ok_or_else(|| invalid_data(format!("missing {name} field at column {index}")).into())
}

fn parse_amount_cents(raw: &str) -> DynResult<i64> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return Err(invalid_data("empty amount").into());
    }

    let mut idx = 0usize;
    let mut negative = false;
    if bytes[idx] == b'-' {
        negative = true;
        idx += 1;
    } else if bytes[idx] == b'+' {
        idx += 1;
    }

    let mut whole = 0i64;
    let mut saw_digit = false;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        saw_digit = true;
        whole = whole * 10 + i64::from(bytes[idx] - b'0');
        idx += 1;
    }

    if !saw_digit {
        return Err(invalid_data(format!("invalid amount: {raw}")).into());
    }

    let mut frac = 0i64;
    let mut frac_digits = 0u32;
    let mut round_up = false;
    if idx < bytes.len() {
        if bytes[idx] != b'.' {
            return Err(invalid_data(format!("invalid amount: {raw}")).into());
        }
        idx += 1;

        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            if frac_digits < 2 {
                frac = frac * 10 + i64::from(bytes[idx] - b'0');
            } else if frac_digits == 2 {
                round_up = bytes[idx] >= b'5';
            }
            frac_digits += 1;
            idx += 1;
        }
    }

    if idx != bytes.len() {
        return Err(invalid_data(format!("invalid amount: {raw}")).into());
    }

    if frac_digits == 1 {
        frac *= 10;
    }
    if round_up {
        frac += 1;
    }
    if frac >= 100 {
        whole += 1;
        frac -= 100;
    }

    let mut cents = whole * 100 + frac;
    if negative {
        cents = -cents;
    }
    Ok(cents)
}

fn parse_datetime_key(raw: &str) -> DynResult<i64> {
    let bytes = raw.as_bytes();
    let mut idx = 0usize;

    let month = parse_component(bytes, &mut idx, b'/')?;
    let day = parse_component(bytes, &mut idx, b'/')?;
    let year = parse_component(bytes, &mut idx, b' ')?;
    let hour = parse_component(bytes, &mut idx, b':')?;
    let minute = parse_tail_component(bytes, &mut idx)?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return Err(invalid_data(format!("invalid datetime: {raw}")).into());
    }

    let full_year = if year <= 69 { 2000 + year as i32 } else { 1900 + year as i32 };
    let days = days_from_civil(full_year, month as i32, day as i32);
    Ok(days * MINUTES_PER_DAY + i64::from(hour * 60 + minute))
}

fn parse_component(bytes: &[u8], idx: &mut usize, delimiter: u8) -> DynResult<u32> {
    let start = *idx;
    let mut value = 0u32;

    while *idx < bytes.len() && bytes[*idx].is_ascii_digit() {
        value = value * 10 + u32::from(bytes[*idx] - b'0');
        *idx += 1;
    }

    if *idx == start || *idx >= bytes.len() || bytes[*idx] != delimiter {
        return Err(invalid_data("invalid datetime component").into());
    }
    *idx += 1;
    Ok(value)
}

fn parse_tail_component(bytes: &[u8], idx: &mut usize) -> DynResult<u32> {
    let start = *idx;
    let mut value = 0u32;

    while *idx < bytes.len() && bytes[*idx].is_ascii_digit() {
        value = value * 10 + u32::from(bytes[*idx] - b'0');
        *idx += 1;
    }

    if *idx != bytes.len() || *idx == start {
        return Err(invalid_data("invalid datetime component").into());
    }

    Ok(value)
}

fn days_from_civil(year: i32, month: i32, day: i32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * month_prime + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era * 146097 + doe - 719468)
}

fn canonicalize_token(token: &str) -> Option<&str> {
    let normalized = match token {
        "payments" | "payables" | "payable" => "payment",
        "deposits" => "deposit",
        "credits" => "credit",
        "debits" => "debit",
        "quickbooks" | "intuit" => "intuit",
        "systemrecorded" | "systemgenerated" | "systemcreated" => "system",
        _ => token,
    };

    keep_token(normalized).then_some(normalized)
}

fn keep_token(token: &str) -> bool {
    if token.len() <= 1 || STOPWORDS.contains(&token) {
        return false;
    }

    let has_alpha = token.bytes().any(|byte| byte.is_ascii_alphabetic());
    let has_digit = token.bytes().any(|byte| byte.is_ascii_digit());

    if has_digit && !has_alpha && token.len() > 4 {
        return false;
    }

    if has_alpha && has_digit && token.len() > 8 {
        return false;
    }

    true
}

fn abs_diff_i64(left: i64, right: i64) -> i64 {
    if left >= right {
        left - right
    } else {
        right - left
    }
}

fn percentage(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

fn cents_to_f64(cents: i64) -> f64 {
    cents as f64 / 100.0
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_amounts_to_cents() {
        assert_eq!(parse_amount_cents("123").unwrap(), 12_300);
        assert_eq!(parse_amount_cents("-45.67").unwrap(), -4_567);
        assert_eq!(parse_amount_cents("8.5").unwrap(), 850);
        assert_eq!(parse_amount_cents("8.555").unwrap(), 856);
    }

    #[test]
    fn parses_datetime_keys() {
        let first = parse_datetime_key("3/14/23 0:00").unwrap();
        let second = parse_datetime_key("3/15/23 0:00").unwrap();
        assert_eq!(second - first, MINUTES_PER_DAY);
    }

    #[test]
    fn drops_noise_tokens() {
        assert!(!keep_token("com"));
        assert!(!keep_token("004YGZFNCBKVC2M"));
        assert!(keep_token("quickbooks"));
        assert!(keep_token("mico"));
    }

    #[test]
    fn maxflow_preserves_maximum_cardinality_then_optimizes_cost() {
        let candidates = vec![
            vec![
                Candidate {
                    bank_idx: 0,
                    score_ppm: 10,
                    minute_gap: 0,
                },
                Candidate {
                    bank_idx: 1,
                    score_ppm: 1_000,
                    minute_gap: 0,
                },
            ],
            vec![Candidate {
                bank_idx: 1,
                score_ppm: 5,
                minute_gap: 0,
            }],
        ];

        let assignments = match_maxflow(&candidates, 2);
        assert_eq!(assignments.iter().filter(|entry| entry.is_some()).count(), 2);
        assert_eq!(assignments[0].unwrap().bank_idx, 0);
        assert_eq!(assignments[1].unwrap().bank_idx, 1);
    }

    #[test]
    fn alias_normalization_makes_quickbooks_and_intuit_overlap() {
        let mut interner = TokenInterner::default();
        let left = interner.tokenize("Systemrecorded deposit for QuickBooks Payments");
        let right = interner.tokenize("ACH CREDIT INTUIT 40442204 DEPOSIT MICO STRATEGIES");
        let weights = vec![1.0; interner.len()];
        let left_sum = left.len() as f64;
        let right_sum = right.len() as f64;
        let score = weighted_jaccard_ppm(&left, left_sum, &right, right_sum, &weights);
        assert!(score > 0);
    }

    #[test]
    fn heuristic_rejects_ambiguous_zero_score_candidates() {
        let candidates = vec![
            vec![
                Candidate {
                    bank_idx: 0,
                    score_ppm: 0,
                    minute_gap: 0,
                },
                Candidate {
                    bank_idx: 1,
                    score_ppm: 0,
                    minute_gap: 0,
                },
            ],
            vec![Candidate {
                bank_idx: 1,
                score_ppm: 0,
                minute_gap: 0,
            }],
        ];

        let assignments = match_heuristic(&candidates, 2);
        assert!(assignments[0].is_none());
        assert_eq!(assignments[1].unwrap().bank_idx, 1);
    }
}
