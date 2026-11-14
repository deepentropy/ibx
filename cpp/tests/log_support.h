// A wrapper that writes every callback as one row of text, with every
// field of the structures: for the live runs and their comparison with
// the reference.
//
// A row: the callback's name, then its arguments separated by tabs. A
// number is written in its shortest form, an unset value as MAX, a flag as
// 0 or 1, a structure as {field=value;...}, a list as [a,b].
#pragma once
#ifndef IBX_LOG_SUPPORT_H
#define IBX_LOG_SUPPORT_H

#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <list>
#include <map>
#include <memory>
#include <set>
#include <string>
#include <type_traits>
#include <vector>

#include "CommissionAndFeesReport.h"
#include "Contract.h"
#include "EWrapper.h"
#include "Execution.h"
#include "ExecutionCondition.h"
#include "MarginCondition.h"
#include "Order.h"
#include "OrderState.h"
#include "PercentChangeCondition.h"
#include "PriceCondition.h"
#include "ScannerSubscription.h"
#include "TimeCondition.h"
#include "VolumeCondition.h"
#include "WshEventData.h"

namespace ibxlog {

// The shortest text that reads back to the same number.
inline std::string shortest(double v)
{
  if (v == UNSET_DOUBLE)
    return "MAX";
  if (std::isnan(v))
    return "nan";
  if (std::isinf(v))
    return v > 0 ? "inf" : "-inf";
  char text[40];
  for (int digits = 1; digits <= 17; ++digits) {
    std::snprintf(text, sizeof text, "%.*g", digits, v);
    if (std::strtod(text, nullptr) == v)
      break;
  }
  return text;
}

class Row {
public:
  std::string text;

  explicit Row(const char* name) : text(name), m_first(false) {}

  // The next argument of the callback.
  template <class T> void arg(const T& v)
  {
    text += '\t';
    put(v);
  }

  // A field of a structure.
  template <class T> void field(const char* name, const T& v)
  {
    if (!m_first)
      text += ';';
    m_first = false;
    text += name;
    text += '=';
    put(v);
  }

  void open()
  {
    text += '{';
    m_stack.push_back(m_first);
    m_first = true;
  }

  void close()
  {
    text += '}';
    m_first = m_stack.back();
    m_stack.pop_back();
  }

  void put(bool v) { text += v ? '1' : '0'; }
  void put(int v) { text += std::to_string(v); }
  void put(long long v) { text += std::to_string(v); }
  void put(double v) { text += shortest(v); }
  void put(Decimal v) { text += shortest(ibx::decimalToDoubleOrMax(v)); }
  void put(char v) { text += v ? std::string(1, v) : std::string(); }
  void put(const char* v) { put(std::string(v)); }
  void put(const std::string& v)
  {
    for (char c : v)
      text += (c == '\t' || c == '\n' || c == '\r') ? ' ' : c;
  }
  template <class T> typename std::enable_if<std::is_enum<T>::value>::type put(T v) { text += std::to_string(static_cast<long long>(v)); }
  template <class T> typename std::enable_if<std::is_class<T>::value && !std::is_same<T, std::string>::value && !std::is_same<T, Decimal>::value>::type put(const T& v);

  template <class T> void put(const std::vector<T>& v) { items(v); }
  template <class T> void put(const std::set<T>& v) { items(v); }
  template <class T> void put(const std::list<T>& v) { items(v); }
  template <class T> void put(const std::shared_ptr<T>& v)
  {
    if (v)
      put(*v);
    else
      text += '~';
  }
  template <class T> void put(const std::unique_ptr<T>& v)
  {
    if (v)
      put(*v);
    else
      text += '~';
  }
  void put(const std::shared_ptr<std::vector<std::shared_ptr<TagValue>>>& v);
  template <class T> void put(const std::shared_ptr<std::vector<std::shared_ptr<T>>>& v)
  {
    if (v)
      items(*v);
    else
      text += "[]";
  }
  void put(const SmartComponentsMap& v)
  {
    text += '[';
    bool first = true;
    for (const auto& item : v) {
      if (!first)
        text += ',';
      first = false;
      open();
      field("bitNumber", item.first);
      field("exchange", std::get<0>(item.second));
      field("exchangeLetter", std::get<1>(item.second));
      close();
    }
    text += ']';
  }

private:
  template <class C> void items(const C& v)
  {
    text += '[';
    bool first = true;
    for (const auto& item : v) {
      if (!first)
        text += ',';
      first = false;
      put(item);
    }
    text += ']';
  }

  bool m_first;
  std::vector<bool> m_stack;
};

inline void dump(Row& r, const SoftDollarTier& x)
{
  r.open();
  r.field("name", x.name());
  r.field("val", x.val());
  r.field("displayName", x.displayName());
  r.close();
}

// A condition, by what its kind has.
inline void dump(Row& r, const OrderCondition& c0)
{
  OrderCondition& c = const_cast<OrderCondition&>(c0);
  r.open();
  r.field("type", static_cast<int>(c.type()));
  r.field("isConjunctionConnection", c.conjunctionConnection());
  if (OperatorCondition* op = dynamic_cast<OperatorCondition*>(&c))
    r.field("isMore", op->isMore());
  if (ContractCondition* cc = dynamic_cast<ContractCondition*>(&c)) {
    r.field("conId", cc->conId());
    r.field("exchange", cc->exchange());
  }
  if (PriceCondition* p = dynamic_cast<PriceCondition*>(&c)) {
    r.field("price", p->price());
    r.field("triggerMethod", static_cast<int>(p->triggerMethod()));
  } else if (TimeCondition* t = dynamic_cast<TimeCondition*>(&c)) {
    r.field("time", t->time());
  } else if (MarginCondition* m = dynamic_cast<MarginCondition*>(&c)) {
    r.field("percent", m->percent());
  } else if (ExecutionCondition* e = dynamic_cast<ExecutionCondition*>(&c)) {
    r.field("secType", e->secType());
    r.field("exchange", e->exchange());
    r.field("symbol", e->symbol());
  } else if (VolumeCondition* v = dynamic_cast<VolumeCondition*>(&c)) {
    r.field("volume", v->volume());
  } else if (PercentChangeCondition* pc = dynamic_cast<PercentChangeCondition*>(&c)) {
    r.field("changePercent", pc->changePercent());
  }
  r.close();
}

#include "generated_log.inc"

inline void Row::put(const std::shared_ptr<std::vector<std::shared_ptr<TagValue>>>& v)
{
  if (v)
    items(*v);
  else
    text += "[]";
}

template <class T> typename std::enable_if<std::is_class<T>::value && !std::is_same<T, std::string>::value && !std::is_same<T, Decimal>::value>::type Row::put(const T& v)
{
  dump(*this, v);
}

}  // namespace ibxlog

#endif
