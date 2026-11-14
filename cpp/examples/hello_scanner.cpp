// The top gainers among the US stocks.
//
//   IB_USERNAME=... IB_PASSWORD=... hello_scanner

#include "session.h"

struct App : Session {
  int rows = 0;
  bool done = false;

  void scannerData(int, int rank, const ContractDetails& d, const std::string&, const std::string&, const std::string&, const std::string&) override
  {
    if (rows++ < 10)
      std::printf("#%-3d %-8s %-6s conId=%d\n", rank, d.contract.symbol.c_str(), d.contract.primaryExchange.c_str(), d.contract.conId);
  }
  void scannerDataEnd(int) override { done = true; }
};

int main()
{
  App app;
  if (!app.connect())
    return 1;
  ScannerSubscription sub;
  sub.instrument = "STK";
  sub.locationCode = "STK.US.MAJOR";
  sub.scanCode = "TOP_PERC_GAIN";
  sub.numberOfRows = 25;
  app.client.reqScannerSubscription(1, sub, TagValueListSPtr(), TagValueListSPtr());
  bool ok = app.pump(30, [&] { return app.done; });
  std::printf("results: %d\n", app.rows);
  app.client.cancelScannerSubscription(1);
  app.client.eDisconnect();
  return ok ? 0 : 1;
}
