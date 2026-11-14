// Small helpers on the values of the structures.
#pragma once
#ifndef IBX_UTILS_H
#define IBX_UTILS_H

#include <ctime>
#include <string>

#include "CommonDefs.h"
#include "Decimal.h"

class Utils {
public:
  static bool stringIsEmpty(std::string str);
  // Whether a value is set (not the unset value of its type).
  static bool isValidValue(int value);
  static bool isValidValue(long long value);
  static bool isValidValue(double value);
  static bool isValidValue(Decimal value) { return !value.isUnset(); }
  static bool isPegBenchOrder(std::string orderType);
  static bool isPegMidOrder(std::string orderType);
  static bool isPegBestOrder(std::string orderType);
  // The enumeration values of the codes a contract's details carry.
  static FundDistributionPolicyIndicator getFundDistributionPolicyIndicator(std::string value);
  static FundAssetType getFundAssetType(std::string value);
  static long long currentTimeMillis();
  static OptionExerciseType getOptionExerciseType(int val);
};

#endif
