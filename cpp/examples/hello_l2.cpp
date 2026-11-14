// Ten seconds of the market depth of AAPL, five rows a side.
//
//   IB_USERNAME=... IB_PASSWORD=... hello_l2

#include "session.h"

struct App : Session {
  int updates = 0;

  void updateMktDepthL2(int, int position, const std::string& marketMaker, int operation, int side, double price, Decimal size, bool) override
  {
    ++updates;
    std::printf("%s row=%d op=%d %-8s %.2f x %s\n", side ? "bid" : "ask", position, operation, marketMaker.c_str(), price,
                DecimalFunctions::decimalStringToDisplay(size).c_str());
  }
  void updateMktDepth(int, int position, int operation, int side, double price, Decimal size) override
  {
    ++updates;
    std::printf("%s row=%d op=%d %.2f x %s\n", side ? "bid" : "ask", position, operation, price,
                DecimalFunctions::decimalStringToDisplay(size).c_str());
  }
};

int main()
{
  App app;
  if (!app.connect())
    return 1;
  app.client.reqMktDepth(1, stock("AAPL", 265598), 5, true, TagValueListSPtr());
  app.pump(10);
  app.client.cancelMktDepth(1, true);
  std::printf("updates: %d\n", app.updates);
  app.client.eDisconnect();
  return 0;
}
