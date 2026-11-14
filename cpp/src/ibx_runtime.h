// What the sources of the library share: the encoding of a structure
// across the C entry layer, and the helpers the generated code calls.
#pragma once
#ifndef IBX_RUNTIME_H
#define IBX_RUNTIME_H

#include <cstdint>
#include <cstring>
#include <exception>
#include <memory>
#include <set>
#include <string>
#include <vector>

#include "CommissionAndFeesReport.h"
#include "Contract.h"
#include "Decimal.h"
#include "EClient.h"
#include "EWrapper.h"
#include "Execution.h"
#include "Order.h"
#include "OrderCancel.h"
#include "OrderState.h"
#include "ScannerSubscription.h"
#include "Utils.h"
#include "WshEventData.h"
#include "ibx_capi.h"

namespace ibx {

// The fields of a structure one after the other: a number as 8 bytes in
// the machine's order (little-endian on every supported machine), a flag
// as one byte, a text or a list as a 4-byte count and its content.
struct Writer {
  std::string& out;

  explicit Writer(std::string& buffer) : out(buffer) {}

  void i(long long v) { out.append(reinterpret_cast<const char*>(&v), sizeof v); }
  void f(double v) { out.append(reinterpret_cast<const char*>(&v), sizeof v); }
  void b(bool v) { out.push_back(v ? '\1' : '\0'); }
  void n(size_t count)
  {
    std::uint32_t v = static_cast<std::uint32_t>(count);
    out.append(reinterpret_cast<const char*>(&v), sizeof v);
  }
  void s(const std::string& v)
  {
    n(v.size());
    out.append(v);
  }
};

// Reads what the other side wrote. A buffer that ends early reads as
// zeros and empty texts.
struct Reader {
  const char* p;
  const char* end;

  explicit Reader(ibx_str data) : p(data.p), end(data.p ? data.p + data.n : data.p) {}

  template <class T> T take()
  {
    T v = T();
    if (static_cast<size_t>(end - p) >= sizeof v) {
      std::memcpy(&v, p, sizeof v);
      p += sizeof v;
    } else {
      p = end;
    }
    return v;
  }

  long long i() { return take<long long>(); }
  double f() { return take<double>(); }
  bool b() { return take<char>() != 0; }
  // A count, never more than the bytes left.
  size_t n()
  {
    size_t count = take<std::uint32_t>();
    size_t left = static_cast<size_t>(end - p);
    return count < left ? count : left;
  }
  std::string s()
  {
    size_t count = n();
    std::string v(p, count);
    p += count;
    return v;
  }
};

// A buffer for one encoded parameter of a request, kept by the thread
// between requests.
struct Buf {
  std::string& text;
  Writer w;

  Buf();
  ~Buf();
  Buf(const Buf&) = delete;
  Buf& operator=(const Buf&) = delete;

  ibx_str view() const { return ibx_str{text.data(), text.size()}; }
};

inline ibx_str view(const std::string& s) { return ibx_str{s.data(), s.size()}; }
inline std::string str(ibx_str s) { return s.n ? std::string(s.p, s.n) : std::string(); }

// Where the callbacks of a pass go. The first exception a callback throws
// is kept, and thrown again when the pass has ended.
struct Sink {
  EWrapper* w;
  std::exception_ptr error;

  void failed()
  {
    if (!error)
      error = std::current_exception();
  }
};

extern const ibx_callbacks callbackTable;

#include "ibx_codecs.inc"

void writeTags(Writer& w, const TagValueListSPtr& list);
void readTags(Reader& r, TagValueListSPtr& list);
void writeLegPrices(Writer& w, const Order::OrderComboLegListSPtr& legs);
void readLegPrices(Reader& r, Order::OrderComboLegListSPtr& legs);
void writeConditions(Writer& w, const std::vector<std::shared_ptr<OrderCondition>>& conditions);
void readConditions(Reader& r, std::vector<std::shared_ptr<OrderCondition>>& conditions);

// A list of structures: null when empty, as a structure starts.
template <class T> void writeList(Writer& w, const std::shared_ptr<std::vector<std::shared_ptr<T>>>& list)
{
  w.n(list ? list->size() : 0);
  if (!list)
    return;
  for (const std::shared_ptr<T>& item : *list) {
    if (item)
      write(w, *item);
    else
      write(w, T());
  }
}

template <class T> void readList(Reader& r, std::shared_ptr<std::vector<std::shared_ptr<T>>>& list)
{
  size_t count = r.n();
  if (!count)
    return;
  list = std::make_shared<std::vector<std::shared_ptr<T>>>();
  list->reserve(count);
  for (size_t k = 0; k < count; ++k) {
    std::shared_ptr<T> item = std::make_shared<T>();
    read(r, *item);
    list->push_back(item);
  }
}

template <class T> void readVec(Reader& r, std::vector<T>& out)
{
  size_t count = r.n();
  out.resize(count);
  for (size_t k = 0; k < count; ++k)
    read(r, out[k]);
}

// A quantity given as a text: unset when the text is empty.
Decimal decimalFromText(const std::string& text);
// The text of a quantity the number alone cannot hold (more than eight
// decimals); empty otherwise.
std::string quantityText(Decimal quantity);

inline TickAttrib tick_attrib(std::uint32_t mask)
{
  TickAttrib a;
  a.canAutoExecute = (mask & 1) != 0;
  a.pastLimit = (mask & 2) != 0;
  a.preOpen = (mask & 4) != 0;
  return a;
}

inline TickAttribLast tick_attrib_last(std::uint32_t mask)
{
  TickAttribLast a;
  a.pastLimit = (mask & 1) != 0;
  a.unreported = (mask & 2) != 0;
  return a;
}

inline TickAttribBidAsk tick_attrib_bid_ask(std::uint32_t mask)
{
  TickAttribBidAsk a;
  a.bidPastLow = (mask & 1) != 0;
  a.askPastHigh = (mask & 2) != 0;
  return a;
}

std::set<std::string> decTextSet(Reader& r);
std::set<double> decDoubleSet(Reader& r);
std::vector<SoftDollarTier> decSoftDollarTiers(Reader& r);
std::vector<ContractDescription> decContractDescriptions(Reader& r);
SmartComponentsMap decSmartComponents(Reader& r);
HistogramDataVector decHistogram(Reader& r);
std::vector<HistoricalSession> decSessions(Reader& r);
std::vector<HistoricalTick> decTicks(Reader& r);
std::vector<HistoricalTickBidAsk> decTicksBidAsk(Reader& r);
std::vector<HistoricalTickLast> decTicksLast(Reader& r);

}  // namespace ibx

#endif
