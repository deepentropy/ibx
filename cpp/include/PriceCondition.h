// A condition on the price of a contract.
#pragma once
#ifndef IBX_PRICECONDITION_H
#define IBX_PRICECONDITION_H

#include "ContractCondition.h"

class TWSAPIDLLEXP PriceCondition : public ContractCondition {
  friend OrderCondition;

public:
  PriceCondition() {}

  static const OrderConditionType conditionType = OrderConditionType::Price;

  enum Method { Default = 0, DoubleBidAsk = 1, Last = 2, DoubleLast = 3, BidAsk = 4, LastBidAsk = 7, MidPoint = 8 };

  double price();
  void price(double price);
  virtual std::string toString();
  Method triggerMethod();
  std::string strTriggerMethod();
  void triggerMethod(int triggerMethod);
  void triggerMethod(Method triggerMethod);

private:
  virtual std::string valueToString() const;
  virtual void valueFromString(const std::string& v);

  double m_price{0.0};
  int m_triggerMethod{0};
};

#endif
