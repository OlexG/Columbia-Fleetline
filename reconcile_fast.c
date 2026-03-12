/*
 * reconcile_fast.c — Pure C reconciliation, maximally optimized.
 *
 *   - mmap for file I/O (zero-copy)
 *   - Custom epoch (no mktime)
 *   - Arena allocator (no malloc per string)
 *   - Buffered JSON writer (single write, no fprintf per field)
 *   - Edit distance with early-exit pruning
 *
 * Build:  cc -O3 -o reconcile_fast reconcile_fast.c
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <ctype.h>

#define MAX_BANK   4096
#define MAX_JE     4096
#define MAX_CAND   32768
#define ARENA_SIZE (8 * 1024 * 1024)

/* ── Arena allocator ──────────────────────────────────────────── */

static char arena[ARENA_SIZE];
static int arena_pos = 0;

static inline char *arena_alloc(int n) {
    char *p = arena + arena_pos;
    arena_pos += n;
    return p;
}

static inline char *arena_strdup(const char *s, int len) {
    char *p = arena_alloc(len + 1);
    memcpy(p, s, len);
    p[len] = '\0';
    return p;
}

/* ── Fast epoch ───────────────────────────────────────────────── */

static const int DAYS_BEFORE[12] = {0,31,59,90,120,151,181,212,243,273,304,334};

static inline long long fast_epoch(int year, int month, int day, int hour, int minute) {
    int y = year - 1;
    long long days = 365LL * (year - 1970)
                   + (y/4 - 1970/4) - (y/100 - 1970/100) + (y/400 - 1970/400);
    days += DAYS_BEFORE[month - 1];
    if (month > 2 && (year%4 == 0 && (year%100 != 0 || year%400 == 0)))
        days++;
    days += day - 1;
    return days * 86400LL + hour * 3600LL + minute * 60LL;
}

/* ── Data structures ──────────────────────────────────────────── */

typedef struct {
    int index;
    long long dt;
    double amount;
    char *desc;
    char *dt_str;
    char *norm;
    int norm_len;
} BankTxn;

typedef struct {
    char *id;
    long long dt;
    double amount;
    char *desc;
    char *dt_str;
    char *norm;
    int norm_len;
    int n_lines;
} JournalEntry;

typedef struct {
    int je_idx, bank_idx;
    double score;
    int date_diff;
} Candidate;

typedef struct {
    int je_idx, bank_idx;
    double score;
} MatchResult;

static BankTxn bank_txns[MAX_BANK];
static int n_bank = 0;

static JournalEntry entries[MAX_JE];
static int n_je = 0;

static Candidate cands[MAX_CAND];
static int n_cands = 0;

static MatchResult results[MAX_JE];
static int n_results = 0;

/* ── Parsing helpers ──────────────────────────────────────────── */

static inline long long parse_dt(const char *s) {
    /* Hand-rolled integer parse — faster than strtol for tiny numbers */
    int mo = 0, d = 0, y = 0, h = 0, mi = 0;
    const char *p = s;
    while (*p >= '0' && *p <= '9') mo = mo*10 + (*p++ - '0'); p++; /* '/' */
    while (*p >= '0' && *p <= '9') d  = d*10  + (*p++ - '0'); p++; /* '/' */
    while (*p >= '0' && *p <= '9') y  = y*10  + (*p++ - '0');
    while (*p == ' ') p++;
    while (*p >= '0' && *p <= '9') h  = h*10  + (*p++ - '0'); p++; /* ':' */
    while (*p >= '0' && *p <= '9') mi = mi*10 + (*p++ - '0');
    if (y < 100) y += 2000;
    return fast_epoch(y, mo, d, h, mi);
}

static inline double parse_amount(const char *s) {
    /* Fast atof without copying — skip commas, $, spaces */
    double r = 0, frac = 0, div = 1;
    int neg = 0, after_dot = 0;
    for (; *s; s++) {
        if (*s == '-') { neg = 1; continue; }
        if (*s == ',' || *s == ' ' || *s == '$') continue;
        if (*s == '.') { after_dot = 1; continue; }
        if (*s >= '0' && *s <= '9') {
            if (after_dot) { frac = frac*10 + (*s - '0'); div *= 10; }
            else r = r*10 + (*s - '0');
        }
    }
    r += frac / div;
    return neg ? -r : r;
}

static inline int trim(const char *s, int len, const char **out) {
    while (len > 0 && ((unsigned char)s[0] <= ' ')) { s++; len--; }
    while (len > 0 && ((unsigned char)s[len-1] <= ' ')) len--;
    *out = s;
    return len;
}

static char *normalize(const char *s, int slen, int *out_len) {
    char *buf = arena_alloc(slen + 1);
    int n = 0, last_sp = 1;
    for (int i = 0; i < slen; i++) {
        unsigned char c = s[i];
        if (c <= ' ') {
            if (!last_sp && n > 0) { buf[n++] = ' '; last_sp = 1; }
        } else {
            buf[n++] = tolower(c);
            last_sp = 0;
        }
    }
    if (n > 0 && buf[n-1] == ' ') n--;
    buf[n] = '\0';
    *out_len = n;
    return buf;
}

/* ── CSV field extraction ─────────────────────────────────────── */

static inline int csv_field(const char *line, int linelen, int *pos, char *buf) {
    int p = *pos, n = 0;
    if (p < linelen && line[p] == '"') {
        p++;
        while (p < linelen) {
            if (line[p] == '"') {
                if (p+1 < linelen && line[p+1] == '"') { buf[n++] = '"'; p += 2; }
                else { p++; break; }
            } else buf[n++] = line[p++];
        }
        if (p < linelen && line[p] == ',') p++;
    } else {
        while (p < linelen && line[p] != ',' && line[p] != '\r' && line[p] != '\n')
            buf[n++] = line[p++];
        if (p < linelen && line[p] == ',') p++;
    }
    buf[n] = '\0';
    *pos = p;
    return n;
}

/* ── mmap ─────────────────────────────────────────────────────── */

typedef struct { const char *data; size_t size; } MMap;

static MMap mmap_file(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) { perror(path); exit(1); }
    struct stat st;
    fstat(fd, &st);
    const char *data = mmap(NULL, st.st_size, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    return (MMap){data, st.st_size};
}

static int next_line(const char *data, size_t size, size_t *offset,
                     const char **line_start) {
    if (*offset >= size) return -1;
    *line_start = data + *offset;
    const char *p = *line_start, *end = data + size;
    int in_q = 0;
    while (p < end) {
        if (*p == '"') in_q = !in_q;
        else if (*p == '\n' && !in_q) { p++; break; }
        p++;
    }
    int len = (int)(p - *line_start);
    while (len > 0 && ((*line_start)[len-1]=='\n' || (*line_start)[len-1]=='\r')) len--;
    *offset = p - data;
    return len;
}

/* ── Hash map ─────────────────────────────────────────────────── */

#define HM_SIZE 8192
#define HM_MASK (HM_SIZE - 1)

typedef struct { char *key; int val; } HMEntry;
static HMEntry hm[HM_SIZE];

static inline unsigned int hash_str(const char *s) {
    unsigned int h = 5381;
    while (*s) h = h * 33 + (unsigned char)*s++;
    return h;
}

static inline int hm_get(const char *key) {
    unsigned int h = hash_str(key) & HM_MASK;
    while (hm[h].key) {
        if (strcmp(hm[h].key, key) == 0) return hm[h].val;
        h = (h + 1) & HM_MASK;
    }
    return -1;
}

static inline void hm_put(const char *key, int val) {
    unsigned int h = hash_str(key) & HM_MASK;
    while (hm[h].key) {
        if (strcmp(hm[h].key, key) == 0) { hm[h].val = val; return; }
        h = (h + 1) & HM_MASK;
    }
    hm[h].key = (char *)key;
    hm[h].val = val;
}

/* ── Edit distance with early-exit threshold ──────────────────── */
/*
 * Returns normalized similarity in [0, 1].
 * Uses a row-pruning optimisation: if the minimum value in the
 * current DP row already exceeds a threshold that can't beat the
 * best candidate seen so far, bail out early with score 0.
 */

static int dp_buf[4096];

static double ed_similarity(const char *a, int la, const char *b, int lb) {
    if (la == 0 && lb == 0) return 1.0;
    if (la == 0 || lb == 0) return 0.0;

    /* Fast path: identical strings → score 1.0 (common case) */
    if (la == lb && memcmp(a, b, la) == 0) return 1.0;

    if (la < lb) {
        const char *t = a; a = b; b = t;
        int tt = la; la = lb; lb = tt;
    }

    /* Length diff alone gives minimum possible ED */
    /* If min_ed / max_len >= 1.0, score is 0 */
    if (la - lb >= la) return 0.0;  /* only when lb==0, handled above */

    int *dp = dp_buf;
    for (int j = 0; j <= lb; j++) dp[j] = j;

    for (int i = 1; i <= la; i++) {
        int prev = dp[0];
        dp[0] = i;
        const char ai = a[i - 1];
        int row_min = i; /* track minimum in row for early exit */
        for (int j = 1; j <= lb; j++) {
            int tmp = dp[j];
            if (ai == b[j - 1]) {
                dp[j] = prev;
            } else {
                int v = prev;
                if (tmp < v) v = tmp;
                if (dp[j - 1] < v) v = dp[j - 1];
                dp[j] = v + 1;
            }
            if (dp[j] < row_min) row_min = dp[j];
            prev = tmp;
        }
        /* If every cell in this row >= la, edit dist >= la → score = 0 */
        if (row_min >= la) return 0.0;
    }
    return 1.0 - (double)dp[lb] / la;
}

/* ── Sorting ──────────────────────────────────────────────────── */

static int cmp_bank_dt(const void *a, const void *b) {
    long long da = bank_txns[*(const int*)a].dt, db = bank_txns[*(const int*)b].dt;
    return (da > db) - (da < db);
}

static int cmp_cand(const void *a, const void *b) {
    const Candidate *ca = a, *cb = b;
    if (ca->score != cb->score) return ca->score > cb->score ? -1 : 1;
    if (ca->date_diff != cb->date_diff) return ca->date_diff - cb->date_diff;
    if (ca->bank_idx != cb->bank_idx) return ca->bank_idx - cb->bank_idx;
    return ca->je_idx - cb->je_idx;
}

static int cmp_results(const void *a, const void *b) {
    return strcmp(entries[((const MatchResult*)a)->je_idx].id,
                 entries[((const MatchResult*)b)->je_idx].id);
}

/* ── Binary search ────────────────────────────────────────────── */

static inline int lower_bound_dt(const long long *arr, int n, long long val) {
    int lo = 0, hi = n;
    while (lo < hi) { int mid = (lo+hi)>>1; if (arr[mid] < val) lo = mid+1; else hi = mid; }
    return lo;
}
static inline int upper_bound_dt(const long long *arr, int n, long long val) {
    int lo = 0, hi = n;
    while (lo < hi) { int mid = (lo+hi)>>1; if (arr[mid] <= val) lo = mid+1; else hi = mid; }
    return lo;
}

/* ── Buffered JSON writer ─────────────────────────────────────── */
/* Build the entire JSON in a growable buffer, then single write().
 * Avoids thousands of fprintf/fputc calls and stdio locking.       */

static char *jbuf;
static int jbuf_len, jbuf_cap;

static inline void jb_init(void) {
    jbuf_cap = 2 * 1024 * 1024;
    jbuf = malloc(jbuf_cap);
    jbuf_len = 0;
}

static inline void jb_ensure(int need) {
    while (jbuf_len + need >= jbuf_cap) {
        jbuf_cap *= 2;
        jbuf = realloc(jbuf, jbuf_cap);
    }
}

static inline void jb_str(const char *s, int n) {
    jb_ensure(n);
    memcpy(jbuf + jbuf_len, s, n);
    jbuf_len += n;
}

static inline void jb_lit(const char *s) {
    jb_str(s, strlen(s));
}

static inline void jb_char(char c) {
    jb_ensure(1);
    jbuf[jbuf_len++] = c;
}

/* Write JSON-escaped string */
static void jb_escaped(const char *s) {
    jb_ensure(strlen(s) * 2 + 2);
    char *p = jbuf + jbuf_len;
    *p++ = '"';
    for (; *s; s++) {
        switch (*s) {
            case '"':  *p++ = '\\'; *p++ = '"';  break;
            case '\\': *p++ = '\\'; *p++ = '\\'; break;
            case '\n': *p++ = '\\'; *p++ = 'n';  break;
            case '\r': *p++ = '\\'; *p++ = 'r';  break;
            case '\t': *p++ = '\\'; *p++ = 't';  break;
            default:   *p++ = *s;
        }
    }
    *p++ = '"';
    jbuf_len = p - jbuf;
}

/* Write a double with 4 decimal places — hand-rolled, no snprintf */
static void jb_double(double v) {
    jb_ensure(32);
    char *p = jbuf + jbuf_len;
    if (v < 0) { *p++ = '-'; v = -v; }
    long long integer = (long long)v;
    int frac = (int)((v - integer) * 10000 + 0.5);
    if (frac >= 10000) { integer++; frac -= 10000; }
    /* Write integer part */
    char ibuf[20];
    int ilen = 0;
    if (integer == 0) { ibuf[ilen++] = '0'; }
    else { long long tmp = integer; while (tmp) { ibuf[ilen++] = '0' + (tmp % 10); tmp /= 10; } }
    for (int i = ilen - 1; i >= 0; i--) *p++ = ibuf[i];
    /* Write fractional part */
    *p++ = '.';
    p[3] = '0' + (frac % 10); frac /= 10;
    p[2] = '0' + (frac % 10); frac /= 10;
    p[1] = '0' + (frac % 10); frac /= 10;
    p[0] = '0' + frac;
    p += 4;
    jbuf_len = p - jbuf;
}

/* Write an integer — hand-rolled */
static void jb_int(int v) {
    jb_ensure(16);
    char *p = jbuf + jbuf_len;
    if (v < 0) { *p++ = '-'; v = -v; }
    char ibuf[12];
    int ilen = 0;
    if (v == 0) { ibuf[ilen++] = '0'; }
    else { while (v) { ibuf[ilen++] = '0' + (v % 10); v /= 10; } }
    for (int i = ilen - 1; i >= 0; i--) *p++ = ibuf[i];
    jbuf_len = p - jbuf;
}

static void jb_flush(const char *path) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    write(fd, jbuf, jbuf_len);
    close(fd);
    free(jbuf);
}

/* ── Main ─────────────────────────────────────────────────────── */

int main(void) {
    const long long DATE_WINDOW = 15LL * 86400;
    const double AMOUNT_TOL = 0.1;
    char field_buf[4096];

    memset(hm, 0, sizeof(hm));

    /* ── 1. Parse bank_transactions.csv ───────────────────────── */
    {
        MMap mm = mmap_file("bank_transactions.csv");
        size_t off = 0;
        const char *line;
        next_line(mm.data, mm.size, &off, &line);

        int linelen;
        while ((linelen = next_line(mm.data, mm.size, &off, &line)) >= 0) {
            if (linelen == 0) continue;
            int pos = 0, flen;
            BankTxn *t = &bank_txns[n_bank];
            t->index = n_bank;

            flen = csv_field(line, linelen, &pos, field_buf);
            const char *ts; int tl = trim(field_buf, flen, &ts);
            t->dt_str = arena_strdup(ts, tl);
            t->dt = parse_dt(t->dt_str);

            flen = csv_field(line, linelen, &pos, field_buf);
            t->amount = parse_amount(field_buf);

            flen = csv_field(line, linelen, &pos, field_buf);
            const char *ds; int dl = trim(field_buf, flen, &ds);
            t->desc = arena_strdup(ds, dl);
            t->norm = normalize(ds, dl, &t->norm_len);

            n_bank++;
        }
        munmap((void*)mm.data, mm.size);
    }

    /* ── 2. Parse & aggregate general_ledger.csv ──────────────── */
    {
        MMap mm = mmap_file("general_ledger.csv");
        size_t off = 0;
        const char *line;
        next_line(mm.data, mm.size, &off, &line);

        int linelen;
        while ((linelen = next_line(mm.data, mm.size, &off, &line)) >= 0) {
            if (linelen == 0) continue;
            int pos = 0, flen;

            flen = csv_field(line, linelen, &pos, field_buf);
            const char *ts; int tl = trim(field_buf, flen, &ts);
            char *dt_str = arena_strdup(ts, tl);
            long long dt = parse_dt(dt_str);

            flen = csv_field(line, linelen, &pos, field_buf);
            double amt = parse_amount(field_buf);

            /* description — save before next field overwrites field_buf */
            flen = csv_field(line, linelen, &pos, field_buf);
            const char *ds; int dl = trim(field_buf, flen, &ds);
            char *desc_copy = arena_strdup(ds, dl);

            flen = csv_field(line, linelen, &pos, field_buf);
            const char *js; int jl = trim(field_buf, flen, &js);
            char *je_id = arena_strdup(js, jl);

            int idx = hm_get(je_id);
            if (idx < 0) {
                idx = n_je;
                hm_put(je_id, idx);
                JournalEntry *e = &entries[idx];
                e->id = je_id;
                e->dt = dt;
                e->dt_str = dt_str;
                e->amount = amt;
                e->n_lines = 1;
                e->desc = desc_copy;
                n_je++;
            } else {
                JournalEntry *e = &entries[idx];
                e->amount += amt;
                e->n_lines++;
                if (dl > 0) {
                    int old_len = (int)strlen(e->desc);
                    int new_len = old_len + 1 + dl;
                    char *nd = arena_alloc(new_len + 1);
                    if (old_len > 0) {
                        memcpy(nd, e->desc, old_len);
                        nd[old_len] = ' ';
                        memcpy(nd + old_len + 1, desc_copy, dl);
                        nd[new_len] = '\0';
                    } else {
                        memcpy(nd, desc_copy, dl);
                        nd[dl] = '\0';
                    }
                    e->desc = nd;
                }
            }
        }
        munmap((void*)mm.data, mm.size);
    }

    for (int i = 0; i < n_je; i++) {
        int dlen = (int)strlen(entries[i].desc);
        entries[i].norm = normalize(entries[i].desc, dlen, &entries[i].norm_len);
    }

    /* ── 3. Sort bank by datetime ─────────────────────────────── */
    int *b_order = malloc(n_bank * sizeof(int));
    for (int i = 0; i < n_bank; i++) b_order[i] = i;
    qsort(b_order, n_bank, sizeof(int), cmp_bank_dt);

    long long *b_dts = malloc(n_bank * sizeof(long long));
    for (int i = 0; i < n_bank; i++)
        b_dts[i] = bank_txns[b_order[i]].dt;

    /* ── 4. Build candidates + score ──────────────────────────── */
    for (int ji = 0; ji < n_je; ji++) {
        JournalEntry *je = &entries[ji];
        int lo = lower_bound_dt(b_dts, n_bank, je->dt - DATE_WINDOW);
        int hi = upper_bound_dt(b_dts, n_bank, je->dt + DATE_WINDOW);

        for (int si = lo; si < hi; si++) {
            int bi = b_order[si];
            if (fabs(bank_txns[bi].amount - je->amount) > AMOUNT_TOL) continue;

            /* Skip ED entirely if either description is empty → score 0 */
            double sc;
            if (bank_txns[bi].norm_len == 0 || je->norm_len == 0)
                sc = (bank_txns[bi].norm_len == 0 && je->norm_len == 0) ? 1.0 : 0.0;
            else
                sc = ed_similarity(bank_txns[bi].norm, bank_txns[bi].norm_len,
                                   je->norm, je->norm_len);
            long long dd = bank_txns[bi].dt - je->dt;
            if (dd < 0) dd = -dd;
            cands[n_cands++] = (Candidate){ji, bi, sc, (int)(dd / 86400)};
        }
    }

    /* silent — no stderr logging */

    /* ── 5. Greedy one-to-one matching ────────────────────────── */
    qsort(cands, n_cands, sizeof(Candidate), cmp_cand);

    char *je_used = calloc(n_je, 1);
    char *bk_used = calloc(n_bank, 1);

    for (int i = 0; i < n_cands; i++) {
        int ji = cands[i].je_idx, bi = cands[i].bank_idx;
        if (je_used[ji] || bk_used[bi]) continue;
        je_used[ji] = bk_used[bi] = 1;
        results[n_results++] = (MatchResult){ji, bi, cands[i].score};
    }

    qsort(results, n_results, sizeof(MatchResult), cmp_results);

    /* ── 6. Write matches.json (buffered, single write) ───────── */
    jb_init();
    jb_lit("[\n");
    for (int i = 0; i < n_results; i++) {
        JournalEntry *je = &entries[results[i].je_idx];
        BankTxn *bt = &bank_txns[results[i].bank_idx];

        jb_lit("  {\n    \"journal_entry_id\": "); jb_escaped(je->id);
        jb_lit(",\n    \"gl_datetime\": ");        jb_escaped(je->dt_str);
        jb_lit(",\n    \"gl_amount\": ");           jb_double(je->amount);
        jb_lit(",\n    \"gl_description\": ");      jb_escaped(je->desc);
        jb_lit(",\n    \"bank_index\": ");           jb_int(bt->index);
        jb_lit(",\n    \"bank_datetime\": ");       jb_escaped(bt->dt_str);
        jb_lit(",\n    \"bank_amount\": ");          jb_double(bt->amount);
        jb_lit(",\n    \"bank_description\": ");    jb_escaped(bt->desc);
        jb_lit(",\n    \"score\": ");                jb_double(results[i].score);
        jb_lit("\n  }");
        if (i + 1 < n_results) jb_char(',');
        jb_char('\n');
    }
    jb_lit("]\n");
    jb_flush("matches.json");

    /* silent — output is matches.json only */

    free(b_order); free(b_dts); free(je_used); free(bk_used);
    return 0;
}
