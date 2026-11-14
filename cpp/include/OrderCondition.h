// A condition of an order; the kinds derive from it.
#pragma once
#ifndef IBX_ORDERCONDITION_H
#define IBX_ORDERCONDITION_H

#include "platformspecific.h"

#include <string>

class TWSAPIDLLEXP OrderCondition {
public:
  enum OrderConditionType { Price = 1, Time = 3, Margin = 4, Execution = 5, Volume = 6, PercentChange = 7 };

  virtual ~OrderCondition() {}
  virtual std::string toString();

  // Whether the condition is joined to the next one by "and" (else "or").
  bool conjunctionConnection() const;
  void conjunctionConnection(bool isConjunctionConnection);
  OrderConditionType type();
  std::string typeName();

  // A new condition of a kind; the caller owns it.
  static OrderCondition* create(OrderConditionType type);

private:
  OrderConditionType m_type{Price};
  bool m_isConjunctionConnection{false};
};

#endif
