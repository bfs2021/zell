/* std_argon2.c - BLAKE2b (RFC 7693) and Argon2d/Argon2i/Argon2id
 * (RFC 9106, version 0x13), the memory-hard function behind
 * crypto.kdf.Argon2id and crypto.password (runtime/ABI.md 23).
 *
 * The lanes of a slice are independent (a block refers to other lanes only
 * in finished slices), so each slice is filled by up to min(p, usable CPUs)
 * threads, the calling thread included, which then wait for each other;
 * the result depends only on the `threads` parameter p, not on how many
 * threads ran. Argon2i and the first half of the first pass of
 * Argon2id use data-independent addressing; Argon2d and the rest of
 * Argon2id index memory by secret-dependent data, as the algorithm
 * specifies. */
#include "rt_internal.h"
#include "crypto/crypto.h"
#include "platform/platform.h"

#include <stdlib.h>
#include <string.h>

/* ---- BLAKE2b ---- */

typedef struct {
    uint64_t h[8], t[2];
    uint8_t buf[128];
    size_t n, outlen;
} b2b;

static const uint64_t B2_IV[8] = {
    0x6a09e667f3bcc908ULL, 0xbb67ae8584caa73bULL, 0x3c6ef372fe94f82bULL, 0xa54ff53a5f1d36f1ULL,
    0x510e527fade682d1ULL, 0x9b05688c2b3e6c1fULL, 0x1f83d9abfb41bd6bULL, 0x5be0cd19137e2179ULL,
};
static const uint8_t B2_SIGMA[12][16] = {
    { 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15 }, { 14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3 },
    { 11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4 }, { 7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8 },
    { 9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13 }, { 2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9 },
    { 12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11 }, { 13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10 },
    { 6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5 }, { 10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0 },
    { 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15 }, { 14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3 },
};

static inline uint64_t rotr64(uint64_t x, int r) { return (x >> r) | (x << (64 - r)); }

#define B2G(a, b, c, d, x, y)                 \
    do {                                      \
        a = a + b + (x); d = rotr64(d ^ a, 32); \
        c = c + d; b = rotr64(b ^ c, 24);       \
        a = a + b + (y); d = rotr64(d ^ a, 16); \
        c = c + d; b = rotr64(b ^ c, 63);       \
    } while (0)

static void b2b_compress(b2b *s, const uint8_t blk[128], int last) {
    uint64_t m[16], v[16];
    for (int i = 0; i < 16; i++) m[i] = zell__ld64le(blk + 8 * i);
    for (int i = 0; i < 8; i++) {
        v[i] = s->h[i];
        v[i + 8] = B2_IV[i];
    }
    v[12] ^= s->t[0];
    v[13] ^= s->t[1];
    if (last) v[14] = ~v[14];
    for (int r = 0; r < 12; r++) {
        const uint8_t *z = B2_SIGMA[r];
        B2G(v[0], v[4], v[8], v[12], m[z[0]], m[z[1]]);
        B2G(v[1], v[5], v[9], v[13], m[z[2]], m[z[3]]);
        B2G(v[2], v[6], v[10], v[14], m[z[4]], m[z[5]]);
        B2G(v[3], v[7], v[11], v[15], m[z[6]], m[z[7]]);
        B2G(v[0], v[5], v[10], v[15], m[z[8]], m[z[9]]);
        B2G(v[1], v[6], v[11], v[12], m[z[10]], m[z[11]]);
        B2G(v[2], v[7], v[8], v[13], m[z[12]], m[z[13]]);
        B2G(v[3], v[4], v[9], v[14], m[z[14]], m[z[15]]);
    }
    for (int i = 0; i < 8; i++) s->h[i] ^= v[i] ^ v[i + 8];
}

static void b2b_init(b2b *s, size_t outlen, const uint8_t *key, size_t klen) {
    memset(s, 0, sizeof *s);
    for (int i = 0; i < 8; i++) s->h[i] = B2_IV[i];
    s->h[0] ^= 0x01010000ULL ^ ((uint64_t)klen << 8) ^ outlen;
    s->outlen = outlen;
    if (klen) {
        memcpy(s->buf, key, klen);
        s->n = 128;
    }
}

static void b2b_update(b2b *s, const uint8_t *p, size_t n) {
    while (n > 0) {
        if (s->n == 128) {
            s->t[0] += 128;
            if (s->t[0] < 128) s->t[1]++;
            b2b_compress(s, s->buf, 0);
            s->n = 0;
        }
        size_t k = 128 - s->n;
        if (k > n) k = n;
        memcpy(s->buf + s->n, p, k);
        s->n += k;
        p += k;
        n -= k;
    }
}

static void b2b_final(b2b *s, uint8_t *out) {
    s->t[0] += s->n;
    if (s->t[0] < s->n) s->t[1]++;
    memset(s->buf + s->n, 0, 128 - s->n);
    b2b_compress(s, s->buf, 1);
    uint8_t full[64];
    for (int i = 0; i < 8; i++) zell__st64le(full + 8 * i, s->h[i]);
    memcpy(out, full, s->outlen);
    zell__crypto_wipe(full, sizeof full);
    zell__crypto_wipe(s, sizeof *s);
}

void zell__blake2b(uint8_t *out, size_t outlen, const uint8_t *key, size_t klen, const uint8_t *p, size_t n) {
    b2b s;
    b2b_init(&s, outlen, key, klen);
    b2b_update(&s, p, n);
    b2b_final(&s, out);
}

static void le32(uint8_t b[4], uint32_t v) { zell__st32le(b, v); }

/* H' (RFC 9106 3.3): variable-length hash of (prefix || p). */
static void hprime(uint8_t *out, size_t T, const uint8_t *a, size_t an, const uint8_t *b, size_t bn) {
    uint8_t t4[4];
    le32(t4, (uint32_t)T);
    b2b s;
    if (T <= 64) {
        b2b_init(&s, T, NULL, 0);
        b2b_update(&s, t4, 4);
        b2b_update(&s, a, an);
        b2b_update(&s, b, bn);
        b2b_final(&s, out);
        return;
    }
    uint8_t v[64];
    b2b_init(&s, 64, NULL, 0);
    b2b_update(&s, t4, 4);
    b2b_update(&s, a, an);
    b2b_update(&s, b, bn);
    b2b_final(&s, v);
    size_t r = (T + 31) / 32 - 2;
    memcpy(out, v, 32);
    for (size_t i = 1; i < r; i++) {
        zell__blake2b(v, 64, NULL, 0, v, 64);
        memcpy(out + 32 * i, v, 32);
    }
    size_t last = T - 32 * r;
    zell__blake2b(out + 32 * r, last, NULL, 0, v, 64);
    zell__crypto_wipe(v, sizeof v);
}

/* ---- Argon2 ---- */

typedef struct {
    uint64_t v[128];
} blk;

#if !(defined(__x86_64__) && defined(__SSE2__))
static inline uint64_t fBlaMka(uint64_t x, uint64_t y) {
    uint64_t m = 0xFFFFFFFFULL;
    return x + y + 2 * ((x & m) * (y & m));
}

#define GB(a, b, c, d)                                  \
    do {                                                \
        a = fBlaMka(a, b); d = rotr64(d ^ a, 32);       \
        c = fBlaMka(c, d); b = rotr64(b ^ c, 24);       \
        a = fBlaMka(a, b); d = rotr64(d ^ a, 16);       \
        c = fBlaMka(c, d); b = rotr64(b ^ c, 63);       \
    } while (0)

#define PERM(v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15) \
    do {                                                                         \
        GB(v0, v4, v8, v12); GB(v1, v5, v9, v13); GB(v2, v6, v10, v14); GB(v3, v7, v11, v15); \
        GB(v0, v5, v10, v15); GB(v1, v6, v11, v12); GB(v2, v7, v8, v13); GB(v3, v4, v9, v14); \
    } while (0)
#endif

#if defined(__x86_64__) && defined(__SSE2__)
#include <emmintrin.h>

/* SSE2 version: the block as 64 registers of two words. A row (16 words)
 * or a column (words 2i, 2i + 1 of every row) is the 4x4 matrix of the
 * BLAKE2b round in eight registers A0 A1 B0 B1 C0 C1 D0 D1 (two words
 * each), so one register op runs two G functions. */
static inline __m128i blamka_sse(__m128i x, __m128i y) {
    __m128i z = _mm_mul_epu32(x, y);   /* lo32(x) * lo32(y) per word */
    return _mm_add_epi64(_mm_add_epi64(x, y), _mm_add_epi64(z, z));
}
#define ROTR32(x) _mm_shuffle_epi32((x), _MM_SHUFFLE(2, 3, 0, 1))
#define ROTR24(x) _mm_or_si128(_mm_srli_epi64((x), 24), _mm_slli_epi64((x), 40))
#define ROTR16(x) _mm_or_si128(_mm_srli_epi64((x), 16), _mm_slli_epi64((x), 48))
#define ROTR63(x) _mm_xor_si128(_mm_srli_epi64((x), 63), _mm_add_epi64((x), (x)))
#define G_SSE(a, b, c, d)                                          \
    do {                                                           \
        a = blamka_sse(a, b); d = ROTR32(_mm_xor_si128(d, a));     \
        c = blamka_sse(c, d); b = ROTR24(_mm_xor_si128(b, c));     \
        a = blamka_sse(a, b); d = ROTR16(_mm_xor_si128(d, a));     \
        c = blamka_sse(c, d); b = ROTR63(_mm_xor_si128(b, c));     \
    } while (0)
/* (hi(a), lo(b)) */
#define HILO(a, b) _mm_castpd_si128(_mm_shuffle_pd(_mm_castsi128_pd(a), _mm_castsi128_pd(b), 1))

static inline __attribute__((always_inline)) void round_sse(__m128i *A0, __m128i *A1, __m128i *B0, __m128i *B1,
                                                             __m128i *C0, __m128i *C1, __m128i *D0, __m128i *D1) {
    __m128i a0 = *A0, a1 = *A1, b0 = *B0, b1 = *B1, c0 = *C0, c1 = *C1, d0 = *D0, d1 = *D1;
    /* columns: G(v0 v4 v8 v12), G(v1 v5 v9 v13) | G(v2 v6 v10 v14), G(v3 v7 v11 v15) */
    G_SSE(a0, b0, c0, d0);
    G_SSE(a1, b1, c1, d1);
    /* diagonals: B (v5 v6)(v7 v4), C (v10 v11)(v8 v9), D (v15 v12)(v13 v14) */
    __m128i tb0 = HILO(b0, b1), tb1 = HILO(b1, b0);
    __m128i td0 = HILO(d1, d0), td1 = HILO(d0, d1);
    G_SSE(a0, tb0, c1, td0);
    G_SSE(a1, tb1, c0, td1);
    /* back */
    b0 = HILO(tb1, tb0);
    b1 = HILO(tb0, tb1);
    d0 = HILO(td0, td1);
    d1 = HILO(td1, td0);
    *A0 = a0; *A1 = a1; *B0 = b0; *B1 = b1; *C0 = c0; *C1 = c1; *D0 = d0; *D1 = d1;
}

/* next = G(prev, ref) (xor next when with_xor). */
static void fill_block(const blk *prev, const blk *ref, blk *next, int with_xor) {
    __m128i R[64], Z[64];
    const __m128i *p = (const __m128i *)prev->v, *r = (const __m128i *)ref->v;
    for (int i = 0; i < 64; i++) {
        R[i] = _mm_xor_si128(_mm_loadu_si128(p + i), _mm_loadu_si128(r + i));
        Z[i] = R[i];
    }
    for (int j = 0; j < 8; j++) {
        __m128i *q = Z + 8 * j;
        round_sse(&q[0], &q[1], &q[2], &q[3], &q[4], &q[5], &q[6], &q[7]);
    }
    for (int i = 0; i < 8; i++) {
        __m128i *q = Z + i;
        round_sse(&q[0], &q[8], &q[16], &q[24], &q[32], &q[40], &q[48], &q[56]);
    }
    __m128i *n = (__m128i *)next->v;
    if (with_xor)
        for (int i = 0; i < 64; i++)
            _mm_storeu_si128(n + i, _mm_xor_si128(_mm_loadu_si128(n + i), _mm_xor_si128(Z[i], R[i])));
    else
        for (int i = 0; i < 64; i++) _mm_storeu_si128(n + i, _mm_xor_si128(Z[i], R[i]));
}
#define HAVE_FILL_BLOCK_SSE2 1
#endif

#ifndef HAVE_FILL_BLOCK_SSE2
/* next = G(prev, ref) (xor next when with_xor): portable version. */
static void fill_block_scalar(const blk *prev, const blk *ref, blk *next, int with_xor) {
    blk R, Z;
    for (int i = 0; i < 128; i++) R.v[i] = prev->v[i] ^ ref->v[i];
    Z = R;
    for (int i = 0; i < 8; i++) {
        uint64_t *q = Z.v + 16 * i;
        PERM(q[0], q[1], q[2], q[3], q[4], q[5], q[6], q[7], q[8], q[9], q[10], q[11], q[12], q[13], q[14], q[15]);
    }
    for (int i = 0; i < 8; i++) {
        uint64_t *q = Z.v + 2 * i;
        PERM(q[0], q[1], q[16], q[17], q[32], q[33], q[48], q[49], q[64], q[65], q[80], q[81], q[96], q[97], q[112], q[113]);
    }
    if (with_xor)
        for (int i = 0; i < 128; i++) next->v[i] ^= Z.v[i] ^ R.v[i];
    else
        for (int i = 0; i < 128; i++) next->v[i] = Z.v[i] ^ R.v[i];
}

static void fill_block(const blk *prev, const blk *ref, blk *next, int with_xor) {
    fill_block_scalar(prev, ref, next, with_xor);
}
#endif

static void blk_from_bytes(blk *b, const uint8_t *p) {
    for (int i = 0; i < 128; i++) b->v[i] = zell__ld64le(p + 8 * i);
}

static void blk_to_bytes(uint8_t *p, const blk *b) {
    for (int i = 0; i < 128; i++) zell__st64le(p + 8 * i, b->v[i]);
}

typedef struct {
    blk *B;
    uint32_t q, seglen, mprime, p, t;
    int type;
} a2ctx;

/* Fills one segment (pass, slice, lane) of the memory. */
static void fill_segment(const a2ctx *x, uint32_t pass, uint32_t slice, uint32_t lane) {
    blk *B = x->B;
    uint32_t q = x->q, seglen = x->seglen, p = x->p;
    int type = x->type;
    blk zero, input, addr;
    memset(&zero, 0, sizeof zero);
    int indep = type == 1 || (type == 2 && pass == 0 && slice < 2);
    if (indep) {
        memset(&input, 0, sizeof input);
        input.v[0] = pass;
        input.v[1] = lane;
        input.v[2] = slice;
        input.v[3] = x->mprime;
        input.v[4] = x->t;
        input.v[5] = (uint64_t)type;
    }
    uint32_t start = 0;
    if (pass == 0 && slice == 0) {
        start = 2;
        if (indep) {
            input.v[6]++;
            fill_block(&zero, &input, &addr, 0);
            fill_block(&zero, &addr, &addr, 0);
        }
    }
    size_t cur = (size_t)lane * q + (size_t)slice * seglen + start;
    size_t prev = (cur % q == 0) ? cur + q - 1 : cur - 1;
    for (uint32_t i = start; i < seglen; i++, cur++, prev++) {
        if (cur % q == 1) prev = cur - 1;
        uint64_t rnd;
        if (indep) {
            if (i % 128 == 0) {
                input.v[6]++;
                fill_block(&zero, &input, &addr, 0);
                fill_block(&zero, &addr, &addr, 0);
            }
            rnd = addr.v[i % 128];
        } else {
            rnd = B[prev].v[0];
        }
        uint64_t refLane = (rnd >> 32) % p;
        if (pass == 0 && slice == 0) refLane = lane;
        int same = refLane == lane;
        uint64_t area;
        if (pass == 0) {
            if (slice == 0) area = i - 1;
            else if (same) area = (uint64_t)slice * seglen + i - 1;
            else area = (uint64_t)slice * seglen + (i == 0 ? (uint64_t)-1 : 0);
        } else {
            if (same) area = (uint64_t)q - seglen + i - 1;
            else area = (uint64_t)q - seglen + (i == 0 ? (uint64_t)-1 : 0);
        }
        uint64_t rel = rnd & 0xFFFFFFFFULL;
        rel = (rel * rel) >> 32;
        rel = area - 1 - ((area * rel) >> 32);
        uint64_t startPos = 0;
        if (pass != 0) startPos = (slice == 3) ? 0 : (uint64_t)(slice + 1) * seglen;
        uint64_t refIndex = (startPos + rel) % q;
        const blk *ref = &B[refLane * q + refIndex];
        fill_block(&B[prev], ref, &B[cur], pass != 0);
    }
    zell__crypto_wipe(&addr, sizeof addr);
}

#define A2_MAX_THREADS 64

typedef struct {
    const a2ctx *ctx;
    uint32_t pass, slice, first, step;
    bool wipe;
} a2job;

/* The lanes first, first + step, ... of one slice, or (wipe) their memory
 * zeroed at the end. */
static void *a2_worker(void *arg) {
    const a2job *j = arg;
    for (uint32_t lane = j->first; lane < j->ctx->p; lane += j->step) {
        if (j->wipe)
            zell__crypto_wipe(&j->ctx->B[(size_t)lane * j->ctx->q], (size_t)j->ctx->q * sizeof(blk));
        else
            fill_segment(j->ctx, j->pass, j->slice, lane);
    }
    return NULL;
}

/* Runs a2_worker over all lanes with nthreads threads (the caller's included). */
static void a2_run(const a2ctx *ctx, uint32_t nthreads, uint32_t pass, uint32_t slice, bool wipe) {
    a2job jobs[A2_MAX_THREADS];
    zell__plat_thread th[A2_MAX_THREADS];
    bool started[A2_MAX_THREADS] = { false };
    for (uint32_t k = 0; k < nthreads; k++) {
        jobs[k] = (a2job){ ctx, pass, slice, k, nthreads, wipe };
        if (k > 0) started[k] = zell__plat_thread_start(&th[k], a2_worker, &jobs[k]);
    }
    a2_worker(&jobs[0]);
    for (uint32_t k = 1; k < nthreads; k++) {
        if (started[k]) zell__plat_thread_join(th[k]);
        else a2_worker(&jobs[k]);   /* no thread: do its lanes here */
    }
}

/* type: 0 Argon2d, 1 Argon2i, 2 Argon2id. Returns 0 ok, 1 bad parameters,
 * 2 out of memory. */
int zell__argon2(int type, const uint8_t *pw, size_t pwlen, const uint8_t *salt, size_t slen, const uint8_t *secret,
                  size_t klen, const uint8_t *ad, size_t adlen, uint32_t t, uint32_t m, uint32_t p, uint8_t *out,
                  size_t T) {
    if (type < 0 || type > 2 || t < 1 || p < 1 || p > 0xFFFFFF || T < 4 || m < 8 * p || slen < 8 ||
        (uint64_t)T > 0xFFFFFFFFULL || klen > 32 || pwlen > 0xFFFFFFFFULL || slen > 0xFFFFFFFFULL ||
        adlen > 0xFFFFFFFFULL)
        return 1;
    uint32_t mprime = 4 * p * (m / (4 * p));
    uint32_t q = mprime / p, seglen = q / 4;
    blk *B = malloc((size_t)mprime * sizeof(blk));
    if (!B) return 2;
    /* H0 */
    uint8_t h0[72], w[4];
    b2b s;
    b2b_init(&s, 64, NULL, 0);
    le32(w, p); b2b_update(&s, w, 4);
    le32(w, (uint32_t)T); b2b_update(&s, w, 4);
    le32(w, m); b2b_update(&s, w, 4);
    le32(w, t); b2b_update(&s, w, 4);
    le32(w, 0x13); b2b_update(&s, w, 4);
    le32(w, (uint32_t)type); b2b_update(&s, w, 4);
    le32(w, (uint32_t)pwlen); b2b_update(&s, w, 4); b2b_update(&s, pw, pwlen);
    le32(w, (uint32_t)slen); b2b_update(&s, w, 4); b2b_update(&s, salt, slen);
    le32(w, (uint32_t)klen); b2b_update(&s, w, 4); b2b_update(&s, secret, klen);
    le32(w, (uint32_t)adlen); b2b_update(&s, w, 4); b2b_update(&s, ad, adlen);
    b2b_final(&s, h0);
    uint8_t tmp[1024];
    for (uint32_t l = 0; l < p; l++) {
        le32(h0 + 64, 0);
        le32(h0 + 68, l);
        hprime(tmp, 1024, h0, 72, NULL, 0);
        blk_from_bytes(&B[(size_t)l * q], tmp);
        le32(h0 + 64, 1);
        hprime(tmp, 1024, h0, 72, NULL, 0);
        blk_from_bytes(&B[(size_t)l * q + 1], tmp);
    }
    a2ctx ctx = { B, q, seglen, mprime, p, t, type };
    int64_t cpus = zell__plat_cpu_limit(NULL);
    uint32_t nthreads = p;
    if (cpus > 0 && (uint64_t)cpus < nthreads) nthreads = (uint32_t)cpus;
    if (nthreads > A2_MAX_THREADS) nthreads = A2_MAX_THREADS;
    for (uint32_t pass = 0; pass < t; pass++)
        for (uint32_t slice = 0; slice < 4; slice++) a2_run(&ctx, nthreads, pass, slice, false);
    blk c = B[q - 1];
    for (uint32_t l = 1; l < p; l++)
        for (int i = 0; i < 128; i++) c.v[i] ^= B[(size_t)l * q + q - 1].v[i];
    blk_to_bytes(tmp, &c);
    hprime(out, T, tmp, 1024, NULL, 0);
    zell__crypto_wipe(tmp, sizeof tmp);
    zell__crypto_wipe(h0, sizeof h0);
    zell__crypto_wipe(&c, sizeof c);
    a2_run(&ctx, nthreads, 0, 0, true);   /* zero the memory, lanes in parallel */
    free(B);
    return 0;
}

/* ---- Zell wrappers: crypto.kdf (ABI.md 23) ---- */

static inline int64_t blen(const zell_slice *s) { return s ? s->len : 0; }
static inline const uint8_t *bdata(const zell_slice *s) {
    return s && s->len ? (const uint8_t *)s->data : (const uint8_t *)"";
}

static uint8_t *new_bytes(zell_slice *out, int64_t n) {
    zell_slice_new(out, 1, n, n);
    return n ? (uint8_t *)out->data : NULL;
}

/* kdf.rtArgon2(type, password, salt, secret, ad, time, memoryKiB, threads, keyLen) []byte! */
void zell_crypto_kdf_rt_argon2(int64_t type, const zell_slice *pw, const zell_slice *salt, const zell_slice *secret,
                                const zell_slice *ad, int64_t t, int64_t m, int64_t p, int64_t n, zell_slice *out,
                                zell_error **err) {
    memset(out, 0, sizeof *out);
    if (t < 1 || t > 0xFFFFFFFFLL || p < 1 || p > 0xFFFFFF || m < 8 * p || m > 0xFFFFFFFFLL || n < 4 || n > (1 << 20)) {
        if (err) *err = zell_error_new_cstr("argon2: invalid parameters (time >= 1, threads >= 1, memory >= 8*threads KiB, keyLen >= 4)");
        return;
    }
    if (blen(salt) < 8) {
        if (err) *err = zell_error_new_cstr("argon2: salt must be at least 8 bytes");
        return;
    }
    if (blen(secret) > 32) {
        if (err) *err = zell_error_new_cstr("argon2: secret longer than 32 bytes");
        return;
    }
    uint8_t *o = new_bytes(out, n);
    int rc = zell__argon2((int)type, bdata(pw), (size_t)blen(pw), bdata(salt), (size_t)blen(salt), bdata(secret),
                           (size_t)blen(secret), bdata(ad), (size_t)blen(ad), (uint32_t)t, (uint32_t)m, (uint32_t)p, o,
                           (size_t)n);
    if (rc != 0) {
        zell_slice_release(out);
        if (err) *err = zell_error_new_cstr(rc == 2 ? "argon2: out of memory" : "argon2: invalid parameters");
    }
}

/* kdf.rtBlake2b(key, data, size) []byte! */
void zell_crypto_kdf_rt_blake2b(const zell_slice *key, const zell_slice *data, int64_t size, zell_slice *out,
                                 zell_error **err) {
    memset(out, 0, sizeof *out);
    if (size < 1 || size > 64 || blen(key) > 64) {
        if (err) *err = zell_error_new_cstr("blake2b: size must be 1..64 and the key at most 64 bytes");
        return;
    }
    uint8_t *o = new_bytes(out, size);
    zell__blake2b(o, (size_t)size, bdata(key), (size_t)blen(key), bdata(data), (size_t)blen(data));
}
