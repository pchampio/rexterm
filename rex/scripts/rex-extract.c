// Rex popup extractor: reads captured pane text on stdin; emits candidates.
// Preserves the user’s ~/.tmux-extrakto/extrakto.c extraction rules.
// Lua supplies the native popup, fzf, session buffer and paste operations.
//
// Filter behavior mirrors the Python implementation (extrakto.py): same
// word-break rules including the box-drawing/private-use unicode ranges,
// same path regex semantics with the ",):" rstrip and "[kmgKMG]/s$" /
// "^\d+/\d+$" excludes, same path-component splitting (>=2 chars).

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define MIN_LEN 5

// ---------- byte buffer ----------

typedef struct {
    char *p;
    size_t len, cap;
} buf_t;

static void buf_grow(buf_t *b, size_t need) {
    if (b->len + need + 1 <= b->cap) return;
    size_t cap = b->cap ? b->cap : 4096;
    while (cap < b->len + need + 1) cap *= 2;
    b->p = realloc(b->p, cap);
    if (!b->p) { perror("realloc"); exit(1); }
    b->cap = cap;
}

static void buf_append(buf_t *b, const char *s, size_t n) {
    buf_grow(b, n);
    memcpy(b->p + b->len, s, n);
    b->len += n;
    b->p[b->len] = 0;
}

// ---------- ordered string set with hash dedup ----------

typedef struct {
    char **items;
    size_t n_items, cap_items;
    char **table;
    size_t cap_table, n_table;
} sset_t;

static unsigned long sset_hash(const char *s, size_t n) {
    unsigned long h = 5381;
    for (size_t i = 0; i < n; i++) h = ((h << 5) + h) ^ (unsigned char)s[i];
    return h;
}

static void sset_grow(sset_t *ss) {
    size_t newcap = ss->cap_table ? ss->cap_table * 2 : 256;
    char **newt = calloc(newcap, sizeof(char *));
    if (!newt) { perror("calloc"); exit(1); }
    for (size_t i = 0; i < ss->cap_table; i++) {
        char *k = ss->table[i];
        if (!k) continue;
        size_t h = sset_hash(k, strlen(k)) & (newcap - 1);
        while (newt[h]) h = (h + 1) & (newcap - 1);
        newt[h] = k;
    }
    free(ss->table);
    ss->table = newt;
    ss->cap_table = newcap;
}

static void sset_add(sset_t *ss, const char *s, size_t n) {
    if (ss->cap_table == 0 || ss->n_table * 2 >= ss->cap_table) sset_grow(ss);
    size_t h = sset_hash(s, n) & (ss->cap_table - 1);
    while (ss->table[h]) {
        char *k = ss->table[h];
        if (strlen(k) == n && memcmp(k, s, n) == 0) return;
        h = (h + 1) & (ss->cap_table - 1);
    }
    char *copy = malloc(n + 1);
    if (!copy) { perror("malloc"); exit(1); }
    memcpy(copy, s, n);
    copy[n] = 0;
    ss->table[h] = copy;
    ss->n_table++;
    if (ss->n_items == ss->cap_items) {
        size_t cap = ss->cap_items ? ss->cap_items * 2 : 64;
        ss->items = realloc(ss->items, cap * sizeof(char *));
        if (!ss->items) { perror("realloc"); exit(1); }
        ss->cap_items = cap;
    }
    ss->items[ss->n_items++] = copy;
}

// ---------- UTF-8 ----------

static void utf8_next(const char *s, size_t len, size_t *i, uint32_t *cp) {
    unsigned char b0 = (unsigned char)s[*i];
    if (b0 < 0x80) { *cp = b0; *i += 1; return; }
    if (b0 < 0xC0) { *cp = b0; *i += 1; return; } // stray continuation
    int extra;
    if (b0 < 0xE0) { *cp = b0 & 0x1F; extra = 1; }
    else if (b0 < 0xF0) { *cp = b0 & 0x0F; extra = 2; }
    else if (b0 < 0xF8) { *cp = b0 & 0x07; extra = 3; }
    else { *cp = b0; *i += 1; return; }
    if (*i + 1 + extra > len) { *cp = b0; *i += 1; return; }
    uint32_t v = *cp;
    for (int k = 1; k <= extra; k++) {
        unsigned char b = (unsigned char)s[*i + k];
        if ((b & 0xC0) != 0x80) { *cp = b0; *i += 1; return; }
        v = (v << 6) | (b & 0x3F);
    }
    *cp = v;
    *i += 1 + extra;
}

static int is_word_break(uint32_t cp) {
    if (cp <= 0x7F) {
        return cp == ' ' || cp == '\t' || cp == '\n' || cp == '\r' ||
               cp == '[' || cp == ']' || cp == '(' || cp == ')' ||
               cp == '{' || cp == '}' || cp == '=' || cp == '$' ||
               // additionally split on these so identifiers inside URLs/paths/
               // emails (lychee in lychee.git, name in user@host/name)
               // come out as their own searchable entries
               cp == ':' || cp == '/' || cp == '@' || cp == '.';
    }
    if (cp >= 0x2500 && cp <= 0x27BF) return 1;
    if (cp >= 0xE000 && cp <= 0xF8FF) return 1;
    if (cp == 0x22C5 || cp == 0x21B4) return 1;
    return 0;
}

// ---------- filters ----------

static int contains_byte(const char *set, char c) {
    for (; *set; set++) if (*set == c) return 1;
    return 0;
}

static const char *WORD_LSTRIP = ",:;()[]{}<>'\"|";
static const char *WORD_RSTRIP = ",:;()[]{}<>'\"|.";

static void emit_word(sset_t *out, const char *s, size_t n) {
    while (n > 0 && contains_byte(WORD_LSTRIP, s[0])) { s++; n--; }
    while (n > 0 && contains_byte(WORD_RSTRIP, s[n - 1])) n--;
    if (n >= MIN_LEN) sset_add(out, s, n);
}

static void extract_words(const char *text, size_t len, sset_t *out) {
    size_t i = 0;
    while (i < len) {
        size_t pos = i;
        uint32_t cp;
        utf8_next(text, len, &i, &cp);
        if (is_word_break(cp)) continue;
        size_t start = pos;
        size_t end = i;
        while (i < len) {
            size_t pos2 = i;
            uint32_t c2;
            utf8_next(text, len, &i, &c2);
            if (is_word_break(c2)) { end = pos2; break; }
            end = i;
        }
        emit_word(out, text + start, end - start);
    }
}

static int is_path_pre(char c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '"' || c == '(' ||
           c == '[' || c == '<' || c == '\'' || c == ':';
}

// path-first chars: chars allowed in the first segment of a path
// matches the Python class [-~a-zA-Z0-9_+,.]
static int is_path_first(char c) {
    if (c >= 'a' && c <= 'z') return 1;
    if (c >= 'A' && c <= 'Z') return 1;
    if (c >= '0' && c <= '9') return 1;
    return c == '-' || c == '_' || c == '+' || c == ',' || c == '.' || c == '~';
}

// path-rest chars: anything that isn't a terminator
// matches the Python negated class [^ \t\n\r|:"'$%&)>\]]
static int is_path_rest(char c) {
    if (c == ' ' || c == '\t' || c == '\n' || c == '\r') return 0;
    if (c == '|' || c == ':' || c == '"' || c == '\'' || c == '$') return 0;
    if (c == '%' || c == '&' || c == ')' || c == '>' || c == ']') return 0;
    return 1;
}

static int is_path_excluded(const char *s, size_t n) {
    if (n >= 3 && s[n - 1] == 's' && s[n - 2] == '/' &&
        (s[n - 3] == 'k' || s[n - 3] == 'm' || s[n - 3] == 'g' ||
         s[n - 3] == 'K' || s[n - 3] == 'M' || s[n - 3] == 'G')) return 1;
    int slashes = 0, all_digits = 1;
    for (size_t i = 0; i < n; i++) {
        if (s[i] == '/') slashes++;
        else if (s[i] < '0' || s[i] > '9') { all_digits = 0; break; }
    }
    return all_digits && slashes == 1;
}

// emulates the Python regex `(~|/)?[-~a-zA-Z0-9_+,.]+/[^...]*` with backtracking.
// Returns 1 and sets *out_end if a match starts at i; 0 otherwise.
static int try_path_match(const char *text, size_t len, size_t i, size_t *out_end) {
    size_t starts[2];
    int ns = 0;
    if (i < len && (text[i] == '~' || text[i] == '/')) starts[ns++] = i + 1;
    starts[ns++] = i;
    for (int s = 0; s < ns; s++) {
        size_t k = starts[s];
        size_t seg_start = k;
        while (k < len && is_path_first(text[k])) k++;
        if (k > seg_start && k < len && text[k] == '/') {
            size_t j = k + 1;
            while (j < len && is_path_rest(text[j])) j++;
            *out_end = j;
            return 1;
        }
    }
    return 0;
}

static void extract_paths(const char *text, size_t len, sset_t *out) {
    size_t i = 0;
    while (i < len) {
        if (i > 0 && !is_path_pre(text[i - 1])) { i++; continue; }
        size_t end;
        if (!try_path_match(text, len, i, &end)) { i++; continue; }
        size_t plen = end - i;
        while (plen > 0) {
            char c = text[i + plen - 1];
            if (c != ',' && c != ')' && c != ':') break;
            plen--;
        }
        if (plen >= MIN_LEN && !is_path_excluded(text + i, plen)) {
            sset_add(out, text + i, plen);
            // hardcoded path splitting: emit each component (>=2 chars, != original)
            size_t seg_start = 0;
            for (size_t k = 0; k <= plen; k++) {
                if (k == plen || text[i + k] == '/') {
                    size_t seg_len = k - seg_start;
                    if (seg_len >= 2 && seg_len != plen) {
                        sset_add(out, text + i + seg_start, seg_len);
                    }
                    seg_start = k + 1;
                }
            }
        }
        i = end ? end : i + 1;
    }
}

static const char *URL_PREFIXES[] = {
    "https://", "http://", "git@", "git://", "ssh://",
    "sftp://", "ftp://", "file:///", NULL,
};

static int is_url_char(char c) {
    if (c >= 'a' && c <= 'z') return 1;
    if (c >= 'A' && c <= 'Z') return 1;
    if (c >= '0' && c <= '9') return 1;
    return strchr("?=%/_.:,;~@!#$&()*+-", c) != NULL;
}

static void extract_urls(const char *text, size_t len, sset_t *out) {
    size_t i = 0;
    while (i < len) {
        size_t prefix = 0;
        for (int p = 0; URL_PREFIXES[p]; p++) {
            size_t pl = strlen(URL_PREFIXES[p]);
            if (i + pl <= len && memcmp(text + i, URL_PREFIXES[p], pl) == 0) {
                prefix = pl;
                break;
            }
        }
        if (prefix == 0) { i++; continue; }
        size_t j = i + prefix;
        while (j < len && is_url_char(text[j])) j++;
        size_t ulen = j - i;
        while (ulen > 0) {
            char c = text[i + ulen - 1];
            if (c != ',' && c != ')' && c != ':') break;
            ulen--;
        }
        if (ulen >= MIN_LEN) {
            sset_add(out, text + i, ulen);
            // also emit the host portion after :// (matches Python url alt2)
            const char *u = text + i;
            for (size_t k = 0; k + 2 < ulen; k++) {
                if (u[k] == ':' && u[k + 1] == '/' && u[k + 2] == '/') {
                    size_t h = k + 3, he = h;
                    while (he < ulen && u[he] != '/' && u[he] != '?' && u[he] != ' ') he++;
                    if (he > h) sset_add(out, u + h, he - h);
                    break;
                }
            }
        }
        i = j;
    }
}

// ---------- stdin ----------

static int read_all(int fd, buf_t *out) {
    char chunk[8192];
    for (;;) {
        ssize_t n = read(fd, chunk, sizeof(chunk));
        if (n < 0) { if (errno == EINTR) continue; return -1; }
        if (n == 0) return 0;
        buf_append(out, chunk, (size_t)n);
    }
}

int main(int argc, char **argv) {
    if (argc != 2 || strcmp(argv[1], "--dump") != 0) {
        fprintf(stderr, "usage: rex-extract --dump < captured-pane.txt\n");
        return 2;
    }
    buf_t cap = {0};
    if (read_all(0, &cap) < 0) { perror("read"); return 1; }
    sset_t set = {0};
    extract_paths(cap.p ? cap.p : "", cap.len, &set);
    extract_urls(cap.p ? cap.p : "", cap.len, &set);
    extract_words(cap.p ? cap.p : "", cap.len, &set);
    for (size_t k = 0; k < set.n_items; k++) puts(set.items[k]);
    return ferror(stdout) ? 1 : 0;
}
