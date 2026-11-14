// A condition on the change of a contract's price, in percent.
#pragma once
#ifndef IBX_PERCENTCHANGECONDITION_H
#define IBX_PERCENTCHANGECONDITION_H

#include "CommonDefs.h"
#include "ContractCondition.h"

class TWSAPIDLLEXP PercentChangeCondition : public ContractCondition {
  friend OrderCondition;

public:
  PercentChangeCondition() : m_changePercent(UNSET_DOUBLE) {}

  static const OrderConditionType conditionType = OrderConditionType::PercentChange;

  double changePercent();
  void changePercent(double changePercent);

protected:
  virtual std::string valueToString() const;
  virtual void valueFromString(const std::string& v);

private:
  double m_changePercent;
};

#endif
