// decimal64 (BID encoding, IEEE 754-2008): arithmetic, text and double conversions.
#include "Decimal.h"

#include <cfloat>
#include <cmath>
#include <cstring>

#if defined(_MSC_VER)
#include <intrin.h>
#endif

// Native 128-bit helpers where the compiler has them, plain C++ elsewhere.
// DECIMAL_PORTABLE_128 forces the plain C++ form.
#if !defined(DECIMAL_PORTABLE_128) && defined(__SIZEOF_INT128__)
#define DEC_NATIVE_INT128 1
#elif !defined(DECIMAL_PORTABLE_128) && defined(_MSC_VER) && defined(_M_X64) && !defined(_M_ARM64EC)
#define DEC_MSVC_X64 1
#endif

// Set when double arithmetic is done in double precision (no wider registers).
#if defined(_M_X64) || defined(_M_ARM64) || defined(__x86_64__) || defined(__aarch64__) \
    || defined(__SSE2_MATH__) || (defined(_M_IX86_FP) && _M_IX86_FP >= 2) \
    || (defined(__FLT_EVAL_METHOD__) && __FLT_EVAL_METHOD__ == 0)
#define DEC_EXACT_DOUBLE 1
#endif

namespace {

typedef std::uint64_t u64;
typedef std::uint32_t u32;
typedef std::int32_t i32;

const u64 SIGN_BIT = 0x8000000000000000ULL;
const u64 INF_BITS = 0x7800000000000000ULL;
const u64 NAN_BITS = 0x7c00000000000000ULL;
const u64 SNAN_BITS = 0x7e00000000000000ULL;
const u64 ZERO_BITS = 0x31c0000000000000ULL;
const u64 TEN15 = 1000000000000000ULL;
const u64 TEN16 = 10000000000000000ULL;
const int EXP_BIAS = 398;
const int EXP_MAX = 767; // largest biased exponent

const u64 DBL_INF_BITS = 0x7ff0000000000000ULL;
const u64 TWO52 = 1ULL << 52;

const u64 POW10[20] = {
    1ULL, 10ULL, 100ULL, 1000ULL, 10000ULL, 100000ULL, 1000000ULL, 10000000ULL, 100000000ULL,
    1000000000ULL, 10000000000ULL, 100000000000ULL, 1000000000000ULL, 10000000000000ULL,
    100000000000000ULL, 1000000000000000ULL, 10000000000000000ULL, 100000000000000000ULL,
    1000000000000000000ULL, 10000000000000000000ULL};

const u64 POW5[28] = {
    1ULL, 5ULL, 25ULL, 125ULL, 625ULL, 3125ULL, 15625ULL, 78125ULL, 390625ULL, 1953125ULL,
    9765625ULL, 48828125ULL, 244140625ULL, 1220703125ULL, 6103515625ULL, 30517578125ULL,
    152587890625ULL, 762939453125ULL, 3814697265625ULL, 19073486328125ULL, 95367431640625ULL,
    476837158203125ULL, 2384185791015625ULL, 11920928955078125ULL, 59604644775390625ULL,
    298023223876953125ULL, 1490116119384765625ULL, 7450580596923828125ULL};

#if defined(DEC_EXACT_DOUBLE)
const double POW10D[23] = {1e0,  1e1,  1e2,  1e3,  1e4,  1e5,  1e6,  1e7,  1e8,  1e9,  1e10, 1e11,
                           1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22};
#endif

// ---- 64 and 128-bit helpers ----

// Number of bits needed to write v (0 for 0).
inline int bitLength(u64 v) {
#if defined(__GNUC__) || defined(__clang__)
    return v ? 64 - __builtin_clzll(v) : 0;
#elif defined(_MSC_VER) && (defined(_M_X64) || defined(_M_ARM64))
    unsigned long i;
    return _BitScanReverse64(&i, v) ? static_cast<int>(i) + 1 : 0;
#else
    int n = 0;
    if (v >> 32) { n += 32; v >>= 32; }
    if (v >> 16) { n += 16; v >>= 16; }
    if (v >> 8) { n += 8; v >>= 8; }
    if (v >> 4) { n += 4; v >>= 4; }
    if (v >> 2) { n += 2; v >>= 2; }
    if (v >> 1) { n += 1; v >>= 1; }
    return n + static_cast<int>(v);
#endif
}

// Number of decimal digits of v (0 for 0).
inline int digits64(u64 v) {
    int n = (bitLength(v) * 1233) >> 12;
    return n + ((n < 20 && v >= POW10[n]) ? 1 : 0);
}

struct U128 {
    u64 hi;
    u64 lo;
};

#if defined(DEC_NATIVE_INT128)
__extension__ typedef unsigned __int128 NativeU128;
#endif

inline U128 mul64(u64 a, u64 b) {
    U128 r;
#if defined(DEC_NATIVE_INT128)
    const NativeU128 p = static_cast<NativeU128>(a) * b;
    r.hi = static_cast<u64>(p >> 64);
    r.lo = static_cast<u64>(p);
#elif defined(DEC_MSVC_X64)
    r.lo = _umul128(a, b, &r.hi);
#else
    const u64 a0 = a & 0xffffffffULL, a1 = a >> 32;
    const u64 b0 = b & 0xffffffffULL, b1 = b >> 32;
    const u64 p00 = a0 * b0, p01 = a0 * b1, p10 = a1 * b0, p11 = a1 * b1;
    const u64 mid = (p00 >> 32) + (p01 & 0xffffffffULL) + (p10 & 0xffffffffULL);
    r.lo = (mid << 32) | (p00 & 0xffffffffULL);
    r.hi = p11 + (p01 >> 32) + (p10 >> 32) + (mid >> 32);
#endif
    return r;
}

// (hi:lo) / d with hi < d, so the quotient fits in 64 bits.
inline u64 divWide(u64 hi, u64 lo, u64 d, u64* rem) {
#if defined(DEC_NATIVE_INT128)
    const NativeU128 n = (static_cast<NativeU128>(hi) << 64) | lo;
    *rem = static_cast<u64>(n % d);
    return static_cast<u64>(n / d);
#elif defined(DEC_MSVC_X64)
    return _udiv128(hi, lo, d, rem);
#else
    // Schoolbook division in base 2^32 on a normalised divisor.
    const u64 base = 1ULL << 32;
    const int s = 64 - bitLength(d);
    d <<= s;
    const u64 d1 = d >> 32, d0 = d & 0xffffffffULL;
    const u64 n32 = s ? ((hi << s) | (lo >> (64 - s))) : hi;
    const u64 n10 = lo << s;
    const u64 n1 = n10 >> 32, n0 = n10 & 0xffffffffULL;
    u64 q1 = n32 / d1, rhat = n32 - q1 * d1;
    while (q1 >= base || q1 * d0 > base * rhat + n1) {
        --q1;
        rhat += d1;
        if (rhat >= base) break;
    }
    const u64 n21 = n32 * base + n1 - q1 * d;
    u64 q0 = n21 / d1;
    rhat = n21 - q0 * d1;
    while (q0 >= base || q0 * d0 > base * rhat + n0) {
        --q0;
        rhat += d1;
        if (rhat >= base) break;
    }
    *rem = (n21 * base + n0 - q0 * d) >> s;
    return q1 * base + q0;
#endif
}

inline U128 divmod128(U128 n, u64 d, u64* rem) {
    U128 q;
    q.hi = n.hi / d;
    q.lo = divWide(n.hi % d, n.lo, d, rem);
    return q;
}

inline bool lessThan(U128 a, U128 b) { return a.hi < b.hi || (a.hi == b.hi && a.lo < b.lo); }

// Number of decimal digits of c (0 for 0).
inline int digits128(U128 c) {
    if (c.hi == 0) return digits64(c.lo);
    int n = 20;
    while (n <= 38 && !lessThan(c, mul64(POW10[19], POW10[n - 19]))) ++n;
    return n;
}

// ---- encoding ----

inline bool isNan(u64 x) { return (x & NAN_BITS) == NAN_BITS; }
inline bool isInf(u64 x) { return (x & NAN_BITS) == INF_BITS; }
inline bool isSpecial(u64 x) { return (x & INF_BITS) == INF_BITS; }

struct Num {
    u64 coeff;
    int exp; // biased
    bool neg;
};

// A finite value. A coefficient above 16 digits reads as zero.
inline Num unpack(u64 x) {
    Num n;
    n.neg = (x >> 63) != 0;
    if ((x & 0x6000000000000000ULL) == 0x6000000000000000ULL) {
        n.exp = static_cast<int>((x >> 51) & 0x3ff);
        n.coeff = (x & 0x0007ffffffffffffULL) | 0x0020000000000000ULL;
        if (n.coeff >= TEN16) n.coeff = 0;
    } else {
        n.exp = static_cast<int>((x >> 53) & 0x3ff);
        n.coeff = x & 0x001fffffffffffffULL;
    }
    return n;
}

// coeff < 10^16, 0 <= exp <= EXP_MAX.
inline u64 pack(bool neg, int exp, u64 coeff) {
    const u64 s = neg ? SIGN_BIT : 0;
    if (coeff < (1ULL << 53)) return s | (static_cast<u64>(exp) << 53) | coeff;
    return s | 0x6000000000000000ULL | (static_cast<u64>(exp) << 51) | (coeff & 0x0007ffffffffffffULL);
}

inline u64 packZero(bool neg, int exp) {
    return pack(neg, exp < 0 ? 0 : (exp > EXP_MAX ? EXP_MAX : exp), 0);
}

inline u64 packInf(bool neg) { return neg ? (SIGN_BIT | INF_BITS) : INF_BITS; }

// The quiet NaN an operation returns for a NaN operand: sign and payload kept,
// a payload above 15 digits dropped.
inline u64 quietNan(u64 x) {
    u64 payload = x & 0x0003ffffffffffffULL;
    if (payload >= TEN15) payload = 0;
    return (x & 0xfc00000000000000ULL) | payload;
}

// Encodes c * 10^(exp - bias), plus a nonzero amount below one unit of c when
// `sticky`, rounded half to even to 16 digits. `exp` may be out of range:
// a value too small loses digits, a value too large becomes infinity.
u64 roundAndPack(bool neg, U128 c, int exp, bool sticky) {
    if (c.hi == 0 && c.lo < TEN16 && exp >= 0 && exp <= EXP_MAX) return pack(neg, exp, c.lo);
    if (c.hi == 0 && c.lo == 0) return packZero(neg, exp);
    const int n = digits128(c);
    int drop = n > 16 ? n - 16 : 0;
    if (exp + drop < 0) drop = -exp;
    u64 q = c.lo;
    if (drop > 0) {
        if (drop > n) return pack(neg, 0, 0); // less than half of the smallest unit
        exp += drop;
        u64 r;
        while (drop > 19) {
            const int k = drop - 19 < 19 ? drop - 19 : 19;
            c = divmod128(c, POW10[k], &r);
            sticky = sticky || r != 0;
            drop -= k;
        }
        q = divmod128(c, POW10[drop], &r).lo;
        const u64 half = POW10[drop] / 2;
        if (r > half || (r == half && (sticky || (q & 1)))) ++q;
        if (q == TEN16) {
            q = TEN15;
            ++exp;
        }
    }
    if (exp > EXP_MAX) {
        while (exp > EXP_MAX && q < TEN15) {
            q *= 10;
            --exp;
        }
        if (exp > EXP_MAX) return packInf(neg);
    }
    return pack(neg, exp, q);
}

// ---- arithmetic ----

u64 addBits(u64 x, u64 y) {
    if (isSpecial(x) || isSpecial(y)) {
        if (isNan(x)) return quietNan(x);
        if (isNan(y)) return quietNan(y);
        if (isInf(x)) {
            if (isInf(y) && ((x ^ y) & SIGN_BIT)) return NAN_BITS;
            return x & (SIGN_BIT | INF_BITS);
        }
        return y & (SIGN_BIT | INF_BITS);
    }
    Num a = unpack(x), b = unpack(y);
    if (a.exp < b.exp) {
        const Num t = a;
        a = b;
        b = t;
    }
    // a has the larger exponent; the sum is counted in units of b when possible.
    const int diff = a.coeff ? a.exp - b.exp : 0;
    if (diff <= 22) {
        U128 h;
        if (diff <= 3) {
            h.hi = 0;
            h.lo = a.coeff * POW10[diff];
        } else {
            h = mul64(a.coeff * 1000, POW10[diff - 3]);
        }
        U128 s;
        bool neg;
        if (a.neg == b.neg) {
            s.lo = h.lo + b.coeff;
            s.hi = h.hi + (s.lo < b.coeff ? 1 : 0);
            neg = a.neg;
        } else if (h.hi != 0 || h.lo >= b.coeff) {
            s.lo = h.lo - b.coeff;
            s.hi = h.hi - (h.lo < b.coeff ? 1 : 0);
            neg = (s.hi | s.lo) ? a.neg : false;
        } else {
            s.hi = 0;
            s.lo = b.coeff - h.lo;
            neg = b.neg;
        }
        return roundAndPack(neg, s, b.exp, false);
    }
    // b lies far below a: only its leading digits and whether more follow matter.
    const U128 h = mul64(a.coeff * 1000, POW10[19]);
    const int gap = diff - 22;
    u64 low = 0;
    bool sticky = b.coeff != 0;
    if (gap < 16) {
        low = b.coeff / POW10[gap];
        sticky = (b.coeff % POW10[gap]) != 0;
    }
    U128 s;
    if (a.neg == b.neg) {
        s.lo = h.lo + low;
        s.hi = h.hi + (s.lo < low ? 1 : 0);
    } else {
        if (sticky) ++low;
        s.lo = h.lo - low;
        s.hi = h.hi - (h.lo < low ? 1 : 0);
    }
    return roundAndPack(a.neg, s, a.exp - 22, sticky);
}

u64 subBits(u64 x, u64 y) { return addBits(x, isNan(y) ? y : (y ^ SIGN_BIT)); }

u64 mulBits(u64 x, u64 y) {
    const bool neg = ((x ^ y) & SIGN_BIT) != 0;
    if (isSpecial(x) || isSpecial(y)) {
        if (isNan(x)) return quietNan(x);
        if (isNan(y)) return quietNan(y);
        // one infinity at least: times zero has no value
        const u64 other = isInf(x) ? y : x;
        if (!isSpecial(other) && unpack(other).coeff == 0) return NAN_BITS;
        return packInf(neg);
    }
    const Num a = unpack(x), b = unpack(y);
    const int exp = a.exp + b.exp - EXP_BIAS;
    if (a.coeff == 0 || b.coeff == 0) return packZero(neg, exp);
    return roundAndPack(neg, mul64(a.coeff, b.coeff), exp, false);
}

u64 divBits(u64 x, u64 y) {
    const bool neg = ((x ^ y) & SIGN_BIT) != 0;
    if (isSpecial(x) || isSpecial(y)) {
        if (isNan(x)) return quietNan(x);
        if (isNan(y)) return quietNan(y);
        if (isInf(x)) return isInf(y) ? NAN_BITS : packInf(neg);
        return neg ? SIGN_BIT : 0; // finite / infinity: zero with the smallest exponent
    }
    const Num a = unpack(x), b = unpack(y);
    const int ideal = a.exp - b.exp + EXP_BIAS;
    if (a.coeff == 0) return b.coeff == 0 ? NAN_BITS : packZero(neg, ideal);
    if (b.coeff == 0) return packInf(neg);
    // 17 or 18 digits of quotient, so that rounding always has a digit to drop.
    const int na = digits64(a.coeff), nb = digits64(b.coeff);
    const int k = 17 + nb - na;
    const int k1 = k < 19 - na ? k : 19 - na;
    const U128 num = mul64(a.coeff * POW10[k1], POW10[k - k1]);
    u64 rem;
    u64 q = divWide(num.hi, num.lo, b.coeff, &rem);
    int exp = ideal - k;
    if (rem == 0) {
        // exact: the exponent goes as near the ideal one as the digits allow
        while (exp < ideal && q % 10 == 0) {
            q /= 10;
            ++exp;
        }
    }
    U128 c;
    c.hi = 0;
    c.lo = q;
    return roundAndPack(neg, c, exp, rem != 0);
}

// ---- wide integers for the double conversions ----

const int BIG_WORDS = 20;

// Little-endian words; words at and above n are zero.
struct Big {
    u64 w[BIG_WORDS];
    int n;
};

void bigSet(Big& b, u64 v) {
    std::memset(b.w, 0, sizeof b.w);
    b.w[0] = v;
    b.n = v ? 1 : 0;
}

void bigMulSmall(Big& b, u64 m) {
    u64 carry = 0;
    for (int i = 0; i < b.n; ++i) {
        U128 p = mul64(b.w[i], m);
        p.lo += carry;
        if (p.lo < carry) ++p.hi;
        b.w[i] = p.lo;
        carry = p.hi;
    }
    if (carry && b.n < BIG_WORDS) b.w[b.n++] = carry;
}

void bigMulPow5(Big& b, int k) {
    for (; k > 27; k -= 27) bigMulSmall(b, POW5[27]);
    if (k > 0) bigMulSmall(b, POW5[k]);
}

void bigShl(Big& b, int bits) {
    if (b.n == 0 || bits <= 0) return;
    const int words = bits / 64, rest = bits % 64;
    const int top = b.n + words;
    if (top >= BIG_WORDS) return; // beyond any value the conversions build
    if (rest) {
        b.w[top] = b.w[b.n - 1] >> (64 - rest);
        for (int i = b.n - 1; i > 0; --i) b.w[i + words] = (b.w[i] << rest) | (b.w[i - 1] >> (64 - rest));
        b.w[words] = b.w[0] << rest;
    } else {
        for (int i = b.n - 1; i >= 0; --i) b.w[i + words] = b.w[i];
    }
    for (int i = 0; i < words; ++i) b.w[i] = 0;
    b.n = top + 1;
    while (b.n > 0 && b.w[b.n - 1] == 0) --b.n;
}

int bigBitLength(const Big& b) { return b.n ? 64 * (b.n - 1) + bitLength(b.w[b.n - 1]) : 0; }

// The 64 bits starting at bit `pos`.
u64 bigBits(const Big& b, int pos) {
    const int word = pos / 64, off = pos % 64;
    const u64 lo = word < BIG_WORDS ? b.w[word] : 0;
    const u64 hi = word + 1 < BIG_WORDS ? b.w[word + 1] : 0;
    return off ? ((lo >> off) | (hi << (64 - off))) : lo;
}

bool bigAnyBelow(const Big& b, int pos) {
    const int word = pos / 64, off = pos % 64;
    for (int i = 0; i < word && i < b.n; ++i)
        if (b.w[i]) return true;
    return off != 0 && word < b.n && (b.w[word] & ((1ULL << off) - 1)) != 0;
}

int bigCmp(const Big& a, const Big& b) {
    if (a.n != b.n) return a.n < b.n ? -1 : 1;
    for (int i = a.n - 1; i >= 0; --i)
        if (a.w[i] != b.w[i]) return a.w[i] < b.w[i] ? -1 : 1;
    return 0;
}

// a -= b * q, with a >= b * q.
void bigSubMul(Big& a, const Big& b, u64 q) {
    u64 carry = 0, borrow = 0;
    for (int i = 0; i < a.n; ++i) {
        u64 sub = carry;
        carry = 0;
        if (i < b.n) {
            U128 p = mul64(b.w[i], q);
            p.lo += sub;
            if (p.lo < sub) ++p.hi;
            sub = p.lo;
            carry = p.hi;
        }
        const u64 v = a.w[i];
        const u64 t = v - sub;
        const u64 under = (v < sub ? 1u : 0u) + (t < borrow ? 1u : 0u);
        a.w[i] = t - borrow;
        borrow = under;
    }
    while (a.n > 0 && a.w[a.n - 1] == 0) --a.n;
}

// Returns floor(num / den) and leaves the remainder in num. The quotient must
// be below 2^64 and den nonzero.
u64 bigDiv(Big& num, const Big& den) {
    if (den.n == 1) {
        u64 rem;
        const u64 q = divWide(num.w[1], num.w[0], den.w[0], &rem);
        bigSet(num, rem);
        return q;
    }
    // Estimate from the leading 64 bits of den (rounded up, so never too
    // large), then correct with the true remainder.
    const int shift = bigBitLength(den) - 64;
    const u64 top = bigBits(den, shift);
    const u64 hi = bigBits(num, shift + 64), lo = bigBits(num, shift);
    u64 q = hi;
    if (top != ~0ULL) {
        u64 rem;
        q = divWide(hi, lo, top + 1, &rem);
    }
    bigSubMul(num, den, q);
    while (bigCmp(num, den) >= 0) {
        bigSubMul(num, den, 1);
        ++q;
    }
    return q;
}

inline u64 doubleBits(double d) {
    u64 bits;
    std::memcpy(&bits, &d, sizeof bits);
    return bits;
}

inline double bitsDouble(u64 bits) {
    double d;
    std::memcpy(&d, &bits, sizeof d);
    return d;
}

// floor(e * log10(2)) for the exponents of a double.
inline int floorLog10Pow2(int e) { return static_cast<int>(std::floor(e * 0.30102999566398120)); }

// ---- decimal to double ----

// Bits of the double nearest to (q + f) * 2^(e2 - 63), where q has its top
// bit set and 0 <= f < 1 is nonzero when `sticky`. Ties go to even.
u64 roundToDouble(u64 q, bool sticky, int e2) {
    if (e2 > 1023) return DBL_INF_BITS;
    if (e2 >= -1022) {
        u64 m = q >> 11;
        const u64 rest = q & 0x7ff;
        if (rest > 0x400 || (rest == 0x400 && (sticky || (m & 1)))) ++m;
        const u64 bits = (static_cast<u64>(e2 + 1023) << 52) + (m - TWO52);
        return bits > DBL_INF_BITS ? DBL_INF_BITS : bits;
    }
    const int keep = e2 + 1075; // bits left above the smallest subnormal
    if (keep < 0) return 0;
    if (keep == 0) return (q > SIGN_BIT || sticky) ? 1 : 0;
    const int cut = 64 - keep;
    u64 m = q >> cut;
    const u64 rest = q & ((1ULL << cut) - 1), half = 1ULL << (cut - 1);
    if (rest > half || (rest == half && (sticky || (m & 1)))) ++m;
    return m;
}

// Bits of the double nearest to c * 10^e10 (c nonzero, sign left out).
u64 decimalToBinary(u64 c, int e10) {
#if defined(DEC_EXACT_DOUBLE)
    // Both operands are exact doubles, so one operation rounds once.
    if (c <= (1ULL << 53) && e10 >= -22 && e10 <= 22) {
        const double d = static_cast<double>(static_cast<std::int64_t>(c));
        return doubleBits(e10 < 0 ? d / POW10D[-e10] : d * POW10D[e10]);
    }
#endif
    Big a;
    bigSet(a, c);
    u64 q;
    bool sticky;
    int e2;
    if (e10 >= 0) {
        bigMulPow5(a, e10);
        const int len = bigBitLength(a);
        if (len >= 64) {
            q = bigBits(a, len - 64);
            sticky = bigAnyBelow(a, len - 64);
        } else {
            q = a.w[0] << (64 - len);
            sticky = false;
        }
        e2 = len - 1 + e10;
    } else {
        Big den;
        bigSet(den, 1);
        bigMulPow5(den, -e10);
        const int shift = 63 + bigBitLength(den) - bitLength(c);
        bigShl(a, shift);
        q = bigDiv(a, den); // 63 or 64 bits
        sticky = a.n != 0;
        e2 = 63 - shift + e10;
        if (!(q >> 63)) {
            q <<= 1;
            --e2;
        }
    }
    return roundToDouble(q, sticky, e2);
}

double toDouble(u64 x) {
    const u64 sign = x & SIGN_BIT;
    if (isNan(x)) {
        u64 payload = x & 0x0003ffffffffffffULL;
        if (payload >= TEN15) payload = 0;
        return bitsDouble(sign | 0x7ff8000000000000ULL | (payload << 1));
    }
    if (isInf(x)) return bitsDouble(sign | DBL_INF_BITS);
    const Num n = unpack(x);
    if (n.coeff == 0) return bitsDouble(sign);
    return bitsDouble(sign | decimalToBinary(n.coeff, n.exp - EXP_BIAS));
}

// ---- double to decimal ----

// m * 2^e2 / 10^p as a ratio of wide integers, without the factor m.
struct Scale {
    Big num;
    Big den;
};

void scaleSet(Scale& s, int e2, int p) {
    bigSet(s.num, 1);
    bigSet(s.den, 1);
    if (p >= 0) bigMulPow5(s.den, p);
    else bigMulPow5(s.num, -p);
    const int b = e2 - p;
    if (b >= 0) bigShl(s.num, b);
    else bigShl(s.den, -b);
}

// floor(m * scale). `rest` is set when something is left; `half`, when given,
// tells whether the part left is below (-1), equal to (0) or above (1) a half.
u64 scaleDiv(const Scale& s, u64 m, bool* rest, int* half) {
    Big n = s.num;
    bigMulSmall(n, m);
    const u64 q = bigDiv(n, s.den);
    *rest = n.n != 0;
    if (half) {
        bigShl(n, 1);
        *half = bigCmp(n, s.den);
    }
    return q;
}

// Splits a finite nonzero double into m * 2^e2.
inline void splitDouble(u64 bits, u64* m, int* e2) {
    const int field = static_cast<int>((bits >> 52) & 0x7ff);
    const u64 frac = bits & (TWO52 - 1);
    if (field == 0) {
        *m = frac;
        *e2 = -1074;
    } else {
        *m = frac | TWO52;
        *e2 = field - 1075;
    }
}

// The decimal nearest to a double: the exact value when it has at most 16
// digits (a whole number keeps exponent 0), else 16 digits rounded.
u64 fromDouble(double d) {
    const u64 bits = doubleBits(d);
    const bool neg = (bits >> 63) != 0;
    const u64 mag = bits & ~SIGN_BIT;
    if (mag >= DBL_INF_BITS) {
        if (mag == DBL_INF_BITS) return packInf(neg);
        u64 payload = (mag & 0x0007ffffffffffffULL) >> 1;
        if (payload >= TEN15) payload = 0;
        return (bits & SIGN_BIT) | NAN_BITS | payload;
    }
    if (mag == 0) return pack(neg, EXP_BIAS, 0);
    u64 m;
    int e2;
    splitDouble(bits, &m, &e2);

    // exact forms
    const int zeros = bitLength(m & (0 - m)) - 1;
    const u64 odd = m >> zeros;
    const int eo = e2 + zeros;
    if (eo >= 0) {
        if (bitLength(odd) + eo <= 54 && (odd << eo) < TEN16) return pack(neg, EXP_BIAS, odd << eo);
    } else if (eo >= -22 && odd <= (TEN16 - 1) / POW5[-eo]) {
        return pack(neg, EXP_BIAS + eo, odd * POW5[-eo]);
    }

    // 16 digits, or 17 when the first guess of the exponent is one too low
    int p = floorLog10Pow2(bitLength(m) - 1 + e2) - 15;
    Scale s;
    scaleSet(s, e2, p);
    bool rest;
    int half;
    u64 q = scaleDiv(s, m, &rest, &half);
    if (q >= TEN16) {
        const u64 last = q % 10;
        q /= 10;
        ++p;
        if (last > 5 || (last == 5 && (rest || (q & 1)))) ++q;
    } else if (half > 0 || (half == 0 && (q & 1))) {
        ++q;
    }
    if (q == TEN16) {
        q = TEN15;
        ++p;
    }
    return pack(neg, EXP_BIAS + p, q);
}

// The decimal read from the plain text of `digits` followed by e10 zeros
// (e10 > 0) or with -e10 decimals: 16 digits kept, a 17th rounded half to even.
u64 fromDigits(bool neg, u64 digits, int e10) {
    const int n = digits64(digits);
    if (e10 > 0 && n <= 16) {
        if (n + e10 <= 16) return pack(neg, EXP_BIAS, digits * POW10[e10]);
        return pack(neg, EXP_BIAS + n + e10 - 16, digits * POW10[16 - n]);
    }
    if (n > 16) {
        const u64 last = digits % 10;
        digits /= 10;
        ++e10;
        if (last > 5 || (last == 5 && (digits & 1))) ++digits;
        if (digits == TEN16) {
            digits = TEN15;
            ++e10;
        }
    }
    return pack(neg, EXP_BIAS + e10, digits);
}

// The shortest digits that read back to m * 2^e2, nearest to it when several.
u64 shortest(bool neg, u64 m, int e2) {
    // The values that read back lie between the two neighbours' midpoints,
    // ends included when m is even. In units of 2^(e2 - 2):
    const u64 mid = 4 * m;
    const u64 low = mid - ((m == TWO52 && e2 > -1074) ? 1 : 2);
    const u64 high = mid + 2;
    const bool closed = (m & 1) == 0;

    // 17 or 18 digits of each
    const int p = floorLog10Pow2(bitLength(m) - 1 + e2) - 16;
    Scale s;
    scaleSet(s, e2 - 2, p);
    bool restL, restD, restH;
    int halfD;
    const u64 qL = scaleDiv(s, low, &restL, 0);
    const u64 qD = scaleDiv(s, mid, &restD, &halfD);
    const u64 qH = scaleDiv(s, high, &restH, 0);

    // Fewest digits first: drop j digits and try the decimal below and above.
    for (int j = 18; j >= 0; --j) {
        const u64 unit = POW10[j];
        const u64 below = qD / unit, part = qD % unit;
        u64 pick;
        if (part == 0 && !restD) {
            pick = below; // the double itself
        } else {
            const u64 floorL = qL / unit, floorH = qH / unit;
            const bool exactL = qL % unit == 0 && !restL;
            const bool exactH = qH % unit == 0 && !restH;
            const u64 above = below + 1;
            const bool okBelow = below > floorL || (exactL && closed);
            const bool okAbove = above < floorH || (above == floorH && (!exactH || closed));
            if (okBelow && okAbove) {
                int side = halfD;
                if (j > 0) {
                    const u64 halfUnit = unit / 2;
                    side = part > halfUnit ? 1 : (part < halfUnit ? -1 : (restD ? 1 : 0));
                }
                pick = side > 0 ? above : (side < 0 ? below : ((below & 1) ? above : below));
            } else if (okBelow) {
                pick = below;
            } else if (okAbove) {
                pick = above;
            } else {
                continue;
            }
        }
        if (pick == 0) continue;
        int e10 = p + j;
        while (pick % 10 == 0) {
            pick /= 10;
            ++e10;
        }
        return fromDigits(neg, pick, e10);
    }
    // not reached: 17 digits always read back
    return fromDigits(neg, qD >= POW10[17] ? qD / 10 : qD, qD >= POW10[17] ? p + 1 : p);
}

// ---- text ----

inline bool isDigit(char c) { return c >= '0' && c <= '9'; }
inline char lower(char c) { return (c >= 'A' && c <= 'Z') ? static_cast<char>(c - 'A' + 'a') : c; }

// Whether p starts with `word` (lower case), letter case ignored.
bool startsWith(const char* p, const char* word) {
    for (; *word; ++p, ++word)
        if (lower(*p) != *word) return false;
    return true;
}

// "inf" or "infinity" and nothing after.
bool isInfinityWord(const char* p) {
    if (!startsWith(p, "inf")) return false;
    return p[3] == 0 || (startsWith(p + 3, "inity") && p[8] == 0);
}

// Encodes a parsed number without an exponent part. An exponent below the
// range is not rounded here: the field simply wraps, as in the usual decimal
// floating point library (it takes more than 398 decimals to get there).
u64 packPlain(u64 sign, i32 exp, u64 coeff) {
    if (exp > EXP_MAX) {
        while (coeff < TEN15 && exp > EXP_MAX) {
            --exp;
            coeff *= 10;
        }
        if (exp > EXP_MAX) return sign | INF_BITS;
    }
    const u64 field = static_cast<u64>(static_cast<std::int64_t>(exp));
    if (coeff < (1ULL << 53)) return (field << 53) | coeff | sign;
    return (field << 51) | sign | 0x6000000000000000ULL | (coeff & 0x0007ffffffffffffULL);
}

u64 parse(const char* p) {
    while (*p == ' ' || *p == '\t') ++p;
    char c = *p;
    if (c != '.' && c != '-' && c != '+' && !isDigit(c)) {
        if (isInfinityWord(p)) return INF_BITS;
        return startsWith(p, "snan") ? SNAN_BITS : NAN_BITS;
    }
    // one character, whatever it is, may come before these words
    if (isInfinityWord(p + 1)) return c == '+' ? INF_BITS : (c == '-' ? (SIGN_BIT | INF_BITS) : NAN_BITS);
    if (startsWith(p + 1, "snan")) return c == '-' ? (SIGN_BIT | SNAN_BITS) : SNAN_BITS;

    const u64 sign = c == '-' ? SIGN_BIT : 0;
    if (c == '-' || c == '+') c = *++p;
    if (c != '.' && !isDigit(c)) return sign | NAN_BITS;

    // leading zeros; those after the point count in the exponent
    bool point = false;
    u32 zerosAfterPoint = 0;
    if (c == '0' || c == '.') {
        if (c == '.') {
            point = true;
            ++p;
        }
        while (*p == '0') {
            ++p;
            if (point) ++zerosAfterPoint;
            if (*p == '.') {
                if (point) return sign | NAN_BITS;
                point = true;
                if (p[1] == 0) return (ZERO_BITS - (static_cast<u64>(zerosAfterPoint) << 53)) | sign;
                ++p;
            } else if (*p == 0) {
                return (ZERO_BITS - (static_cast<u64>(zerosAfterPoint) << 53)) | sign;
            }
        }
    }

    // digits: 16 are kept, the 17th rounds, later ones only break a tie
    u64 coeff = 0;
    u32 digits = 0, decimals = 0, dropped = 0;
    bool inexact = false, roundedUp = false, tie = false;
    for (c = *p; isDigit(c) || c == '.'; c = *++p) {
        if (c == '.') {
            if (point) return sign | NAN_BITS;
            point = true;
            continue;
        }
        if (point) ++decimals;
        ++digits;
        if (digits <= 16) {
            coeff = coeff * 10 + static_cast<u64>(c - '0');
            continue;
        }
        if (digits == 17) {
            tie = c == '5' && !(coeff & 1);
            if (c > '5' || (c == '5' && (coeff & 1))) {
                ++coeff;
                roundedUp = true;
            }
            if (coeff == TEN16) {
                coeff = TEN15;
                dropped = 1;
            }
        } else if (tie && c > '0') {
            ++coeff;
            tie = false;
            roundedUp = true;
        }
        if (c > '0') inexact = true;
        ++dropped;
    }
    const u32 shift = dropped - (decimals + zerosAfterPoint);

    if (c == 0) return packPlain(sign, static_cast<i32>(shift + static_cast<u32>(EXP_BIAS)), coeff);
    if (c != 'E' && c != 'e') return sign | NAN_BITS;
    c = *++p;
    const bool negExp = c == '-';
    if (c == '-' || c == '+') c = *++p;
    if (!isDigit(c)) return sign | NAN_BITS;
    u32 e = 0;
    for (; isDigit(c); c = *++p)
        if (e < 100000000) e = e * 10 + static_cast<u32>(c - '0'); // far out of range already
    if (c != 0) return sign | NAN_BITS;
    const i32 exp = static_cast<i32>((negExp ? 0 - e : e) + shift + static_cast<u32>(EXP_BIAS));

    if (exp < 0) {
        // too small: digits are dropped from the truncated coefficient
        if (roundedUp) --coeff;
        if (exp + 16 < 0) return sign;
        const u64 unit = POW10[-exp];
        u64 q = coeff / unit;
        const u64 r = coeff % unit, half = unit / 2;
        if (r > half || (r == half && (inexact || (q & 1)))) ++q;
        return sign | q;
    }
    U128 c128;
    c128.hi = 0;
    c128.lo = coeff;
    return roundAndPack(sign != 0, c128, exp, false);
}

// Writes v in decimal at out, returns the number of characters.
int writeDigits(char* out, u64 v) {
    char tmp[20];
    int n = 0;
    do {
        tmp[n++] = static_cast<char>('0' + v % 10);
        v /= 10;
    } while (v);
    for (int i = 0; i < n; ++i) out[i] = tmp[n - 1 - i];
    return n;
}

} // namespace

Decimal DecimalFunctions::add(Decimal decimal1, Decimal decimal2) {
    return Decimal::fromBits(addBits(decimal1.bits(), decimal2.bits()));
}

Decimal DecimalFunctions::sub(Decimal decimal1, Decimal decimal2) {
    return Decimal::fromBits(subBits(decimal1.bits(), decimal2.bits()));
}

Decimal DecimalFunctions::mul(Decimal decimal1, Decimal decimal2) {
    return Decimal::fromBits(mulBits(decimal1.bits(), decimal2.bits()));
}

Decimal DecimalFunctions::div(Decimal decimal1, Decimal decimal2) {
    return Decimal::fromBits(divBits(decimal1.bits(), decimal2.bits()));
}

double DecimalFunctions::decimalToDouble(Decimal decimal) { return toDouble(decimal.bits()); }

Decimal DecimalFunctions::doubleToDecimal(double d) { return Decimal::fromBits(fromDouble(d)); }

Decimal DecimalFunctions::stringToDecimal(std::string str) {
    // texts that stand for "no value"
    if (str == "2147483647" || str == "9223372036854775807" || str == "1.7976931348623157E308"
        || str == "-9223372036854775808") {
        str.clear();
    }
    return Decimal::fromBits(parse(str.c_str()));
}

std::string DecimalFunctions::decimalToString(Decimal value) {
    const u64 x = value.bits();
    char buf[32];
    int n = 0;
    buf[n++] = (x & SIGN_BIT) ? '-' : '+';
    if (isSpecial(x)) {
        const char* word = isInf(x) ? "Inf" : ((x & SNAN_BITS) == SNAN_BITS ? "SNaN" : "NaN");
        while (*word) buf[n++] = *word++;
    } else {
        const Num v = unpack(x);
        n += writeDigits(buf + n, v.coeff);
        buf[n++] = 'E';
        int e = v.exp - EXP_BIAS;
        buf[n++] = e < 0 ? '-' : '+';
        if (e < 0) e = -e;
        n += writeDigits(buf + n, static_cast<u64>(e));
    }
    return std::string(buf, static_cast<std::size_t>(n));
}

std::string DecimalFunctions::decimalStringToDisplay(Decimal value) {
    const std::string text = decimalToString(value);
    if (text == "+NaN" || text == "-SNaN") return std::string();
    const std::size_t e = text.find('E');
    if (e == std::string::npos) return text;

    // sign, digits, 'E', sign, exponent
    const std::size_t count = e - 1;
    std::size_t shift = 0;
    for (std::size_t i = e + 2; i < text.size(); ++i) shift = shift * 10 + static_cast<std::size_t>(text[i] - '0');
    std::string out;
    if (text[0] == '-') out += '-';
    if (text[e + 1] == '+') {
        // a positive exponent adds no zeros: only the digits are shown
        // (kept as it is, existing programs rely on this text)
        out.append(text, 1, count);
    } else if (shift < count) {
        out.append(text, 1, count - shift);
        out += '.';
        out.append(text, 1 + count - shift, shift);
    } else {
        out += "0.";
        out.append(shift - count, '0');
        out.append(text, 1, count);
    }
    return out;
}

namespace ibx {

Decimal decimalFromDouble(double d) {
    if (d == DBL_MAX || d != d) return UNSET_DECIMAL;
    const u64 bits = doubleBits(d);
    const bool neg = (bits >> 63) != 0;
    const double a = bitsDouble(bits & ~SIGN_BIT);
    if (a < 9007199254740992.0) {
        // whole numbers below 2^53
        const u64 whole = static_cast<u64>(a);
        if (static_cast<double>(static_cast<std::int64_t>(whole)) == a) return Decimal::fromBits(pack(neg, EXP_BIAS, whole));
#if defined(DEC_EXACT_DOUBLE)
        // Up to 15 digits: at most one such decimal reads back to the double,
        // and the division tells exactly whether it does.
        for (int k = 1; k <= 22; ++k) {
            const double scaled = a * POW10D[k];
            if (scaled >= 1e15) break;
            const u64 v = static_cast<u64>(scaled + 0.5);
            if (static_cast<double>(static_cast<std::int64_t>(v)) / POW10D[k] == a)
                return Decimal::fromBits(pack(neg, EXP_BIAS - k, v));
        }
#endif
    }
    if ((bits & ~SIGN_BIT) == DBL_INF_BITS) return Decimal::fromBits(packInf(neg));
    u64 m;
    int e2;
    splitDouble(bits, &m, &e2);
    return Decimal::fromBits(shortest(neg, m, e2));
}

double decimalToDoubleOrMax(Decimal d) {
    if (isNan(d.bits())) return DBL_MAX;
    return toDouble(d.bits());
}

} // namespace ibx
