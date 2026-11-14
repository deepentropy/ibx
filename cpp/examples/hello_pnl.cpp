// The profit and loss of the account, for five seconds.
//
//   IB_USERNAME=... IB_PASSWORD=... hello_pnl

#include "session.h"

struct App : Session {
  int updates = 0;

  void pnl(int, double dailyPnL, double unrealizedPnL, double realizedPnL) override
  {
    ++updates;
    std::printf("daily=%.2f unrealized=%.2f realized=%.2f\n", dailyPnL, unrealizedPnL, realizedPnL);
  }
};

int main()
{
  App app;
  if (!app.connect())
    return 1;
  app.client.reqPnL(1, app.account, "");
  app.pump(5);
  app.client.cancelPnL(1);
  std::printf("updates: %d\n", app.updates);
  app.client.eDisconnect();
  return 0;
}
