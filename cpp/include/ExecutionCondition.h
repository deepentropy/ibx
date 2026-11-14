// A condition on a trade of a symbol.
#pragma once
#ifndef IBX_EXECUTIONCONDITION_H
#define IBX_EXECUTIONCONDITION_H

#include "OrderCondition.h"

class TWSAPIDLLEXP ExecutionCondition : public OrderCondition {
  friend OrderCondition;

public:
  ExecutionCondition() {}

  static const OrderConditionType conditionType = OrderConditionType::Execution;

  virtual std::string toString();
  std::string exchange();
  void exchange(const std::string& exchange);
  std::string secType();
  void secType(const std::string& secType);
  std::string symbol();
  void symbol(const std::string& symbol);

private:
  std::string m_exchange;
  std::string m_secType;
  std::string m_symbol;
};

#endif
