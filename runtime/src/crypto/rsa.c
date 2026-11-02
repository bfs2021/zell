/* rsa.c - RSA signatures (RFC 8017): RSASSA-PKCS1-v1_5 and RSASSA-PSS
 * (MGF1, same hash) verification and signing.
 *
 * Bignums are little-endian arrays of 64-bit limbs (up to 8192-bit moduli)
 * with Montgomery multiplication (CIOS). Public operations (verification)
 * handle only public data and use plain square-and-multiply. Private
 * operations use CRT with
 *   - exponent blinding: d_p + r (p - 1), d_q + r' (q - 1) with random
 *     64-bit r, r', so the exponent bits differ on every signature;
 *   - a fixed 4-bit-window exponentiation whose table lookups scan every
 *     entry with masks (no secret-dependent branches or indices);
 *   - verification of the result with the public exponent before it is
 *     released (protects against fault attacks on CRT).
 * The private key is a PKCS#1 RSAPrivateKey DER (RFC 8017 A.1.2). */
#include "crypto.h"

#include <string.h>

typedef unsigned __int128 u128;

#define MAXL 130   /* limbs: 8192-bit modulus + slack */

typedef struct {
    int nl;               /* limbs of the modulus */
    uint64_t m[MAXL];
    uint64_t m0;          /* -m^-1 mod 2^64 */
    uint64_t rr[MAXL];    /* R^2 mod m */
} mctx;

/* ---- limb helpers ---- */

static bool bn_from_be(uint64_t *r, int nl, const uint8_t *b, size_t len) {
    while (len && *b == 0) { b++; len--; }
    if (len > (size_t)nl * 8) return false;
    memset(r, 0, (size_t)nl * 8);
    for (size_t i = 0; i < len; i++) {
        size_t bit = (len - 1 - i) * 8;
        r[bit / 64] |= (uint64_t)b[i] << (bit % 64);
    }
    return true;
}

static void bn_to_be(uint8_t *b, size_t len, const uint64_t *a, int nl) {
    for (size_t i = 0; i < len; i++) {
        size_t bit = (len - 1 - i) * 8;
        b[i] = bit / 64 < (size_t)nl ? (uint8_t)(a[bit / 64] >> (bit % 64)) : 0;
    }
}

static uint64_t bn_sub(uint64_t *r, const uint64_t *a, const uint64_t *b, int nl) {
    uint64_t borrow = 0;
    for (int i = 0; i < nl; i++) {
        u128 t = (u128)a[i] - b[i] - borrow;
        r[i] = (uint64_t)t;
        borrow = (uint64_t)(t >> 64) & 1;
    }
    return borrow;
}

static uint64_t bn_add(uint64_t *r, const uint64_t *a, const uint64_t *b, int nl) {
    uint64_t carry = 0;
    for (int i = 0; i < nl; i++) {
        u128 t = (u128)a[i] + b[i] + carry;
        r[i] = (uint64_t)t;
        carry = (uint64_t)(t >> 64);
    }
    return carry;
}

static void bn_cmov(uint64_t *r, const uint64_t *a, int nl, uint64_t mask) {
    for (int i = 0; i < nl; i++) r[i] ^= (r[i] ^ a[i]) & mask;
}


static int bn_bits(const uint64_t *a, int nl) {
    for (int i = nl - 1; i >= 0; i--)
        if (a[i]) return i * 64 + 64 - __builtin_clzll(a[i]);
    return 0;
}

/* ---- Montgomery ---- */

static void mmul(const mctx *c, uint64_t *r, const uint64_t *a, const uint64_t *b);
static void msqr(const mctx *c, uint64_t *r, const uint64_t *a);

static bool mctx_init(mctx *c, const uint64_t *m, int nl) {
    if (nl < 1 || nl > MAXL - 2 || !(m[0] & 1)) return false;
    memset(c, 0, sizeof *c);
    c->nl = nl;
    memcpy(c->m, m, (size_t)nl * 8);
    uint64_t inv = 1;
    for (int i = 0; i < 7; i++) inv *= 2 - m[0] * inv;
    c->m0 = 0 - inv;
    int mb = bn_bits(m, nl), r = 64 * nl;
    if (mb < 2) return false;
    /* x = 2^(r + 64) mod m = Mont(2^64): start from 2^(mb-1) < m and double
     * up to 2^(r+64) (about 65 doublings, as the top limb is nonzero) */
    uint64_t x[MAXL] = { 0 }, t[MAXL];
    x[(mb - 1) / 64] = (uint64_t)1 << ((mb - 1) % 64);
    for (int i = mb - 1; i < r + 64; i++) {
        uint64_t carry = bn_add(x, x, x, nl);
        uint64_t borrow = bn_sub(t, x, c->m, nl);
        bn_cmov(x, t, nl, 0 - (carry | (borrow ^ 1)));
    }
    /* Mont(2^64)^nl = Mont(2^(64 nl)) = Mont(R) = R^2 mod m: square while
     * the exponent doubles, multiply for the set bits of nl */
    uint64_t acc[MAXL];
    memcpy(acc, x, (size_t)nl * 8);
    for (int b = 30 - __builtin_clz((unsigned)nl); b >= 0; b--) {
        msqr(c, acc, acc);
        if ((nl >> b) & 1) mmul(c, acc, acc, x);
    }
    memcpy(c->rr, acc, (size_t)nl * 8);
    return true;
}

/* Montgomery multiplication by product scanning (the "FIPS" method): the
 * column sums of a b and of u m (u the quotient digits) accumulate in three
 * words, so the multiplications of a column are independent instead of one
 * carry chain through a row. */

/* (c2:c1:c0) += x y */
static inline void mac3(uint64_t *c0, uint64_t *c1, uint64_t *c2, uint64_t x, uint64_t y) {
    u128 p = (u128)x * y;
    u128 s = (u128)*c0 + (uint64_t)p;
    *c0 = (uint64_t)s;
    s = (u128)*c1 + (uint64_t)(p >> 64) + (uint64_t)(s >> 64);
    *c1 = (uint64_t)s;
    *c2 += (uint64_t)(s >> 64);
}

/* r = a b R^-1 mod m (a, b < m); r may alias a or b. */
static void mmul(const mctx *c, uint64_t *r, const uint64_t *a, const uint64_t *b) {
    int nl = c->nl;
    const uint64_t *m = c->m;
    uint64_t u[MAXL], out[MAXL + 1];
    uint64_t c0 = 0, c1 = 0, c2 = 0;
    for (int i = 0; i < nl; i++) {
        for (int j = 0; j < i; j++) {
            mac3(&c0, &c1, &c2, a[j], b[i - j]);
            mac3(&c0, &c1, &c2, u[j], m[i - j]);
        }
        mac3(&c0, &c1, &c2, a[i], b[0]);
        u[i] = c0 * c->m0;
        mac3(&c0, &c1, &c2, u[i], m[0]);
        c0 = c1;
        c1 = c2;
        c2 = 0;
    }
    for (int i = nl; i < 2 * nl; i++) {
        for (int j = i - nl + 1; j < nl; j++) {
            mac3(&c0, &c1, &c2, a[j], b[i - j]);
            mac3(&c0, &c1, &c2, u[j], m[i - j]);
        }
        out[i - nl] = c0;
        c0 = c1;
        c1 = c2;
        c2 = 0;
    }
    uint64_t d[MAXL];
    uint64_t borrow = bn_sub(d, out, m, nl);
    bn_cmov(out, d, nl, 0 - (c0 | (borrow ^ 1)));
    memcpy(r, out, (size_t)nl * 8);
}

/* (c2:c1:c0) += 2 (s2:s1:s0) */
static inline void add_twice(uint64_t *c0, uint64_t *c1, uint64_t *c2, uint64_t s0, uint64_t s1, uint64_t s2) {
    uint64_t d2 = (s2 << 1) | (s1 >> 63), d1 = (s1 << 1) | (s0 >> 63), d0 = s0 << 1;
    u128 x = (u128)*c0 + d0;
    *c0 = (uint64_t)x;
    x = (u128)*c1 + d1 + (uint64_t)(x >> 64);
    *c1 = (uint64_t)x;
    *c2 += d2 + (uint64_t)(x >> 64);
}

/* r = a^2 R^-1 mod m (a < m); r may alias a: as mmul, with the cross
 * products of a column summed once and doubled. */
static void msqr(const mctx *c, uint64_t *r, const uint64_t *a) {
    int nl = c->nl;
    const uint64_t *m = c->m;
    uint64_t u[MAXL], out[MAXL + 1];
    uint64_t c0 = 0, c1 = 0, c2 = 0;
    for (int i = 0; i < 2 * nl; i++) {
        int lo = i < nl ? 0 : i - nl + 1;
        uint64_t s0 = 0, s1 = 0, s2 = 0;
        for (int j = lo; j < i - j; j++) mac3(&s0, &s1, &s2, a[j], a[i - j]);
        add_twice(&c0, &c1, &c2, s0, s1, s2);
        if ((i & 1) == 0 && i / 2 < nl) mac3(&c0, &c1, &c2, a[i / 2], a[i / 2]);
        if (i < nl) {
            for (int j = 0; j < i; j++) mac3(&c0, &c1, &c2, u[j], m[i - j]);
            u[i] = c0 * c->m0;
            mac3(&c0, &c1, &c2, u[i], m[0]);
        } else {
            for (int j = i - nl + 1; j < nl; j++) mac3(&c0, &c1, &c2, u[j], m[i - j]);
            out[i - nl] = c0;
        }
        c0 = c1;
        c1 = c2;
        c2 = 0;
    }
    uint64_t d[MAXL];
    uint64_t borrow = bn_sub(d, out, m, nl);
    bn_cmov(out, d, nl, 0 - (c0 | (borrow ^ 1)));
    memcpy(r, out, (size_t)nl * 8);
}

/* r = T mod m for a double-length T (2 nl limbs, T < m R). */
static void mreduce_wide(const mctx *c, uint64_t *r, const uint64_t *T) {
    int nl = c->nl;
    uint64_t t[2 * MAXL + 1];
    memcpy(t, T, (size_t)2 * nl * 8);
    t[2 * nl] = 0;
    for (int i = 0; i < nl; i++) {
        uint64_t mm = t[i] * c->m0, carry = 0;
        for (int j = 0; j < nl; j++) {
            u128 x = (u128)mm * c->m[j] + t[i + j] + carry;
            t[i + j] = (uint64_t)x;
            carry = (uint64_t)(x >> 64);
        }
        for (int k = i + nl; k <= 2 * nl; k++) {   /* full length: no data-dependent exit */
            u128 x = (u128)t[k] + carry;
            t[k] = (uint64_t)x;
            carry = (uint64_t)(x >> 64);
        }
    }
    uint64_t *hi = t + nl, d[MAXL];
    uint64_t borrow = bn_sub(d, hi, c->m, nl);
    bn_cmov(hi, d, nl, 0 - (t[2 * nl] | (borrow ^ 1)));
    /* hi = T R^-1; multiply by R^2 to get T mod m */
    mmul(c, r, hi, c->rr);
}

/* r = a^e mod m with a public exponent (variable time). a < m, normal form. */
static void modexp_pub(const mctx *c, uint64_t *r, const uint64_t *a, const uint64_t *e, int el) {
    int nl = c->nl;
    uint64_t am[MAXL], acc[MAXL], one[MAXL] = { 1 };
    int top = bn_bits(e, el) - 1;
    if (top < 0) {   /* a^0 */
        memcpy(r, one, (size_t)nl * 8);
        return;
    }
    /* the top bit of e is set: start from a itself */
    mmul(c, am, a, c->rr);
    memcpy(acc, am, (size_t)nl * 8);
    for (int i = top - 1; i >= 0; i--) {
        msqr(c, acc, acc);
        if ((e[i / 64] >> (i % 64)) & 1) mmul(c, acc, acc, am);
    }
    mmul(c, r, acc, one);
}

/* r = a^e mod m in constant time for the el limbs of e (secret exponent). */
static void modexp_ct(const mctx *c, uint64_t *r, const uint64_t *a, const uint64_t *e, int el) {
    int nl = c->nl;
    static const int W = 4;
    uint64_t tab[16][MAXL], acc[MAXL], sel[MAXL], one[MAXL] = { 1 };
    mmul(c, tab[0], one, c->rr);
    mmul(c, tab[1], a, c->rr);
    for (int i = 2; i < 16; i++) mmul(c, tab[i], tab[i - 1], tab[1]);
    memcpy(acc, tab[0], (size_t)nl * 8);
    for (int w = el * 64 / W - 1; w >= 0; w--) {
        for (int k = 0; k < W; k++) msqr(c, acc, acc);
        unsigned nib = (unsigned)(e[(w * W) / 64] >> ((w * W) % 64)) & 15;
        memset(sel, 0, (size_t)nl * 8);
        for (unsigned i = 0; i < 16; i++) {
            uint64_t eq = ((uint64_t)(i ^ nib) - 1) >> 63;
            bn_cmov(sel, tab[i], nl, 0 - eq);
        }
        mmul(c, acc, acc, sel);
    }
    mmul(c, r, acc, one);
    zell__crypto_wipe(tab, sizeof tab);
    zell__crypto_wipe(sel, sizeof sel);
}

/* ---- encodings ---- */

static const uint8_t DI_SHA1[] = { 0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14 };
static const uint8_t DI_SHA256[] = { 0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01,
                                     0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20 };
static const uint8_t DI_SHA384[] = { 0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01,
                                     0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04, 0x30 };
static const uint8_t DI_SHA512[] = { 0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01,
                                     0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04, 0x40 };

/* EMSA-PKCS1-v1_5 encoding of a digest into em (k bytes). */
static bool emsa_pkcs1(uint8_t *em, size_t k, int alg, const uint8_t *digest, size_t dlen) {
    const uint8_t *di;
    size_t dil;
    switch (alg) {
    case ZELL_HASH_SHA1: di = DI_SHA1; dil = sizeof DI_SHA1; break;
    case ZELL_HASH_SHA256: di = DI_SHA256; dil = sizeof DI_SHA256; break;
    case ZELL_HASH_SHA384: di = DI_SHA384; dil = sizeof DI_SHA384; break;
    case ZELL_HASH_SHA512: di = DI_SHA512; dil = sizeof DI_SHA512; break;
    default: return false;
    }
    if (dlen != zell__hash_size(alg) || k < dil + dlen + 11) return false;
    em[0] = 0;
    em[1] = 1;
    size_t ps = k - 3 - dil - dlen;
    memset(em + 2, 0xff, ps);
    em[2 + ps] = 0;
    memcpy(em + 3 + ps, di, dil);
    memcpy(em + 3 + ps + dil, digest, dlen);
    return true;
}

static void mgf1_xor(int alg, uint8_t *out, size_t n, const uint8_t *seed, size_t slen) {
    size_t hl = zell__hash_size(alg);
    uint8_t ctr[4], h[64];
    for (uint32_t i = 0; n; i++) {
        zell__st32be(ctr, i);
        zell_hash_ctx c;
        zell__hash_init(&c, alg);
        zell__hash_update(&c, seed, slen);
        zell__hash_update(&c, ctr, 4);
        zell__hash_final(&c, h);
        size_t k = n < hl ? n : hl;
        for (size_t j = 0; j < k; j++) out[j] ^= h[j];
        out += k;
        n -= k;
    }
}

/* EMSA-PSS-ENCODE with sLen = hLen; em has emLen = ceil((modbits-1)/8) bytes. */
static bool emsa_pss_encode(uint8_t *em, size_t emlen, size_t embits, int alg, const uint8_t *digest,
                            size_t dlen) {
    size_t hl = zell__hash_size(alg);
    if (hl == 0 || dlen != hl || emlen < 2 * hl + 2) return false;
    uint8_t salt[64], m1[8 + 64 + 64] = { 0 }, H[64];
    if (!zell__crypto_random(salt, hl)) return false;
    memcpy(m1 + 8, digest, hl);
    memcpy(m1 + 8 + hl, salt, hl);
    zell__hash(alg, m1, 8 + 2 * hl, H);
    size_t dblen = emlen - hl - 1;
    memset(em, 0, dblen);
    em[dblen - hl - 1] = 1;
    memcpy(em + dblen - hl, salt, hl);
    mgf1_xor(alg, em, dblen, H, hl);
    em[0] &= (uint8_t)(0xff >> (8 * emlen - embits));
    memcpy(em + dblen, H, hl);
    em[emlen - 1] = 0xbc;
    return true;
}

static bool emsa_pss_verify(const uint8_t *emin, size_t emlen, size_t embits, int alg, const uint8_t *digest,
                            size_t dlen, int saltlen) {
    size_t hl = zell__hash_size(alg);
    if (hl == 0 || dlen != hl || emlen < hl + 2 || emlen > 1024) return false;
    uint8_t em[1024];
    memcpy(em, emin, emlen);
    if (em[emlen - 1] != 0xbc) return false;
    size_t dblen = emlen - hl - 1;
    const uint8_t *H = em + dblen;
    uint8_t topmask = (uint8_t)(0xff << (8 - (8 * emlen - embits)));
    if (8 * emlen - embits && (em[0] & topmask)) return false;
    mgf1_xor(alg, em, dblen, H, hl);
    em[0] &= (uint8_t)(0xff >> (8 * emlen - embits));
    size_t i = 0;
    while (i < dblen && em[i] == 0) i++;
    if (i == dblen || em[i] != 1) return false;
    size_t slen = dblen - i - 1;
    if (saltlen >= 0 && slen != (size_t)saltlen) return false;
    uint8_t m1[8 + 64 + 1024], h2[64];
    memset(m1, 0, 8);
    memcpy(m1 + 8, digest, hl);
    memcpy(m1 + 8 + hl, em + i + 1, slen);
    zell__hash(alg, m1, 8 + hl + slen, h2);
    return zell__crypto_ct_eq(h2, H, hl) == 1;
}

/* ---- public operations ---- */

typedef struct {
    mctx n;
    uint64_t e[MAXL];
    int el;
    size_t k;       /* modulus bytes */
    int bits;
} pubkey;

static bool pub_load(pubkey *pk, const uint8_t *n, size_t nlen, const uint8_t *e, size_t elen) {
    while (nlen && *n == 0) { n++; nlen--; }
    while (elen && *e == 0) { e++; elen--; }
    if (nlen < 64 || nlen > 1024 || elen == 0 || elen > 64) return false;   /* 512..8192 bits */
    int nl = (int)((nlen + 7) / 8);
    uint64_t m[MAXL];
    bn_from_be(m, nl, n, nlen);
    if (!mctx_init(&pk->n, m, nl)) return false;
    pk->el = (int)((elen + 7) / 8);
    bn_from_be(pk->e, pk->el, e, elen);
    pk->k = nlen;
    pk->bits = bn_bits(m, nl);
    return true;
}

/* m = s^e mod n as k bytes; false if s >= n. */
static bool pub_op(const pubkey *pk, const uint8_t *sig, size_t slen, uint8_t *out) {
    if (slen != pk->k) return false;
    int nl = pk->n.nl;
    uint64_t s[MAXL], t[MAXL], r[MAXL];
    bn_from_be(s, nl, sig, slen);
    if (!bn_sub(t, s, pk->n.m, nl)) return false;   /* s >= n */
    modexp_pub(&pk->n, r, s, pk->e, pk->el);
    bn_to_be(out, pk->k, r, nl);
    return true;
}

bool zell__rsa_public(const uint8_t *n, size_t nlen, const uint8_t *e, size_t elen, const uint8_t *in,
                       size_t ilen, uint8_t *out) {
    pubkey pk;
    return pub_load(&pk, n, nlen, e, elen) && pub_op(&pk, in, ilen, out);
}

bool zell__rsa_verify_pkcs1(const uint8_t *n, size_t nlen, const uint8_t *e, size_t elen, int alg,
                             const uint8_t *digest, size_t dlen, const uint8_t *sig, size_t slen) {
    pubkey pk;
    if (!pub_load(&pk, n, nlen, e, elen)) return false;
    uint8_t m[1024], want[1024];
    if (!pub_op(&pk, sig, slen, m)) return false;
    if (!emsa_pkcs1(want, pk.k, alg, digest, dlen)) return false;
    return zell__crypto_ct_eq(m, want, pk.k) == 1;
}

bool zell__rsa_verify_pss(const uint8_t *n, size_t nlen, const uint8_t *e, size_t elen, int alg,
                           const uint8_t *digest, size_t dlen, const uint8_t *sig, size_t slen, int saltlen) {
    pubkey pk;
    if (!pub_load(&pk, n, nlen, e, elen)) return false;
    uint8_t m[1024];
    if (!pub_op(&pk, sig, slen, m)) return false;
    size_t embits = (size_t)pk.bits - 1, emlen = (embits + 7) / 8;
    /* the integer must fit in emLen bytes */
    for (size_t i = 0; i < pk.k - emlen; i++)
        if (m[i]) return false;
    return emsa_pss_verify(m + (pk.k - emlen), emlen, embits, alg, digest, dlen, saltlen);
}

/* ---- PKCS#1 private key DER ---- */

typedef struct {
    const uint8_t *p;
    size_t len;
} span;

/* Reads one DER TLV of tag `tag` from *in; false on malformed input. */
static bool der_next(span *in, uint8_t tag, span *out) {
    if (in->len < 2 || in->p[0] != tag) return false;
    size_t len = in->p[1], hdr = 2;
    if (len & 0x80) {
        size_t nb = len & 0x7f;
        if (nb == 0 || nb > 3 || in->len < 2 + nb) return false;
        len = 0;
        for (size_t i = 0; i < nb; i++) len = len << 8 | in->p[2 + i];
        hdr = 2 + nb;
    }
    if (in->len - hdr < len) return false;
    out->p = in->p + hdr;
    out->len = len;
    in->p += hdr + len;
    in->len -= hdr + len;
    return true;
}

typedef struct {
    span n, e, d, p, q, dp, dq, qinv;
} privkey_der;

static bool priv_parse(const uint8_t *der, size_t derlen, privkey_der *k) {
    span all = { der, derlen }, seq, ver;
    if (!der_next(&all, 0x30, &seq)) return false;
    if (!der_next(&seq, 0x02, &ver) || ver.len != 1 || ver.p[0] != 0) return false;
    span *f[8] = { &k->n, &k->e, &k->d, &k->p, &k->q, &k->dp, &k->dq, &k->qinv };
    for (int i = 0; i < 8; i++) {
        if (!der_next(&seq, 0x02, f[i])) return false;
        while (f[i]->len > 1 && f[i]->p[0] == 0) { f[i]->p++; f[i]->len--; }
    }
    return true;
}

size_t zell__rsa_priv_size(const uint8_t *der, size_t derlen) {
    privkey_der k;
    if (!priv_parse(der, derlen, &k)) return 0;
    return k.n.len;
}

/* s = m^d mod n by CRT with blinding and verification; in/out are k bytes. */
static bool priv_op(const uint8_t *der, size_t derlen, const uint8_t *in, uint8_t *out, size_t *klen) {
    privkey_der kd;
    if (!priv_parse(der, derlen, &kd)) return false;
    pubkey pk;
    if (!pub_load(&pk, kd.n.p, kd.n.len, kd.e.p, kd.e.len)) return false;
    size_t k = pk.k;
    int hl = (int)((kd.p.len > kd.q.len ? kd.p.len : kd.q.len) + 7) / 8;
    if (hl > MAXL / 2 - 2) return false;
    uint64_t pp[MAXL] = { 0 }, qq[MAXL] = { 0 };
    if (!bn_from_be(pp, hl, kd.p.p, kd.p.len) || !bn_from_be(qq, hl, kd.q.p, kd.q.len)) return false;
    mctx cp, cq;
    if (!mctx_init(&cp, pp, hl) || !mctx_init(&cq, qq, hl)) return false;
    uint64_t c[MAXL] = { 0 };
    bn_from_be(c, 2 * hl, in, k);
    {
        uint64_t t[MAXL];
        uint64_t nn[MAXL] = { 0 };
        memcpy(nn, pk.n.m, (size_t)pk.n.nl * 8);
        if (!bn_sub(t, c, nn, 2 * hl)) return false;   /* input >= n */
    }
    /* blinded exponents dp + r (p - 1), dq + r' (q - 1), hl + 1 limbs */
    uint64_t r2[2];
    if (!zell__crypto_random((uint8_t *)r2, sizeof r2)) return false;
    uint64_t dpb[MAXL] = { 0 }, dqb[MAXL] = { 0 };
    if (!bn_from_be(dpb, hl + 1, kd.dp.p, kd.dp.len) || !bn_from_be(dqb, hl + 1, kd.dq.p, kd.dq.len)) return false;
    for (int which = 0; which < 2; which++) {
        uint64_t *db = which ? dqb : dpb, *pr = which ? qq : pp, rnd = r2[which] | 1;
        uint64_t pm1[MAXL];
        memcpy(pm1, pr, (size_t)hl * 8);
        pm1[0] &= ~(uint64_t)1;   /* p odd: p - 1 */
        uint64_t carry = 0;
        for (int i = 0; i <= hl; i++) {
            u128 x = (u128)(i < hl ? pm1[i] : 0) * rnd + db[i] + carry;
            db[i] = (uint64_t)x;
            carry = (uint64_t)(x >> 64);
        }
    }
    uint64_t cmp[MAXL], cmq[MAXL], m1[MAXL], m2[MAXL];
    mreduce_wide(&cp, cmp, c);
    mreduce_wide(&cq, cmq, c);
    modexp_ct(&cp, m1, cmp, dpb, hl + 1);
    modexp_ct(&cq, m2, cmq, dqb, hl + 1);
    /* h = qinv (m1 - m2) mod p */
    uint64_t wide[2 * MAXL] = { 0 }, m2p[MAXL] = { 0 }, diff[MAXL] = { 0 }, qinv[MAXL] = { 0 }, h[MAXL] = { 0 }, tmp[MAXL] = { 0 };
    memcpy(wide, m2, (size_t)hl * 8);
    mreduce_wide(&cp, m2p, wide);
    uint64_t borrow = bn_sub(diff, m1, m2p, hl);
    bn_add(tmp, diff, pp, hl);
    bn_cmov(diff, tmp, hl, 0 - borrow);
    if (!bn_from_be(qinv, hl, kd.qinv.p, kd.qinv.len)) return false;
    mmul(&cp, tmp, qinv, diff);
    mmul(&cp, h, tmp, cp.rr);
    /* s = m2 + h q */
    uint64_t s[2 * MAXL + 1] = { 0 };
    for (int i = 0; i < hl; i++) {
        uint64_t carry = 0;
        for (int j = 0; j < hl; j++) {
            u128 x = (u128)h[i] * qq[j] + s[i + j] + carry;
            s[i + j] = (uint64_t)x;
            carry = (uint64_t)(x >> 64);
        }
        s[i + hl] = carry;
    }
    uint64_t carry = 0;
    for (int i = 0; i < 2 * hl + 1; i++) {
        u128 x = (u128)s[i] + (i < hl ? m2[i] : 0) + carry;
        s[i] = (uint64_t)x;
        carry = (uint64_t)(x >> 64);
    }
    bn_to_be(out, k, s, 2 * hl + 1);
    /* verify: s^e mod n == input */
    uint8_t chk[1024];
    if (!pub_op(&pk, out, k, chk) || !zell__crypto_ct_eq(chk, in, k)) {
        memset(out, 0, k);
        return false;
    }
    zell__crypto_wipe(dpb, sizeof dpb);
    zell__crypto_wipe(dqb, sizeof dqb);
    zell__crypto_wipe(m1, sizeof m1);
    zell__crypto_wipe(m2, sizeof m2);
    zell__crypto_wipe(h, sizeof h);
    *klen = k;
    return true;
}

bool zell__rsa_sign_pkcs1(const uint8_t *der, size_t derlen, int alg, const uint8_t *digest, size_t dlen,
                           uint8_t *sig, size_t *siglen) {
    size_t k = zell__rsa_priv_size(der, derlen);
    uint8_t em[1024];
    if (k == 0 || k > 1024 || !emsa_pkcs1(em, k, alg, digest, dlen)) return false;
    return priv_op(der, derlen, em, sig, siglen);
}

bool zell__rsa_sign_pss(const uint8_t *der, size_t derlen, int alg, const uint8_t *digest, size_t dlen,
                         uint8_t *sig, size_t *siglen) {
    privkey_der kd;
    if (!priv_parse(der, derlen, &kd) || kd.n.len > 1024 || kd.n.len < 64) return false;
    uint64_t nb[MAXL];
    int nl = (int)((kd.n.len + 7) / 8);
    bn_from_be(nb, nl, kd.n.p, kd.n.len);
    size_t bits = (size_t)bn_bits(nb, nl), k = kd.n.len;
    size_t embits = bits - 1, emlen = (embits + 7) / 8;
    uint8_t em[1024] = { 0 };
    if (!emsa_pss_encode(em + (k - emlen), emlen, embits, alg, digest, dlen)) return false;
    return priv_op(der, derlen, em, sig, siglen);
}

/* ---- RSAES-OAEP (RFC 8017 7.1), MGF1 with the same hash ---- */

bool zell__rsa_encrypt_oaep(const uint8_t *n, size_t nlen, const uint8_t *e, size_t elen, int alg,
                             const uint8_t *msg, size_t mlen, const uint8_t *label, size_t llen,
                             uint8_t *out, size_t *olen) {
    pubkey pk;
    size_t hl = zell__hash_size(alg);
    if (!hl || !pub_load(&pk, n, nlen, e, elen)) return false;
    size_t k = pk.k;
    if (k < 2 * hl + 2 || mlen > k - 2 * hl - 2) return false;
    uint8_t em[1024];
    memset(em, 0, k);
    uint8_t *seed = em + 1, *db = em + 1 + hl;
    size_t dblen = k - hl - 1;
    zell__hash(alg, label, llen, db);
    db[dblen - mlen - 1] = 1;
    memcpy(db + dblen - mlen, msg, mlen);
    if (!zell__crypto_random(seed, hl)) return false;
    mgf1_xor(alg, db, dblen, seed, hl);
    mgf1_xor(alg, seed, hl, db, dblen);
    bool ok = pub_op(&pk, em, k, out);
    zell__crypto_wipe(em, sizeof em);
    if (ok) *olen = k;
    return ok;
}

bool zell__rsa_decrypt_oaep(const uint8_t *der, size_t derlen, int alg, const uint8_t *ct, size_t clen,
                             const uint8_t *label, size_t llen, uint8_t *out, size_t *mlen) {
    size_t hl = zell__hash_size(alg);
    size_t k = zell__rsa_priv_size(der, derlen);
    if (!hl || k == 0 || k > 1024 || clen != k || k < 2 * hl + 2) return false;
    uint8_t em[1024], lh[64];
    size_t klen = 0;
    if (!priv_op(der, derlen, ct, em, &klen)) return false;
    uint8_t *seed = em + 1, *db = em + 1 + hl;
    size_t dblen = k - hl - 1;
    mgf1_xor(alg, seed, hl, db, dblen);
    mgf1_xor(alg, db, dblen, seed, hl);
    zell__hash(alg, label, llen, lh);
    /* constant-time checks: em[0] == 0, lHash matches, PS zeros then 0x01 */
    uint32_t good = (uint32_t)zell__crypto_ct_eq(db, lh, hl);
    good &= (uint32_t)(((uint32_t)em[0] - 1) >> 31) & 1;   /* em[0] == 0 */
    uint32_t looking = 1, index = 0, invalid = 0;
    for (size_t i = hl; i < dblen; i++) {
        uint32_t b = db[i];
        uint32_t is0 = ((b - 1) >> 31) & 1, is1 = (((b ^ 1) - 1) >> 31) & 1;
        uint32_t take = looking & is1;
        index = (index & ~(0u - take)) | ((uint32_t)i & (0u - take));
        looking &= ~is1 & 1;
        invalid |= looking & ~is0 & 1;
    }
    good &= ~looking & 1;
    good &= ~invalid & 1;
    bool ok = good == 1;
    if (ok) {
        size_t start = (size_t)index + 1;
        *mlen = dblen - start;
        memcpy(out, db + start, *mlen);
    }
    zell__crypto_wipe(em, sizeof em);
    return ok;
}

/* ---- prime generation ---- */

static const uint16_t SMALL_PRIMES[] = {
    3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97, 101, 103, 107, 109,
    113, 127, 131, 137, 139, 149, 151, 157, 163, 167, 173, 179, 181, 191, 193, 197, 199, 211, 223, 227, 229, 233, 239,
    241, 251, 257, 263, 269, 271, 277, 281, 283, 293, 307, 311, 313, 317, 331, 337, 347, 349, 353, 359, 367, 373, 379,
    383, 389, 397, 401, 409, 419, 421, 431, 433, 439, 443, 449, 457, 461, 463, 467, 479, 487, 491, 499, 503, 509, 521,
    523, 541, 547, 557, 563, 569, 571, 577, 587, 593, 599, 601, 607, 613, 617, 619, 631, 641, 643, 647, 653, 659, 661,
    673, 677, 683, 691, 701, 709, 719, 727, 733, 739, 743, 751, 757, 761, 769, 773, 787, 797, 809, 811, 821, 823, 827,
    829, 839, 853, 857, 859, 863, 877, 881, 883, 887, 907, 911, 919, 929, 937, 941, 947, 953, 967, 971, 977, 983, 991, 997,
};

static uint64_t bn_mod_small(const uint64_t *a, int nl, uint64_t m) {
    u128 r = 0;
    for (int i = nl - 1; i >= 0; i--) r = ((r << 64) | a[i]) % m;
    return (uint64_t)r;
}

static bool miller_rabin(const uint64_t *nn, int nl, int rounds) {
    mctx c;
    if (!mctx_init(&c, nn, nl)) return false;
    uint64_t nm1[MAXL], d[MAXL], one[MAXL] = { 1 };
    bn_sub(nm1, nn, one, nl);
    memcpy(d, nm1, (size_t)nl * 8);
    int s = 0;
    while (!(d[0] & 1)) {
        for (int i = 0; i < nl; i++) d[i] = (d[i] >> 1) | (i + 1 < nl ? d[i + 1] << 63 : 0);
        s++;
    }
    uint64_t mone[MAXL], mnm1[MAXL];
    mmul(&c, mone, one, c.rr);
    mmul(&c, mnm1, nm1, c.rr);
    for (int r = 0; r < rounds; r++) {
        uint64_t a[MAXL] = { 0 }, t[MAXL], x[MAXL];
        /* random base in [2, n-2]: random nl-limb value reduced below n-3 */
        do {
            if (!zell__crypto_random((uint8_t *)a, (size_t)nl * 8)) return false;
            int top = bn_bits(nn, nl) - 1;
            a[top / 64] &= ((uint64_t)1 << (top % 64)) - 1;   /* a < 2^top <= n - 1 */
            if (top % 64 == 0) a[top / 64] = 0;
        } while (bn_bits(a, nl) < 2);
        modexp_pub(&c, x, a, d, nl);
        mmul(&c, t, x, c.rr);   /* Montgomery form */
        if (memcmp(t, mone, (size_t)nl * 8) == 0 || memcmp(t, mnm1, (size_t)nl * 8) == 0) continue;
        bool comp = true;
        for (int i = 1; i < s; i++) {
            mmul(&c, t, t, t);
            if (memcmp(t, mnm1, (size_t)nl * 8) == 0) {
                comp = false;
                break;
            }
            if (memcmp(t, mone, (size_t)nl * 8) == 0) break;
        }
        if (comp) return false;
    }
    return true;
}

bool zell__rsa_is_probable_prime(const uint8_t *p, size_t plen, int rounds) {
    while (plen && *p == 0) { p++; plen--; }
    if (plen == 0 || plen > 512) return false;
    int nl = (int)((plen + 7) / 8);
    uint64_t a[MAXL] = { 0 };
    bn_from_be(a, nl, p, plen);
    if (nl == 1 && a[0] < 1000) {
        if (a[0] < 2) return false;
        if (a[0] == 2) return true;
        for (size_t i = 0; i < sizeof SMALL_PRIMES / sizeof SMALL_PRIMES[0]; i++)
            if (a[0] == SMALL_PRIMES[i]) return true;
        return false;
    }
    if (!(a[0] & 1)) return false;
    for (size_t i = 0; i < sizeof SMALL_PRIMES / sizeof SMALL_PRIMES[0]; i++)
        if (bn_mod_small(a, nl, SMALL_PRIMES[i]) == 0) return false;
    return miller_rabin(a, nl, rounds);
}

bool zell__rsa_gen_prime(uint8_t *out, int bits, uint64_t e) {
    if (bits < 64 || bits > 4096 || bits % 8 != 0) return false;
    size_t len = (size_t)bits / 8;
    int nl = (bits + 63) / 64;
    int rounds = bits >= 1024 ? 8 : bits >= 512 ? 12 : 20;
    for (int tries = 0; tries < 100000; tries++) {
        if (!zell__crypto_random(out, len)) return false;
        out[0] |= 0xC0;
        out[len - 1] |= 1;
        uint64_t a[MAXL] = { 0 };
        bn_from_be(a, nl, out, len);
        bool small = false;
        for (size_t i = 0; i < sizeof SMALL_PRIMES / sizeof SMALL_PRIMES[0] && !small; i++)
            if (bn_mod_small(a, nl, SMALL_PRIMES[i]) == 0) small = true;
        if (small) continue;
        if (e > 1 && bn_mod_small(a, nl, e) == 1) continue;
        if (miller_rabin(a, nl, rounds)) return true;
    }
    return false;
}
