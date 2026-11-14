// A condition on the margin cushion of the account, in percent.
#pragma once
#ifndef IBX_MARGINCONDITION_H
#define IBX_MARGINCONDITION_H

#include "OperatorCondition.h"

class TWSAPIDLLEXP MarginCondition : public OperatorCondition {
  friend OrderCondition;

public:
  MarginCondition() {}

  static const OrderConditionType conditionType = OrderConditionType::Margin;

  virtual std::string valueToString() const;
  virtual void valueFromString(const std::string& v);
  virtual std::string toString();
  int percent();
  void percent(int percent);

private:
  int m_percent{0};
};

#endif
