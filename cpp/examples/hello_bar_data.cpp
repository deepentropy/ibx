// One day of 5-minute bars of SPY.
//
//   IB_USERNAME=... IB_PASSWORD=... hello_bar_data

#include "session.h"

struct App : Session {
  int bars = 0;
  bool done = false;

  void historicalData(int, const Bar& bar) override
  {
    if (bars++ < 5)
      std::printf("%s o=%.2f h=%.2f l=%.2f c=%.2f v=%s\n", bar.time.c_str(), bar.open, bar.high, bar.low, bar.close,
                  DecimalFunctions::decimalStringToDisplay(bar.volume).c_str());
  }
  void historicalDataEnd(int, const std::string&, const std::string&) override { done = true; }
};

int main()
{
  App app;
  if (!app.connect())
    return 1;
  app.client.reqHistoricalData(1, stock("SPY", 756733), "", "1 D", "5 mins", "TRADES", 1, 1, false, TagValueListSPtr());
  bool ok = app.pump(30, [&] { return app.done; });
  std::printf("bars: %d\n", app.bars);
  app.client.eDisconnect();
  return ok ? 0 : 1;
}
