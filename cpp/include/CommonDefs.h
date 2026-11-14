// Values and small enumerations shared by the structures.
#pragma once
#ifndef IBX_COMMONDEFS_H
#define IBX_COMMONDEFS_H

#include <cfloat>
#include <climits>
#include <string>

// The value of a field that is not set.
#define UNSET_INTEGER INT_MAX
#define UNSET_DOUBLE DBL_MAX
#define UNSET_LLONG LLONG_MAX

enum faDataType { GROUPS = 1, ALIASES = 3 };

inline const char* faDataTypeStr(faDataType pFaDataType)
{
  switch (pFaDataType) {
    case GROUPS:
      return "GROUPS";
    case ALIASES:
      return "ALIASES";
  }
  return 0;
}

enum MarketDataType { REALTIME = 1, FROZEN = 2, DELAYED = 3, DELAYED_FROZEN = 4 };

const std::string INFINITY_STR = "Infinity";

enum class FundAssetType { None, Others, MoneyMarket, FixedIncome, MultiAsset, Equity, Sector, Guaranteed, Alternative };

enum class FundDistributionPolicyIndicator { None, AccumulationFund, IncomeFund };

enum class OptionExerciseType {
  None,
  Exercise,
  Lapse,
  DoNothing,
  Assigned,
  AutoexerciseClearing,
  Expired,
  Netting,
  AutoexerciseTrading
};

#endif
