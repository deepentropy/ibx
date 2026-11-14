// Ten seconds of the top of book of SPY.
//
//   IB_USERNAME=... IB_PASSWORD=... hello_tick_data

#include "session.h"

struct App : Session {
  int ticks = 0;

  void tickPrice(int, TickType field, double price, const TickAttrib&) override
  {
    ++ticks;
    std::printf("price type=%d %.2f\n", static_cast<int>(field), price);
  }
  void tickSize(int, TickType field, Decimal size) override
  {
    ++ticks;
    std::printf("size  type=%d %s\n", static_cast<int>(field), DecimalFunctions::decimalStringToDisplay(size).c_str());
  }
};

int main()
{
  App app;
  if (!app.connect())
    return 1;
  app.client.reqMktData(1, stock("SPY", 756733), "", false, false, TagValueListSPtr());
  app.pump(10);
  app.client.cancelMktData(1);
  std::printf("ticks: %d\n", app.ticks);
  app.client.eDisconnect();
  return 0;
}
