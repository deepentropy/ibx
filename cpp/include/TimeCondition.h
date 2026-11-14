// A condition on the time.
#pragma once
#ifndef IBX_TIMECONDITION_H
#define IBX_TIMECONDITION_H

#include "OperatorCondition.h"

class TWSAPIDLLEXP TimeCondition : public OperatorCondition {
  friend OrderCondition;

public:
  TimeCondition() {}

  static const OrderConditionType conditionType = OrderConditionType::Time;

  virtual std::string toString();
  std::string time();
  void time(const std::string& time);

protected:
  virtual std::string valueToString() const;
  virtual void valueFromString(const std::string& v);

private:
  std::string m_time;
};

#endif
