/* sha2.c - streaming SHA-256, SHA-384, SHA-512 (FIPS 180-4) and SHA-1
 * (FIPS 180-4, only for legacy formats), plus HMAC (RFC 2104) over them.
 * Contexts are plain bytes (zell_hash_ctx), so the Zell wrappers keep
 * them inside a []byte.
 *
 * SHA-256 has two compression functions: the portable one, and on x86-64
 * one built on the SHA extensions (sha256rnds2/msg1/msg2, compiled with a
 * function-level target attribute) chosen at run time by cpuid
 * (zell__cpu_has_shani; ZELL_NO_SHANI=1 forces the portable path). */
#include "crypto.h"

#include <string.h>

static inline uint32_t ror32(uint32_t x, int n) { return (x >> n) | (x << (32 - n)); }
static inline uint32_t rol32(uint32_t x, int n) { return (x << n) | (x >> (32 - n)); }
static inline uint64_t ror64(uint64_t x, int n) { return (x >> n) | (x << (64 - n)); }

static const uint32_t K256[64] = {
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
};

static const uint64_t K512[80] = {
    0x428a2f98d728ae22ULL, 0x7137449123ef65cdULL, 0xb5c0fbcfec4d3b2fULL, 0xe9b5dba58189dbbcULL,
    0x3956c25bf348b538ULL, 0x59f111f1b605d019ULL, 0x923f82a4af194f9bULL, 0xab1c5ed5da6d8118ULL,
    0xd807aa98a3030242ULL, 0x12835b0145706fbeULL, 0x243185be4ee4b28cULL, 0x550c7dc3d5ffb4e2ULL,
    0x72be5d74f27b896fULL, 0x80deb1fe3b1696b1ULL, 0x9bdc06a725c71235ULL, 0xc19bf174cf692694ULL,
    0xe49b69c19ef14ad2ULL, 0xefbe4786384f25e3ULL, 0x0fc19dc68b8cd5b5ULL, 0x240ca1cc77ac9c65ULL,
    0x2de92c6f592b0275ULL, 0x4a7484aa6ea6e483ULL, 0x5cb0a9dcbd41fbd4ULL, 0x76f988da831153b5ULL,
    0x983e5152ee66dfabULL, 0xa831c66d2db43210ULL, 0xb00327c898fb213fULL, 0xbf597fc7beef0ee4ULL,
    0xc6e00bf33da88fc2ULL, 0xd5a79147930aa725ULL, 0x06ca6351e003826fULL, 0x142929670a0e6e70ULL,
    0x27b70a8546d22ffcULL, 0x2e1b21385c26c926ULL, 0x4d2c6dfc5ac42aedULL, 0x53380d139d95b3dfULL,
    0x650a73548baf63deULL, 0x766a0abb3c77b2a8ULL, 0x81c2c92e47edaee6ULL, 0x92722c851482353bULL,
    0xa2bfe8a14cf10364ULL, 0xa81a664bbc423001ULL, 0xc24b8b70d0f89791ULL, 0xc76c51a30654be30ULL,
    0xd192e819d6ef5218ULL, 0xd69906245565a910ULL, 0xf40e35855771202aULL, 0x106aa07032bbd1b8ULL,
    0x19a4c116b8d2d0c8ULL, 0x1e376c085141ab53ULL, 0x2748774cdf8eeb99ULL, 0x34b0bcb5e19b48a8ULL,
    0x391c0cb3c5c95a63ULL, 0x4ed8aa4ae3418acbULL, 0x5b9cca4f7763e373ULL, 0x682e6ff3d6b2b8a3ULL,
    0x748f82ee5defb2fcULL, 0x78a5636f43172f60ULL, 0x84c87814a1f0ab72ULL, 0x8cc702081a6439ecULL,
    0x90befffa23631e28ULL, 0xa4506cebde82bde9ULL, 0xbef9a3f7b2c67915ULL, 0xc67178f2e372532bULL,
    0xca273eceea26619cULL, 0xd186b8c721c0c207ULL, 0xeada7dd6cde0eb1eULL, 0xf57d4f7fee6ed178ULL,
    0x06f067aa72176fbaULL, 0x0a637dc5a2c898a6ULL, 0x113f9804bef90daeULL, 0x1b710b35131c471bULL,
    0x28db77f523047d84ULL, 0x32caab7b40c72493ULL, 0x3c9ebe0a15c9bebcULL, 0x431d67c49c100d4cULL,
    0x4cc5d4becb3e42b6ULL, 0x597f299cfc657e2aULL, 0x5fcb6fab3ad6faecULL, 0x6c44198c4a475817ULL,
};

static void sha256_block(uint32_t *h, const uint8_t *p) {
    uint32_t w[64];
    for (int t = 0; t < 16; t++) w[t] = zell__ld32be(p + 4 * t);
    for (int t = 16; t < 64; t++) {
        uint32_t s0 = ror32(w[t - 15], 7) ^ ror32(w[t - 15], 18) ^ (w[t - 15] >> 3);
        uint32_t s1 = ror32(w[t - 2], 17) ^ ror32(w[t - 2], 19) ^ (w[t - 2] >> 10);
        w[t] = w[t - 16] + s0 + w[t - 7] + s1;
    }
    uint32_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
    for (int t = 0; t < 64; t++) {
        uint32_t t1 = hh + (ror32(e, 6) ^ ror32(e, 11) ^ ror32(e, 25)) + ((e & f) ^ (~e & g)) + K256[t] + w[t];
        uint32_t t2 = (ror32(a, 2) ^ ror32(a, 13) ^ ror32(a, 22)) + ((a & b) ^ (a & c) ^ (b & c));
        hh = g; g = f; f = e; e = d + t1; d = c; c = b; b = a; a = t1 + t2;
    }
    h[0] += a; h[1] += b; h[2] += c; h[3] += d; h[4] += e; h[5] += f; h[6] += g; h[7] += hh;
}

#if defined(__x86_64__) && (defined(__GNUC__) || defined(__clang__))
#define ZELL_HAVE_SHANI 1
#include <immintrin.h>

#define SHANI_TARGET __attribute__((target("sha,sse4.1,ssse3")))

/* Four rounds 4g..4g+3 with message words m0 = W[4g..4g+3]; while g < 12
 * also turns m0 into W[4g+16..4g+19] from m1 = W[4g+4..], m2, m3. */
#define SHANI_QROUND(g, m0, m1, m2, m3)                                              \
    do {                                                                             \
        __m128i k_ = _mm_add_epi32(m0, _mm_loadu_si128((const __m128i *)(K256 + 4 * (g)))); \
        s1 = _mm_sha256rnds2_epu32(s1, s0, k_);                                      \
        if ((g) < 12)                                                                \
            m0 = _mm_sha256msg2_epu32(                                               \
                _mm_add_epi32(_mm_sha256msg1_epu32(m0, m1), _mm_alignr_epi8(m3, m2, 4)), m3); \
        s0 = _mm_sha256rnds2_epu32(s0, s1, _mm_shuffle_epi32(k_, 0x0E));             \
    } while (0)

/* n consecutive 64-byte blocks. The state lives as ABEF/CDGH words, the
 * layout sha256rnds2 works on. */
SHANI_TARGET static void sha256_blocks_ni(uint32_t *h, const uint8_t *p, size_t n) {
    const __m128i bswap = _mm_set_epi64x(0x0c0d0e0f08090a0bULL, 0x0405060700010203ULL);
    __m128i t = _mm_shuffle_epi32(_mm_loadu_si128((const __m128i *)h), 0xB1);       /* CDAB */
    __m128i s1 = _mm_shuffle_epi32(_mm_loadu_si128((const __m128i *)(h + 4)), 0x1B); /* EFGH */
    __m128i s0 = _mm_alignr_epi8(t, s1, 8);                                         /* ABEF */
    s1 = _mm_blend_epi16(s1, t, 0xF0);                                              /* CDGH */
    for (; n; n--, p += 64) {
        __m128i abef = s0, cdgh = s1;
        __m128i w0 = _mm_shuffle_epi8(_mm_loadu_si128((const __m128i *)p), bswap);
        __m128i w1 = _mm_shuffle_epi8(_mm_loadu_si128((const __m128i *)(p + 16)), bswap);
        __m128i w2 = _mm_shuffle_epi8(_mm_loadu_si128((const __m128i *)(p + 32)), bswap);
        __m128i w3 = _mm_shuffle_epi8(_mm_loadu_si128((const __m128i *)(p + 48)), bswap);
        SHANI_QROUND(0, w0, w1, w2, w3);
        SHANI_QROUND(1, w1, w2, w3, w0);
        SHANI_QROUND(2, w2, w3, w0, w1);
        SHANI_QROUND(3, w3, w0, w1, w2);
        SHANI_QROUND(4, w0, w1, w2, w3);
        SHANI_QROUND(5, w1, w2, w3, w0);
        SHANI_QROUND(6, w2, w3, w0, w1);
        SHANI_QROUND(7, w3, w0, w1, w2);
        SHANI_QROUND(8, w0, w1, w2, w3);
        SHANI_QROUND(9, w1, w2, w3, w0);
        SHANI_QROUND(10, w2, w3, w0, w1);
        SHANI_QROUND(11, w3, w0, w1, w2);
        SHANI_QROUND(12, w0, w1, w2, w3);
        SHANI_QROUND(13, w1, w2, w3, w0);
        SHANI_QROUND(14, w2, w3, w0, w1);
        SHANI_QROUND(15, w3, w0, w1, w2);
        s0 = _mm_add_epi32(s0, abef);
        s1 = _mm_add_epi32(s1, cdgh);
    }
    t = _mm_shuffle_epi32(s0, 0x1B);                                        /* FEBA */
    s1 = _mm_shuffle_epi32(s1, 0xB1);                                       /* DCHG */
    _mm_storeu_si128((__m128i *)h, _mm_blend_epi16(t, s1, 0xF0));           /* DCBA */
    _mm_storeu_si128((__m128i *)(h + 4), _mm_alignr_epi8(s1, t, 8));        /* HGFE */
}
#undef SHANI_QROUND
#endif

/* n consecutive SHA-256 blocks: SHA-NI when available, else portable. */
static void sha256_blocks(uint32_t *h, const uint8_t *p, size_t n) {
#ifdef ZELL_HAVE_SHANI
    if (zell__cpu_has_shani()) {
        sha256_blocks_ni(h, p, n);
        return;
    }
#endif
    for (; n; n--, p += 64) sha256_block(h, p);
}

/* SHA-512 compression: rounds unrolled by eight with the roles of the
 * working variables rotated instead of moved, the message schedule kept
 * in a 16-word ring and expanded as the rounds go. */
#define S512_SIGMA0(a) (ror64(a, 28) ^ ror64(a, 34) ^ ror64(a, 39))
#define S512_SIGMA1(e) (ror64(e, 14) ^ ror64(e, 18) ^ ror64(e, 41))
#define S512_s0(x) (ror64(x, 1) ^ ror64(x, 8) ^ ((x) >> 7))
#define S512_s1(x) (ror64(x, 19) ^ ror64(x, 61) ^ ((x) >> 6))
#define S512_ROUND(a, b, c, d, e, f, g, h, k, w)                                      do {                                                                                  uint64_t t1_ = (h) + S512_SIGMA1(e) + ((g) ^ ((e) & ((f) ^ (g)))) + (k) + (w);         uint64_t t2_ = S512_SIGMA0(a) + (((a) & (b)) | ((c) & ((a) | (b))));                (d) += t1_;                                                                       (h) = t1_ + t2_;                                                              } while (0)
#define S512_W(i) (w[(i) & 15] += S512_s1(w[((i) - 2) & 15]) + w[((i) - 7) & 15] + S512_s0(w[((i) - 15) & 15]))

static void sha512_block(uint64_t *h, const uint8_t *p) {
    uint64_t w[16];
    for (int t = 0; t < 16; t++) w[t] = zell__ld64be(p + 8 * t);
    uint64_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
    for (int t = 0; t < 16; t += 8) {
        S512_ROUND(a, b, c, d, e, f, g, hh, K512[t + 0], w[t + 0]);
        S512_ROUND(hh, a, b, c, d, e, f, g, K512[t + 1], w[t + 1]);
        S512_ROUND(g, hh, a, b, c, d, e, f, K512[t + 2], w[t + 2]);
        S512_ROUND(f, g, hh, a, b, c, d, e, K512[t + 3], w[t + 3]);
        S512_ROUND(e, f, g, hh, a, b, c, d, K512[t + 4], w[t + 4]);
        S512_ROUND(d, e, f, g, hh, a, b, c, K512[t + 5], w[t + 5]);
        S512_ROUND(c, d, e, f, g, hh, a, b, K512[t + 6], w[t + 6]);
        S512_ROUND(b, c, d, e, f, g, hh, a, K512[t + 7], w[t + 7]);
    }
    for (int t = 16; t < 80; t += 8) {
        S512_ROUND(a, b, c, d, e, f, g, hh, K512[t + 0], S512_W(t + 0));
        S512_ROUND(hh, a, b, c, d, e, f, g, K512[t + 1], S512_W(t + 1));
        S512_ROUND(g, hh, a, b, c, d, e, f, K512[t + 2], S512_W(t + 2));
        S512_ROUND(f, g, hh, a, b, c, d, e, K512[t + 3], S512_W(t + 3));
        S512_ROUND(e, f, g, hh, a, b, c, d, K512[t + 4], S512_W(t + 4));
        S512_ROUND(d, e, f, g, hh, a, b, c, K512[t + 5], S512_W(t + 5));
        S512_ROUND(c, d, e, f, g, hh, a, b, K512[t + 6], S512_W(t + 6));
        S512_ROUND(b, c, d, e, f, g, hh, a, K512[t + 7], S512_W(t + 7));
    }
    h[0] += a; h[1] += b; h[2] += c; h[3] += d; h[4] += e; h[5] += f; h[6] += g; h[7] += hh;
}
#undef S512_W
#undef S512_ROUND

static void sha1_block(uint32_t *h, const uint8_t *p) {
    uint32_t w[80];
    for (int t = 0; t < 16; t++) w[t] = zell__ld32be(p + 4 * t);
    for (int t = 16; t < 80; t++) w[t] = rol32(w[t - 3] ^ w[t - 8] ^ w[t - 14] ^ w[t - 16], 1);
    uint32_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4];
    for (int t = 0; t < 80; t++) {
        uint32_t f, k;
        if (t < 20) { f = (b & c) | (~b & d); k = 0x5a827999; }
        else if (t < 40) { f = b ^ c ^ d; k = 0x6ed9eba1; }
        else if (t < 60) { f = (b & c) | (b & d) | (c & d); k = 0x8f1bbcdc; }
        else { f = b ^ c ^ d; k = 0xca62c1d6; }
        uint32_t tmp = rol32(a, 5) + f + e + k + w[t];
        e = d; d = c; c = rol32(b, 30); b = a; a = tmp;
    }
    h[0] += a; h[1] += b; h[2] += c; h[3] += d; h[4] += e;
}

size_t zell__hash_size(int alg) {
    switch (alg) {
    case ZELL_HASH_SHA256: return 32;
    case ZELL_HASH_SHA384: return 48;
    case ZELL_HASH_SHA512: return 64;
    case ZELL_HASH_SHA1: return 20;
    default: return 0;
    }
}

size_t zell__hash_block(int alg) {
    return alg == ZELL_HASH_SHA384 || alg == ZELL_HASH_SHA512 ? 128 : 64;
}

bool zell__hash_init(zell_hash_ctx *c, int alg) {
    static const uint32_t iv256[8] = { 0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                                       0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19 };
    static const uint64_t iv384[8] = { 0xcbbb9d5dc1059ed8ULL, 0x629a292a367cd507ULL, 0x9159015a3070dd17ULL,
                                       0x152fecd8f70e5939ULL, 0x67332667ffc00b31ULL, 0x8eb44a8768581511ULL,
                                       0xdb0c2e0d64f98fa7ULL, 0x47b5481dbefa4fa4ULL };
    static const uint64_t iv512[8] = { 0x6a09e667f3bcc908ULL, 0xbb67ae8584caa73bULL, 0x3c6ef372fe94f82bULL,
                                       0xa54ff53a5f1d36f1ULL, 0x510e527fade682d1ULL, 0x9b05688c2b3e6c1fULL,
                                       0x1f83d9abfb41bd6bULL, 0x5be0cd19137e2179ULL };
    static const uint32_t iv1[5] = { 0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0 };
    memset(c, 0, sizeof *c);
    c->alg = (uint32_t)alg;
    switch (alg) {
    case ZELL_HASH_SHA256: memcpy(c->h.h32, iv256, sizeof iv256); return true;
    case ZELL_HASH_SHA384: memcpy(c->h.h64, iv384, sizeof iv384); return true;
    case ZELL_HASH_SHA512: memcpy(c->h.h64, iv512, sizeof iv512); return true;
    case ZELL_HASH_SHA1: memcpy(c->h.h32, iv1, sizeof iv1); return true;
    default: return false;
    }
}

/* n consecutive blocks of the context's algorithm. */
static void compress(zell_hash_ctx *c, const uint8_t *blk, size_t n) {
    switch (c->alg) {
    case ZELL_HASH_SHA256: sha256_blocks(c->h.h32, blk, n); break;
    case ZELL_HASH_SHA1: for (; n; n--, blk += 64) sha1_block(c->h.h32, blk); break;
    default: for (; n; n--, blk += 128) sha512_block(c->h.h64, blk); break;
    }
}

void zell__hash_update(zell_hash_ctx *c, const uint8_t *p, size_t n) {
    size_t bs = zell__hash_block((int)c->alg);
    c->total += n;
    if (c->buflen) {
        size_t k = bs - c->buflen;
        if (k > n) k = n;
        memcpy(c->buf + c->buflen, p, k);
        c->buflen += (uint32_t)k;
        p += k;
        n -= k;
        if (c->buflen < bs) return;
        compress(c, c->buf, 1);
        c->buflen = 0;
    }
    if (n >= bs) {
        size_t nb = n / bs;
        compress(c, p, nb);
        p += nb * bs;
        n -= nb * bs;
    }
    if (n) {
        memcpy(c->buf, p, n);
        c->buflen = (uint32_t)n;
    }
}

void zell__hash_final(zell_hash_ctx *c, uint8_t *out) {
    size_t bs = zell__hash_block((int)c->alg), lenb = bs == 128 ? 16 : 8;
    uint64_t bits = c->total * 8;
    uint8_t pad[256];
    memset(pad, 0, sizeof pad);
    pad[0] = 0x80;
    size_t used = c->buflen + 1 + lenb;
    size_t padlen = (used <= bs ? bs : 2 * bs) - c->buflen;
    zell__st64be(pad + padlen - 8, bits);
    zell__hash_update(c, pad, padlen);
    size_t n = zell__hash_size((int)c->alg);
    if (c->alg == ZELL_HASH_SHA256 || c->alg == ZELL_HASH_SHA1) {
        for (size_t i = 0; i < n / 4; i++) zell__st32be(out + 4 * i, c->h.h32[i]);
    } else {
        uint8_t tmp[64];
        for (int i = 0; i < 8; i++) zell__st64be(tmp + 8 * i, c->h.h64[i]);
        memcpy(out, tmp, n);
    }
}

void zell__hash(int alg, const uint8_t *p, size_t n, uint8_t *out) {
    zell_hash_ctx c;
    zell__hash_init(&c, alg);
    zell__hash_update(&c, p, n);
    zell__hash_final(&c, out);
}

static int name_eq(const char *a, size_t n, const char *lit) {
    size_t i = 0, j = 0;
    for (; i < n && lit[j]; i++) {
        char ch = a[i];
        if (ch == '-' || ch == '_') continue;
        if (ch >= 'A' && ch <= 'Z') ch = (char)(ch + 32);
        if (ch != lit[j]) return 0;
        j++;
    }
    for (; i < n; i++) if (a[i] != '-' && a[i] != '_') return 0;
    return lit[j] == 0;
}

int zell__hash_by_name(const char *name, size_t len) {
    if (name_eq(name, len, "sha256")) return ZELL_HASH_SHA256;
    if (name_eq(name, len, "sha384")) return ZELL_HASH_SHA384;
    if (name_eq(name, len, "sha512")) return ZELL_HASH_SHA512;
    if (name_eq(name, len, "sha1")) return ZELL_HASH_SHA1;
    return 0;
}

void zell__hmac_init(zell_hmac_ctx *c, int alg, const uint8_t *key, size_t klen) {
    size_t bs = zell__hash_block(alg);
    uint8_t k[128], pad[128];
    memset(k, 0, sizeof k);
    if (klen > bs) zell__hash(alg, key, klen, k);
    else if (klen) memcpy(k, key, klen);
    for (size_t i = 0; i < bs; i++) pad[i] = k[i] ^ 0x36;
    zell__hash_init(&c->inner, alg);
    zell__hash_update(&c->inner, pad, bs);
    for (size_t i = 0; i < bs; i++) pad[i] = k[i] ^ 0x5c;
    zell__hash_init(&c->outer, alg);
    zell__hash_update(&c->outer, pad, bs);
    zell__crypto_wipe(k, sizeof k);
    zell__crypto_wipe(pad, sizeof pad);
}

void zell__hmac_update(zell_hmac_ctx *c, const uint8_t *p, size_t n) {
    zell__hash_update(&c->inner, p, n);
}

void zell__hmac_final(zell_hmac_ctx *c, uint8_t *out) {
    uint8_t ih[64];
    size_t n = zell__hash_size((int)c->inner.alg);
    zell__hash_final(&c->inner, ih);
    zell__hash_update(&c->outer, ih, n);
    zell__hash_final(&c->outer, out);
    zell__crypto_wipe(ih, sizeof ih);
}

void zell__hmac(int alg, const uint8_t *key, size_t klen, const uint8_t *msg, size_t n, uint8_t *out) {
    zell_hmac_ctx c;
    zell__hmac_init(&c, alg, key, klen);
    zell__hmac_update(&c, msg, n);
    zell__hmac_final(&c, out);
    zell__crypto_wipe(&c, sizeof c);
}
