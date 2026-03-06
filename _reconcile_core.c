/*
 * _reconcile_core.c — C shared library for hot-path operations.
 *   1. Batch edit-distance similarity (normalized Levenshtein)
 *   2. Fast date-to-epoch (avoids mktime)
 *
 * Build:
 *   macOS:  cc -O3 -shared -fPIC -o _reconcile_core.dylib _reconcile_core.c
 *   Linux:  cc -O3 -shared -fPIC -o _reconcile_core.so    _reconcile_core.c
 */

#include <stdlib.h>
#include <string.h>

/* ── Fast epoch from (year, month, day, hour, minute) ──────────
 * Returns seconds since 1970-01-01 UTC.  No timezone/DST.
 * Matches mktime(…, tm_isdst=-1) when local == UTC, but is
 * consistent across all entries regardless — we only need diffs.
 */
static const int DAYS_BEFORE_MONTH[12] = {
    0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334
};

long long fast_epoch(int year, int month, int day, int hour, int minute) {
    /* Leap year count from 1970..year-1 */
    int y = year - 1;
    long long days = 365LL * (year - 1970)
                   + (y/4 - 1970/4)     /* +leap years with /4 rule  */
                   - (y/100 - 1970/100)  /* -century non-leaps        */
                   + (y/400 - 1970/400); /* +400-year leaps           */

    days += DAYS_BEFORE_MONTH[month - 1];
    if (month > 2 && (year%4 == 0 && (year%100 != 0 || year%400 == 0)))
        days += 1;
    days += day - 1;

    return days * 86400LL + hour * 3600LL + minute * 60LL;
}

/* ── Batch normalized edit-distance similarity ─────────────────
 *
 * For each pair (a[i], b[i]), computes:
 *   score = 1 - edit_distance(a, b) / max(|a|, |b|)
 *
 * Strings are passed as a flat buffer + offset/length arrays.
 * Single DP row, reused across calls.
 */
void batch_similarity(
    const char *a_buf, const int *a_off, const int *a_len,
    const char *b_buf, const int *b_off, const int *b_len,
    double *scores, int n)
{
    /* Find max string length for DP allocation */
    int max_short = 0;
    for (int k = 0; k < n; k++) {
        int la = a_len[k], lb = b_len[k];
        int shorter = la < lb ? la : lb;
        if (shorter > max_short) max_short = shorter;
    }
    int *dp = (int *)malloc((max_short + 1) * sizeof(int));

    for (int k = 0; k < n; k++) {
        const char *a = a_buf + a_off[k];
        const char *b = b_buf + b_off[k];
        int la = a_len[k], lb = b_len[k];

        if (la == 0 && lb == 0) { scores[k] = 1.0; continue; }
        if (la == 0 || lb == 0) { scores[k] = 0.0; continue; }

        /* Ensure la >= lb (shorter string as DP columns) */
        if (la < lb) {
            const char *tmp = a; a = b; b = tmp;
            int t = la; la = lb; lb = t;
        }

        for (int j = 0; j <= lb; j++) dp[j] = j;

        for (int i = 1; i <= la; i++) {
            int prev = dp[0];
            dp[0] = i;
            const char ai = a[i - 1];
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
                prev = tmp;
            }
        }

        int mx = la; /* la >= lb after swap */
        scores[k] = 1.0 - (double)dp[lb] / mx;
    }

    free(dp);
}
