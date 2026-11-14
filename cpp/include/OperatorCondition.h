// A condition that compares a value: above or below.
#pragma once
#ifndef IBX_OPERATORCONDITION_H
#define IBX_OPERATORCONDITION_H

#include "OrderCondition.h"

class TWSAPIDLLEXP OperatorCondition : public OrderCondition {
public:
  virtual std::string toString();
  bool isMore();
  void isMore(bool isMore);

protected:
  virtual std::string valueToString() const = 0;
  virtual void valueFromString(const std::string& v) = 0;

private:
  bool m_isMore{false};
};

#endif
