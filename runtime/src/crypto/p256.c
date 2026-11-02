/* p256.c - NIST P-256 (secp256r1): ECDH (SEC 1 3.3.1) and ECDSA
 * (FIPS 186-4 6, deterministic nonces per RFC 6979 3.2, optionally hedged
 * with extra randomness as in RFC 6979 3.6).
 *
 * Arithmetic: 4 x 64-bit limbs, Montgomery multiplication (CIOS) modulo
 * either the field prime p or the group order n, with constant-time final
 * subtraction; additions/subtractions use masks, never branches. Points are
 * projective (X:Y:Z) and combined with the complete addition formula of
 * Renes-Costello-Batina 2016 (algorithm 4, a = -3), which has no special
 * cases (doubling and the point at infinity included), so the 4-bit
 * fixed-window scalar multiplication with a constant-time table scan does
 * not branch on secret data.
 *
 * Field arithmetic modulo p has its own multiplication, squaring and
 * reduction (p = 2^256 - 2^224 + 2^192 + 2^96 - 1: -p^-1 mod 2^64 = 1 and
 * the third limb is zero) and inverts with a fixed addition chain; the
 * group order uses the generic CIOS code and a 4-bit window for inverses.
 *
 * Verification handles only public data, so it is not constant time: u1*G
 * adds the signed radix-16 digits of u1 from the P256_BASE table with
 * mixed Jacobian additions (no doublings), u2*Q runs a width-5 wNAF over
 * Jacobian coordinates (dbl-2001-b, add-2007-bl, a = -3), and the result
 * is compared with r projectively (X == r Z^2, also r + n) instead of being
 * converted to affine. */
#include "crypto.h"

#include <string.h>

typedef unsigned __int128 u128;
typedef uint64_t limb4[4];

typedef struct {
    limb4 m;        /* modulus */
    uint64_t m0;    /* -m^-1 mod 2^64 */
    limb4 rr;       /* R^2 mod m, R = 2^256 */
    limb4 one;      /* R mod m (1 in Montgomery form) */
} mont;

static const limb4 P_M = { 0xffffffffffffffffULL, 0x00000000ffffffffULL, 0, 0xffffffff00000001ULL };
static const limb4 N_M = { 0xf3b9cac2fc632551ULL, 0xbce6faada7179e84ULL, 0xffffffffffffffffULL, 0xffffffff00000000ULL };
/* b, G and the Montgomery constants live in p256_table.h (generated from the
 * SEC 2 values with this file's own arithmetic). */

static void l4_from_be(limb4 r, const uint8_t b[32]) {
    for (int i = 0; i < 4; i++) r[i] = zell__ld64be(b + 24 - 8 * i);
}
static void l4_to_be(uint8_t b[32], const limb4 a) {
    for (int i = 0; i < 4; i++) zell__st64be(b + 24 - 8 * i, a[i]);
}
static void l4_copy(limb4 r, const limb4 a) { for (int i = 0; i < 4; i++) r[i] = a[i]; }

/* r = a - b; returns the borrow (0 or 1). */
static uint64_t l4_sub(limb4 r, const limb4 a, const limb4 b) {
    uint64_t borrow = 0;
    for (int i = 0; i < 4; i++) {
        u128 t = (u128)a[i] - b[i] - borrow;
        r[i] = (uint64_t)t;
        borrow = (uint64_t)(t >> 64) & 1;
    }
    return borrow;
}
static uint64_t l4_add(limb4 r, const limb4 a, const limb4 b) {
    uint64_t carry = 0;
    for (int i = 0; i < 4; i++) {
        u128 t = (u128)a[i] + b[i] + carry;
        r[i] = (uint64_t)t;
        carry = (uint64_t)(t >> 64);
    }
    return carry;
}
/* r = (mask ? a : r) with mask all-ones or zero. */
static void l4_cmov(limb4 r, const limb4 a, uint64_t mask) {
    for (int i = 0; i < 4; i++) r[i] ^= (r[i] ^ a[i]) & mask;
}
static uint64_t l4_is_zero(const limb4 a) {
    uint64_t x = a[0] | a[1] | a[2] | a[3];
    return 1 & ~((x | (0 - x)) >> 63);   /* 1 if zero */
}
/* 1 if a < m (constant time). */
static uint64_t l4_lt(const limb4 a, const limb4 m) {
    limb4 t;
    return l4_sub(t, a, m);
}

/* r = a * b * R^-1 mod m for a, b < m. */
static void mont_mul(const mont *c, limb4 r, const limb4 a, const limb4 b) {
    uint64_t t[6] = { 0 };
    for (int i = 0; i < 4; i++) {
        uint64_t carry = 0;
        for (int j = 0; j < 4; j++) {
            u128 x = (u128)a[j] * b[i] + t[j] + carry;
            t[j] = (uint64_t)x;
            carry = (uint64_t)(x >> 64);
        }
        u128 x = (u128)t[4] + carry;
        t[4] = (uint64_t)x;
        t[5] = (uint64_t)(x >> 64);
        uint64_t mm = t[0] * c->m0;
        x = (u128)mm * c->m[0] + t[0];
        carry = (uint64_t)(x >> 64);
        for (int j = 1; j < 4; j++) {
            x = (u128)mm * c->m[j] + t[j] + carry;
            t[j - 1] = (uint64_t)x;
            carry = (uint64_t)(x >> 64);
        }
        x = (u128)t[4] + carry;
        t[3] = (uint64_t)x;
        t[4] = t[5] + (uint64_t)(x >> 64);
    }
    limb4 res = { t[0], t[1], t[2], t[3] }, d;
    uint64_t borrow = l4_sub(d, res, c->m);
    /* subtract if t >= m, i.e. carry word set or no borrow */
    l4_cmov(res, d, 0 - (t[4] | (borrow ^ 1)));
    l4_copy(r, res);
}

static void mont_add(const mont *c, limb4 r, const limb4 a, const limb4 b) {
    limb4 s, d;
    uint64_t carry = l4_add(s, a, b);
    uint64_t borrow = l4_sub(d, s, c->m);
    l4_cmov(s, d, 0 - (carry | (borrow ^ 1)));
    l4_copy(r, s);
}

static void mont_sub(const mont *c, limb4 r, const limb4 a, const limb4 b) {
    limb4 d, e;
    uint64_t borrow = l4_sub(d, a, b);
    l4_add(e, d, c->m);
    l4_cmov(d, e, 0 - borrow);
    l4_copy(r, d);
}

static void mont_to(const mont *c, limb4 r, const limb4 a) { mont_mul(c, r, a, c->rr); }
static void mont_from(const mont *c, limb4 r, const limb4 a) {
    limb4 one = { 1, 0, 0, 0 };
    mont_mul(c, r, a, one);
}

/* ---- arithmetic modulo p ---- */

/* r = t * R^-1 mod p for a 512-bit t < p * R (t is clobbered). Round i
 * adds m * p * 2^(64 i) with m = t[i] (-p^-1 = 1 mod 2^64), which clears
 * word i; the limbs of p are 2^64 - 1, 2^32 - 1, 0 and 2^64 - 2^32 + 1, so
 * the products are shifts: m (2^64 - 1) + m = m 2^64, m (2^32 - 1) + m =
 * m 2^32 (words i + 1, i + 2), m (2^64 - 2^32 + 1) = m 2^64 - m 2^32 + m
 * (words i + 3, i + 4). The carry out of word i + 4 is kept for the next
 * round (CIOS); the last one is the 2^256 bit of a result below 2p. */
static void fp_reduce(limb4 r, uint64_t t[8]) {
    uint64_t c = 0;
    for (int i = 0; i < 4; i++) {
        uint64_t m = t[i];
        u128 x = (u128)t[i + 1] + ((u128)m << 32);
        t[i + 1] = (uint64_t)x;
        x = (u128)t[i + 2] + (uint64_t)(x >> 64);
        t[i + 2] = (uint64_t)x;
        u128 mp3 = ((u128)m << 64) - ((u128)m << 32) + m;
        x = (u128)t[i + 3] + (uint64_t)mp3 + (uint64_t)(x >> 64);
        t[i + 3] = (uint64_t)x;
        x = (u128)t[i + 4] + (uint64_t)(mp3 >> 64) + (uint64_t)(x >> 64) + c;
        t[i + 4] = (uint64_t)x;
        c = (uint64_t)(x >> 64);
    }
    limb4 res = { t[4], t[5], t[6], t[7] }, d;
    uint64_t borrow = l4_sub(d, res, P_M);
    l4_cmov(res, d, 0 - (c | (borrow ^ 1)));
    l4_copy(r, res);
}

/* r = a * b * R^-1 mod p for a, b < p: CIOS in registers, each row adding
 * a * b[i] and then m * p (m = the low word, shifts as in fp_reduce) while
 * dropping the cleared low word. */
static void fp_mul(limb4 r, const limb4 a, const limb4 b) {
    uint64_t t0 = 0, t1 = 0, t2 = 0, t3 = 0, t4 = 0;
    for (int i = 0; i < 4; i++) {
        uint64_t bi = b[i];
        u128 x = (u128)a[0] * bi + t0;
        t0 = (uint64_t)x;
        x = (u128)a[1] * bi + t1 + (uint64_t)(x >> 64);
        t1 = (uint64_t)x;
        x = (u128)a[2] * bi + t2 + (uint64_t)(x >> 64);
        t2 = (uint64_t)x;
        x = (u128)a[3] * bi + t3 + (uint64_t)(x >> 64);
        t3 = (uint64_t)x;
        x = (u128)t4 + (uint64_t)(x >> 64);
        t4 = (uint64_t)x;
        uint64_t t5 = (uint64_t)(x >> 64);
        uint64_t m = t0;
        x = (u128)t1 + ((u128)m << 32);
        t0 = (uint64_t)x;
        x = (u128)t2 + (uint64_t)(x >> 64);
        t1 = (uint64_t)x;
        u128 mp3 = ((u128)m << 64) - ((u128)m << 32) + m;
        x = (u128)t3 + (uint64_t)mp3 + (uint64_t)(x >> 64);
        t2 = (uint64_t)x;
        x = (u128)t4 + (uint64_t)(mp3 >> 64) + (uint64_t)(x >> 64);
        t3 = (uint64_t)x;
        t4 = t5 + (uint64_t)(x >> 64);
    }
    limb4 res = { t0, t1, t2, t3 }, d;
    uint64_t borrow = l4_sub(d, res, P_M);
    l4_cmov(res, d, 0 - (t4 | (borrow ^ 1)));
    l4_copy(r, res);
}

/* r = a^2 * R^-1 mod p: the six cross products once, doubled, plus the
 * squares of the limbs. */
static void fp_sqr(limb4 r, const limb4 a) {
    uint64_t t[8] = { 0 };
    for (int i = 0; i < 3; i++) {
        uint64_t carry = 0;
        for (int j = i + 1; j < 4; j++) {
            u128 x = (u128)a[i] * a[j] + t[i + j] + carry;
            t[i + j] = (uint64_t)x;
            carry = (uint64_t)(x >> 64);
        }
        t[i + 4] = carry;
    }
    for (int k = 7; k > 0; k--) t[k] = (t[k] << 1) | (t[k - 1] >> 63);
    t[0] <<= 1;
    uint64_t carry = 0;
    for (int i = 0; i < 4; i++) {
        u128 sq = (u128)a[i] * a[i];
        u128 x = (u128)t[2 * i] + (uint64_t)sq + carry;
        t[2 * i] = (uint64_t)x;
        x = (u128)t[2 * i + 1] + (uint64_t)(sq >> 64) + (uint64_t)(x >> 64);
        t[2 * i + 1] = (uint64_t)x;
        carry = (uint64_t)(x >> 64);
    }
    fp_reduce(r, t);
}

static void fp_sqr_n(limb4 r, const limb4 a, int n) {
    fp_sqr(r, a);
    for (int i = 1; i < n; i++) fp_sqr(r, r);
}

/* r = a^(p-2) = a^-1 mod p (Montgomery form in and out). p - 2 is, from
 * the top: 32 ones, 31 zeros, a one, 96 zeros, 94 ones, a zero, a one;
 * x_k = a^(2^k - 1) builds the runs: 255 squarings, 12 multiplications. */
static void fp_inv(limb4 r, const limb4 a) {
    limb4 x2, x4, x8, x16, x32, t;
    fp_sqr(t, a);
    fp_mul(x2, t, a);
    fp_sqr_n(t, x2, 2);
    fp_mul(x4, t, x2);
    fp_sqr_n(t, x4, 4);
    fp_mul(x8, t, x4);
    fp_sqr_n(t, x8, 8);
    fp_mul(x16, t, x8);
    fp_sqr_n(t, x16, 16);
    fp_mul(x32, t, x16);
    fp_sqr_n(t, x32, 32);
    fp_mul(t, t, a);
    fp_sqr_n(t, t, 128);
    fp_mul(t, t, x32);
    fp_sqr_n(t, t, 32);
    fp_mul(t, t, x32);
    fp_sqr_n(t, t, 16);
    fp_mul(t, t, x16);
    fp_sqr_n(t, t, 8);
    fp_mul(t, t, x8);
    fp_sqr_n(t, t, 4);
    fp_mul(t, t, x4);
    fp_sqr_n(t, t, 2);
    fp_mul(t, t, x2);
    fp_sqr_n(t, t, 2);
    fp_mul(r, t, a);
}

/* r = a^(m-2) for the generic modulus (the group order): a fixed 4-bit
 * window over the public exponent, 256 squarings and 64 + 14
 * multiplications, the same sequence for every a. */
static void mont_inv_win(const mont *c, limb4 r, const limb4 a) {
    limb4 e, two = { 2, 0, 0, 0 }, tab[16], acc;
    l4_sub(e, c->m, two);
    l4_copy(tab[0], c->one);
    l4_copy(tab[1], a);
    for (int i = 2; i < 16; i++) mont_mul(c, tab[i], tab[i - 1], a);
    l4_copy(acc, c->one);
    for (int w = 63; w >= 0; w--) {
        for (int k = 0; k < 4; k++) mont_mul(c, acc, acc, acc);
        unsigned d = (unsigned)(e[w / 16] >> (4 * (w % 16))) & 15;
        mont_mul(c, acc, acc, tab[d]);
    }
    l4_copy(r, acc);
    zell__crypto_wipe(tab, sizeof tab);
}

/* ---- curve ---- */

typedef struct {
    limb4 X, Y, Z;
} pt;

typedef struct {
    mont fp, fn;
    limb4 b;      /* Montgomery form */
    pt g;
} curve;

#include "p256_table.h"

/* Complete addition, RCB 2016 algorithm 4 (a = -3). */
static void pt_add(const curve *k, pt *r, const pt *p, const pt *q) {
    const mont *f = &k->fp;
    limb4 t0, t1, t2, t3, t4, X3, Y3, Z3;
    fp_mul(t0, p->X, q->X);
    fp_mul(t1, p->Y, q->Y);
    fp_mul(t2, p->Z, q->Z);
    mont_add(f, t3, p->X, p->Y);
    mont_add(f, t4, q->X, q->Y);
    fp_mul(t3, t3, t4);
    mont_add(f, t4, t0, t1);
    mont_sub(f, t3, t3, t4);
    mont_add(f, t4, p->Y, p->Z);
    mont_add(f, X3, q->Y, q->Z);
    fp_mul(t4, t4, X3);
    mont_add(f, X3, t1, t2);
    mont_sub(f, t4, t4, X3);
    mont_add(f, X3, p->X, p->Z);
    mont_add(f, Y3, q->X, q->Z);
    fp_mul(X3, X3, Y3);
    mont_add(f, Y3, t0, t2);
    mont_sub(f, Y3, X3, Y3);
    fp_mul(Z3, k->b, t2);
    mont_sub(f, X3, Y3, Z3);
    mont_add(f, Z3, X3, X3);
    mont_add(f, X3, X3, Z3);
    mont_sub(f, Z3, t1, X3);
    mont_add(f, X3, t1, X3);
    fp_mul(Y3, k->b, Y3);
    mont_add(f, t1, t2, t2);
    mont_add(f, t2, t1, t2);
    mont_sub(f, Y3, Y3, t2);
    mont_sub(f, Y3, Y3, t0);
    mont_add(f, t1, Y3, Y3);
    mont_add(f, Y3, t1, Y3);
    mont_add(f, t1, t0, t0);
    mont_add(f, t0, t1, t0);
    mont_sub(f, t0, t0, t2);
    fp_mul(t1, t4, Y3);
    fp_mul(t2, t0, Y3);
    fp_mul(Y3, X3, Z3);
    mont_add(f, Y3, Y3, t2);
    fp_mul(X3, t3, X3);
    mont_sub(f, X3, X3, t1);
    fp_mul(Z3, t4, Z3);
    fp_mul(t1, t3, t0);
    mont_add(f, Z3, Z3, t1);
    l4_copy(r->X, X3);
    l4_copy(r->Y, Y3);
    l4_copy(r->Z, Z3);
}

/* Exception-free doubling, RCB 2016 algorithm 6 (a = -3). */
static void pt_dbl(const curve *k, pt *r, const pt *p) {
    const mont *f = &k->fp;
    limb4 t0, t1, t2, t3, X3, Y3, Z3;
    fp_sqr(t0, p->X);
    fp_sqr(t1, p->Y);
    fp_sqr(t2, p->Z);
    fp_mul(t3, p->X, p->Y);
    mont_add(f, t3, t3, t3);
    fp_mul(Z3, p->X, p->Z);
    mont_add(f, Z3, Z3, Z3);
    fp_mul(Y3, k->b, t2);
    mont_sub(f, Y3, Y3, Z3);
    mont_add(f, X3, Y3, Y3);
    mont_add(f, Y3, X3, Y3);
    mont_sub(f, X3, t1, Y3);
    mont_add(f, Y3, t1, Y3);
    fp_mul(Y3, X3, Y3);
    fp_mul(X3, X3, t3);
    mont_add(f, t3, t2, t2);
    mont_add(f, t2, t2, t3);
    fp_mul(Z3, k->b, Z3);
    mont_sub(f, Z3, Z3, t2);
    mont_sub(f, Z3, Z3, t0);
    mont_add(f, t3, Z3, Z3);
    mont_add(f, Z3, Z3, t3);
    mont_add(f, t3, t0, t0);
    mont_add(f, t0, t3, t0);
    mont_sub(f, t0, t0, t2);
    fp_mul(t0, t0, Z3);
    mont_add(f, Y3, Y3, t0);
    fp_mul(t0, p->Y, p->Z);
    mont_add(f, t0, t0, t0);
    fp_mul(Z3, t0, Z3);
    mont_sub(f, X3, X3, Z3);
    fp_mul(Z3, t0, t1);
    mont_add(f, Z3, Z3, Z3);
    mont_add(f, Z3, Z3, Z3);
    l4_copy(r->X, X3);
    l4_copy(r->Y, Y3);
    l4_copy(r->Z, Z3);
}

static void pt_infinity(const curve *k, pt *r) {
    memset(r, 0, sizeof *r);
    l4_copy(r->Y, k->fp.one);
}

static void pt_cmov(pt *r, const pt *a, uint64_t mask) {
    l4_cmov(r->X, a->X, mask);
    l4_cmov(r->Y, a->Y, mask);
    l4_cmov(r->Z, a->Z, mask);
}

/* r = [s]p, s a 32-byte big-endian scalar; constant time. */
static void pt_mul(const curve *k, pt *r, const uint8_t s[32], const pt *p) {
    pt tab[16], acc, t;
    pt_infinity(k, &tab[0]);
    tab[1] = *p;
    for (int i = 2; i < 16; i++) pt_add(k, &tab[i], &tab[i - 1], p);
    pt_infinity(k, &acc);
    for (int w = 0; w < 64; w++) {
        for (int d = 0; d < 4; d++) pt_dbl(k, &acc, &acc);
        unsigned nib = (s[w / 2] >> (w % 2 ? 0 : 4)) & 15;
        pt sel;
        memset(&sel, 0, sizeof sel);
        for (unsigned i = 0; i < 16; i++) {
            uint64_t eq = ((uint64_t)(i ^ nib) - 1) >> 63;   /* 1 iff i == nib */
            pt_cmov(&sel, &tab[i], 0 - eq);
        }
        pt_add(k, &t, &acc, &sel);
        acc = t;
    }
    *r = acc;
    zell__crypto_wipe(tab, sizeof tab);
}

/* r = [s]G, s a 32-byte big-endian scalar; constant time. The scalar is
 * recoded into 65 signed radix-16 digits in [-8, 8]; every digit scans all
 * eight affine entries of its P256_BASE row with conditional moves, the
 * selected point is conditionally negated and added (complete formulas,
 * so an accumulator at infinity is fine), and for a zero digit the
 * accumulator is moved back. */
static void pt_mul_base(const curve *k, pt *r, const uint8_t s[32]) {
    int8_t e[65];
    for (int i = 0; i < 32; i++) {
        e[2 * i] = (int8_t)(s[31 - i] & 15);
        e[2 * i + 1] = (int8_t)((s[31 - i] >> 4) & 15);
    }
    int8_t carry = 0;
    for (int i = 0; i < 64; i++) {
        e[i] = (int8_t)(e[i] + carry);
        carry = (int8_t)((e[i] + 8) >> 4);
        e[i] = (int8_t)(e[i] - (int8_t)(carry << 4));
    }
    e[64] = carry;
    pt acc, sel, t;
    pt_infinity(k, &acc);
    l4_copy(sel.Z, k->fp.one);
    for (int j = 0; j < 65; j++) {
        uint64_t neg = (uint64_t)((uint8_t)e[j] >> 7);
        uint64_t abs = (uint64_t)(uint8_t)(e[j] - (((0 - (int8_t)neg) & e[j]) << 1));
        memset(sel.X, 0, sizeof sel.X);
        memset(sel.Y, 0, sizeof sel.Y);
        for (unsigned i = 0; i < 8; i++) {
            uint64_t eq = (((abs ^ (i + 1)) - 1) & ~(abs ^ (i + 1))) >> 63;   /* 1 iff abs == i + 1 */
            l4_cmov(sel.X, P256_BASE[j][i][0], 0 - eq);
            l4_cmov(sel.Y, P256_BASE[j][i][1], 0 - eq);
        }
        limb4 ny, zero = { 0, 0, 0, 0 };
        mont_sub(&k->fp, ny, zero, sel.Y);
        l4_cmov(sel.Y, ny, 0 - neg);
        pt_add(k, &t, &acc, &sel);
        uint64_t nonzero = ((abs - 1) >> 63) ^ 1;   /* 1 iff abs != 0 */
        pt_cmov(&acc, &t, 0 - nonzero);
    }
    *r = acc;
    zell__crypto_wipe(e, sizeof e);
    zell__crypto_wipe(&sel, sizeof sel);
    zell__crypto_wipe(&t, sizeof t);
}

/* Affine coordinates as big-endian bytes; false for the point at infinity. */
static bool pt_affine(const curve *k, const pt *p, uint8_t x[32], uint8_t y[32]) {
    limb4 zi, t;
    if (l4_is_zero(p->Z)) return false;
    fp_inv(zi, p->Z);
    fp_mul(t, p->X, zi);
    mont_from(&k->fp, t, t);
    l4_to_be(x, t);
    if (y) {
        fp_mul(t, p->Y, zi);
        mont_from(&k->fp, t, t);
        l4_to_be(y, t);
    }
    return true;
}

/* Decodes and validates an uncompressed point (on the curve, coordinates < p). */
static bool pt_decode(const curve *k, pt *r, const uint8_t in[65]) {
    if (in[0] != 4) return false;
    limb4 x, y;
    l4_from_be(x, in + 1);
    l4_from_be(y, in + 33);
    if (!l4_lt(x, P_M) || !l4_lt(y, P_M)) return false;
    const mont *f = &k->fp;
    mont_to(f, r->X, x);
    mont_to(f, r->Y, y);
    l4_copy(r->Z, f->one);
    /* y^2 == x^3 - 3x + b */
    limb4 lhs, rhs, t;
    fp_mul(lhs, r->Y, r->Y);
    fp_mul(rhs, r->X, r->X);
    fp_mul(rhs, rhs, r->X);
    mont_add(f, t, r->X, r->X);
    mont_add(f, t, t, r->X);
    mont_sub(f, rhs, rhs, t);
    mont_add(f, rhs, rhs, k->b);
    limb4 d;
    l4_sub(d, lhs, rhs);
    return l4_is_zero(d) == 1;
}

/* 1 <= s < n */
static bool scalar_ok(const uint8_t s[32]) {
    limb4 v;
    l4_from_be(v, s);
    return !l4_is_zero(v) && l4_lt(v, N_M);
}

bool zell__p256_pubkey(uint8_t pub[65], const uint8_t priv[32]) {
    if (!scalar_ok(priv)) return false;
    const curve k = P256;
    pt q;
    pt_mul_base(&k, &q, priv);
    pub[0] = 4;
    return pt_affine(&k, &q, pub + 1, pub + 33);
}

bool zell__p256_ecdh(uint8_t out[32], const uint8_t priv[32], const uint8_t peer[65]) {
    if (!scalar_ok(priv)) return false;
    const curve k = P256;
    pt p, q;
    if (!pt_decode(&k, &p, peer)) return false;
    pt_mul(&k, &q, priv, &p);
    return pt_affine(&k, &q, out, NULL);
}

bool zell__p256_genkey(uint8_t priv[32]) {
    for (int tries = 0; tries < 64; tries++) {
        if (!zell__crypto_random(priv, 32)) return false;
        if (scalar_ok(priv)) return true;
    }
    return false;
}

/* The leftmost 256 bits of a digest as an integer reduced modulo n. */
static void digest_to_scalar(limb4 e, const uint8_t *digest, size_t dlen) {
    uint8_t buf[32] = { 0 };
    if (dlen >= 32) memcpy(buf, digest, 32);
    else memcpy(buf + 32 - dlen, digest, dlen);
    l4_from_be(e, buf);
    limb4 d;
    uint64_t borrow = l4_sub(d, e, N_M);
    l4_cmov(e, d, 0 - (borrow ^ 1));
}

static void hmac2(uint8_t out[32], const uint8_t key[32], const uint8_t *a, size_t an, const uint8_t *b,
                  size_t bn, const uint8_t *c, size_t cn, const uint8_t *d, size_t dn, const uint8_t *e, size_t en) {
    zell_hmac_ctx h;
    zell__hmac_init(&h, ZELL_HASH_SHA256, key, 32);
    zell__hmac_update(&h, a, an);
    zell__hmac_update(&h, b, bn);
    zell__hmac_update(&h, c, cn);
    zell__hmac_update(&h, d, dn);
    zell__hmac_update(&h, e, en);
    zell__hmac_final(&h, out);
    zell__crypto_wipe(&h, sizeof h);
}

bool zell__p256_sign(uint8_t sig[64], const uint8_t priv[32], const uint8_t *digest, size_t dlen,
                      const uint8_t *extra, size_t elen) {
    if (!scalar_ok(priv)) return false;
    const curve k = P256;
    const mont *fn = &k.fn;
    limb4 e, d;
    digest_to_scalar(e, digest, dlen);
    l4_from_be(d, priv);
    uint8_t h1[32];
    l4_to_be(h1, e);
    /* RFC 6979 3.2 (HMAC-SHA256), extra data per 3.6 */
    uint8_t V[32], K[32], z = 0, one = 1;
    memset(V, 1, 32);
    memset(K, 0, 32);
    if (!extra) elen = 0;
    hmac2(K, K, V, 32, &z, 1, priv, 32, h1, 32, extra, elen);
    hmac2(V, K, V, 32, NULL, 0, NULL, 0, NULL, 0, NULL, 0);
    hmac2(K, K, V, 32, &one, 1, priv, 32, h1, 32, extra, elen);
    hmac2(V, K, V, 32, NULL, 0, NULL, 0, NULL, 0, NULL, 0);
    bool ok = false;
    for (int tries = 0; tries < 64 && !ok; tries++) {
        hmac2(V, K, V, 32, NULL, 0, NULL, 0, NULL, 0, NULL, 0);
        if (scalar_ok(V)) {
            pt R;
            pt_mul_base(&k, &R, V);
            uint8_t rx[32];
            if (pt_affine(&k, &R, rx, NULL)) {
                limb4 r, rd, kk, ki, s, t;
                l4_from_be(r, rx);
                uint64_t borrow = l4_sub(rd, r, N_M);
                l4_cmov(r, rd, 0 - (borrow ^ 1));
                if (!l4_is_zero(r)) {
                    /* s = k^-1 (e + r d) mod n, all in Montgomery form */
                    l4_from_be(kk, V);
                    mont_to(fn, kk, kk);
                    mont_inv_win(fn, ki, kk);
                    limb4 rm, dm, em;
                    mont_to(fn, rm, r);
                    mont_to(fn, dm, d);
                    mont_to(fn, em, e);
                    mont_mul(fn, t, rm, dm);
                    mont_add(fn, t, t, em);
                    mont_mul(fn, s, ki, t);
                    mont_from(fn, s, s);
                    if (!l4_is_zero(s)) {
                        l4_to_be(sig, r);
                        l4_to_be(sig + 32, s);
                        ok = true;
                    }
                    zell__crypto_wipe(kk, sizeof kk);
                    zell__crypto_wipe(ki, sizeof ki);
                    zell__crypto_wipe(dm, sizeof dm);
                }
            }
        }
        if (!ok) {
            hmac2(K, K, V, 32, &z, 1, NULL, 0, NULL, 0, NULL, 0);
            hmac2(V, K, V, 32, NULL, 0, NULL, 0, NULL, 0, NULL, 0);
        }
    }
    zell__crypto_wipe(V, sizeof V);
    zell__crypto_wipe(K, sizeof K);
    zell__crypto_wipe(d, sizeof d);
    return ok;
}

/* ---- verification (public data, variable time) ---- */

/* Jacobian (X, Y, Z): x = X/Z^2, y = Y/Z^3; Z == 0 is the point at infinity. */
typedef pt jpt;

static bool fp_is_zero(const limb4 a) { return l4_is_zero(a) == 1; }

/* dbl-2001-b (a = -3): 3M + 5S. */
static void jdbl(jpt *r, const jpt *p) {
    if (fp_is_zero(p->Z)) {
        *r = *p;
        return;
    }
    limb4 delta, gamma, beta, alpha, t0, t1, X3, Y3, Z3;
    fp_sqr(delta, p->Z);
    fp_sqr(gamma, p->Y);
    fp_mul(beta, p->X, gamma);
    mont_sub(&P256.fp, t0, p->X, delta);
    mont_add(&P256.fp, t1, p->X, delta);
    fp_mul(alpha, t0, t1);
    mont_add(&P256.fp, t0, alpha, alpha);
    mont_add(&P256.fp, alpha, t0, alpha);
    fp_sqr(X3, alpha);
    mont_add(&P256.fp, t0, beta, beta);
    mont_add(&P256.fp, t0, t0, t0);
    mont_add(&P256.fp, t1, t0, t0);
    mont_sub(&P256.fp, X3, X3, t1);
    mont_add(&P256.fp, t1, p->Y, p->Z);
    fp_sqr(Z3, t1);
    mont_sub(&P256.fp, Z3, Z3, gamma);
    mont_sub(&P256.fp, Z3, Z3, delta);
    mont_sub(&P256.fp, t0, t0, X3);
    fp_mul(Y3, alpha, t0);
    fp_sqr(t1, gamma);
    mont_add(&P256.fp, t1, t1, t1);
    mont_add(&P256.fp, t1, t1, t1);
    mont_add(&P256.fp, t1, t1, t1);
    mont_sub(&P256.fp, Y3, Y3, t1);
    l4_copy(r->X, X3);
    l4_copy(r->Y, Y3);
    l4_copy(r->Z, Z3);
}

/* add-2007-bl, or madd-2007-bl when q is affine (qz1: q->Z is one). */
static void jadd(jpt *r, const jpt *p, const jpt *q, bool qz1) {
    const mont *f = &P256.fp;
    if (fp_is_zero(p->Z)) {
        *r = *q;
        return;
    }
    if (fp_is_zero(q->Z)) {
        *r = *p;
        return;
    }
    limb4 z1z1, z2z2, u1, u2, s1, s2, h, rr, t;
    fp_sqr(z1z1, p->Z);
    fp_mul(u2, q->X, z1z1);
    fp_mul(t, p->Z, z1z1);
    fp_mul(s2, q->Y, t);
    if (qz1) {
        l4_copy(u1, p->X);
        l4_copy(s1, p->Y);
    } else {
        fp_sqr(z2z2, q->Z);
        fp_mul(u1, p->X, z2z2);
        fp_mul(t, q->Z, z2z2);
        fp_mul(s1, p->Y, t);
    }
    mont_sub(f, h, u2, u1);
    mont_sub(f, rr, s2, s1);
    if (fp_is_zero(h)) {
        if (fp_is_zero(rr)) {
            jdbl(r, p);
        } else {
            memset(r, 0, sizeof *r);
            l4_copy(r->X, f->one);
            l4_copy(r->Y, f->one);
        }
        return;
    }
    mont_add(f, rr, rr, rr);
    limb4 i, j, v, X3, Y3, Z3;
    mont_add(f, t, h, h);
    fp_sqr(i, t);
    fp_mul(j, h, i);
    fp_mul(v, u1, i);
    fp_sqr(X3, rr);
    mont_sub(f, X3, X3, j);
    mont_sub(f, X3, X3, v);
    mont_sub(f, X3, X3, v);
    mont_sub(f, t, v, X3);
    fp_mul(Y3, rr, t);
    fp_mul(t, s1, j);
    mont_add(f, t, t, t);
    mont_sub(f, Y3, Y3, t);
    if (qz1) {
        mont_add(f, t, p->Z, h);
        fp_sqr(Z3, t);
        mont_sub(f, Z3, Z3, z1z1);
        fp_sqr(t, h);
        mont_sub(f, Z3, Z3, t);
    } else {
        mont_add(f, t, p->Z, q->Z);
        fp_sqr(Z3, t);
        mont_sub(f, Z3, Z3, z1z1);
        mont_sub(f, Z3, Z3, z2z2);
        fp_mul(Z3, Z3, h);
    }
    l4_copy(r->X, X3);
    l4_copy(r->Y, Y3);
    l4_copy(r->Z, Z3);
}

static void jneg(jpt *r, const jpt *p) {
    limb4 zero = { 0, 0, 0, 0 };
    *r = *p;
    mont_sub(&P256.fp, r->Y, zero, p->Y);
}

/* Width-5 NAF of a 256-bit scalar (little-endian limbs): digits odd in
 * [-15, 15] or zero, at most one nonzero in any 5 consecutive; returns the
 * number of digits (<= 257). */
static int wnaf5(int8_t out[257], const limb4 k) {
    uint64_t v[5] = { k[0], k[1], k[2], k[3], 0 };
    int n = 0;
    while (v[0] | v[1] | v[2] | v[3] | v[4]) {
        int8_t d = 0;
        if (v[0] & 1) {
            int m = (int)(v[0] & 31);
            d = (int8_t)(m >= 16 ? m - 32 : m);
            /* v -= d */
            if (d > 0) {
                uint64_t b = (uint64_t)d;
                for (int i = 0; i < 5 && b; i++) {
                    uint64_t old = v[i];
                    v[i] = old - b;
                    b = old < b;
                }
            } else {
                uint64_t c = (uint64_t)(-d);
                for (int i = 0; i < 5 && c; i++) {
                    v[i] += c;
                    c = v[i] < c;
                }
            }
        }
        out[n++] = d;
        for (int i = 0; i < 4; i++) v[i] = (v[i] >> 1) | (v[i + 1] << 63);
        v[4] >>= 1;
    }
    return n;
}

/* r = u1 G + u2 Q (variable time). */
static void verify_mul(jpt *r, const limb4 u1, const limb4 u2, const jpt *q) {
    const mont *f = &P256.fp;
    /* u2 Q: odd multiples Q, 3Q, ..., 15Q */
    jpt tab[8], q2;
    tab[0] = *q;
    jdbl(&q2, q);
    for (int i = 1; i < 8; i++) jadd(&tab[i], &tab[i - 1], &q2, false);
    int8_t naf[257];
    int n = wnaf5(naf, u2);
    jpt acc;
    memset(&acc, 0, sizeof acc);
    for (int i = n - 1; i >= 0; i--) {
        jdbl(&acc, &acc);
        int8_t d = naf[i];
        if (d > 0) {
            jadd(&acc, &acc, &tab[d >> 1], false);
        } else if (d < 0) {
            jpt neg;
            jneg(&neg, &tab[(-d) >> 1]);
            jadd(&acc, &acc, &neg, false);
        }
    }
    /* u1 G: signed radix-16 digits, digit j adds |e_j| 16^j G from the table */
    uint8_t s[32];
    l4_to_be(s, u1);
    int e[65];
    for (int i = 0; i < 32; i++) {
        e[2 * i] = s[31 - i] & 15;
        e[2 * i + 1] = (s[31 - i] >> 4) & 15;
    }
    int carry = 0;
    for (int i = 0; i < 64; i++) {
        e[i] += carry;
        carry = (e[i] + 8) >> 4;
        e[i] -= carry << 4;
    }
    e[64] = carry;
    jpt g;
    for (int j = 0; j < 65; j++) {
        if (e[j] == 0) continue;
        int a = e[j] < 0 ? -e[j] : e[j];
        l4_copy(g.X, P256_BASE[j][a - 1][0]);
        if (e[j] < 0) {
            limb4 zero = { 0, 0, 0, 0 };
            mont_sub(f, g.Y, zero, P256_BASE[j][a - 1][1]);
        } else {
            l4_copy(g.Y, P256_BASE[j][a - 1][1]);
        }
        l4_copy(g.Z, f->one);
        jadd(&acc, &acc, &g, true);
    }
    *r = acc;
}

bool zell__p256_verify(const uint8_t sig[64], const uint8_t pub[65], const uint8_t *digest, size_t dlen) {
    if (!scalar_ok(sig) || !scalar_ok(sig + 32)) return false;
    const curve k = P256;
    const mont *fn = &k.fn;
    pt Q;
    if (!pt_decode(&k, &Q, pub)) return false;
    limb4 e, r, s, w, u1, u2;
    digest_to_scalar(e, digest, dlen);
    l4_from_be(r, sig);
    l4_from_be(s, sig + 32);
    mont_to(fn, s, s);
    mont_inv_win(fn, w, s);
    limb4 em, rm;
    mont_to(fn, em, e);
    mont_to(fn, rm, r);
    mont_mul(fn, u1, em, w);
    mont_mul(fn, u2, rm, w);
    mont_from(fn, u1, u1);
    mont_from(fn, u2, u2);
    jpt X;
    verify_mul(&X, u1, u2, &Q);
    if (fp_is_zero(X.Z)) return false;
    /* x(X) mod n == r  <=>  X == r Z^2 or (r + n < p and X == (r + n) Z^2) */
    limb4 z2, cand, t;
    fp_sqr(z2, X.Z);
    mont_to(&k.fp, cand, r);
    fp_mul(t, cand, z2);
    limb4 diff;
    l4_sub(diff, t, X.X);
    if (l4_is_zero(diff)) return true;
    limb4 rn;
    uint64_t c = l4_add(rn, r, N_M);
    if (c || !l4_lt(rn, P_M)) return false;
    mont_to(&k.fp, cand, rn);
    fp_mul(t, cand, z2);
    l4_sub(diff, t, X.X);
    return l4_is_zero(diff) == 1;
}
