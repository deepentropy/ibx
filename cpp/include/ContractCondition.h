// A condition on a contract, on an exchange.
#pragma once
#ifndef IBX_CONTRACTCONDITION_H
#define IBX_CONTRACTCONDITION_H

#include "OperatorCondition.h"

class TWSAPIDLLEXP ContractCondition : public OperatorCondition {
public:
  virtual std::string toString();
  int conId();
  void conId(int conId);
  std::string exchange();
  void exchange(const std::string& exchange);

private:
  int m_conId{0};
  std::string m_exchange;
};

#endif
