// 64-bit decimal floating-point value (decimal64 in BID encoding, IEEE 754-2008).
#pragma once
#include "platformspecific.h"
#include <climits>
#include <cstdint>
#include <string>

class TWSAPIDLLEXP Decimal {
public:
    constexpr Decimal() : value(ULLONG_MAX) {}
    static constexpr Decimal fromBits(std::uint64_t bits) { return Decimal(bits); }
    std::uint64_t bits() const { return value; }
    bool isUnset() const { return bits() == ULLONG_MAX; }
private:
    explicit constexpr Decimal(std::uint64_t bits) : value(bits) {}
    std::uint64_t value;
};
static const Decimal UNSET_DECIMAL = Decimal::fromBits(ULLONG_MAX);
static const Decimal ZERO_DECIMAL = Decimal::fromBits(0x31c0000000000000ULL);

class TWSAPIDLLEXP DecimalFunctions {
public:
    static Decimal add(Decimal decimal1, Decimal decimal2);
    static Decimal sub(Decimal decimal1, Decimal decimal2);
    static Decimal mul(Decimal decimal1, Decimal decimal2);
    static Decimal div(Decimal decimal1, Decimal decimal2);
    static double decimalToDouble(Decimal decimal);
    static Decimal doubleToDecimal(double d);
    static Decimal stringToDecimal(std::string str);
    static std::string decimalToString(Decimal value);
    static std::string decimalStringToDisplay(Decimal value);
};

namespace ibx {

// The decimal of the shortest plain text that reads back to `d` (100.0 gives
// 100, 0.5 gives 5E-1). DBL_MAX and NaN give UNSET_DECIMAL.
TWSAPIDLLEXP Decimal decimalFromDouble(double d);

// The nearest double. An unset or NaN decimal gives DBL_MAX.
TWSAPIDLLEXP double decimalToDoubleOrMax(Decimal d);

} // namespace ibx
