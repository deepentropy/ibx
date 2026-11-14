// A condition on the volume of a contract.
#pragma once
#ifndef IBX_VOLUMECONDITION_H
#define IBX_VOLUMECONDITION_H

#include "ContractCondition.h"

class TWSAPIDLLEXP VolumeCondition : public ContractCondition {
  friend OrderCondition;

public:
  VolumeCondition() {}

  static const OrderConditionType conditionType = OrderConditionType::Volume;

  int volume();
  void volume(int volume);

protected:
  virtual std::string valueToString() const;
  virtual void valueFromString(const std::string& v);

private:
  int m_volume{0};
};

#endif
