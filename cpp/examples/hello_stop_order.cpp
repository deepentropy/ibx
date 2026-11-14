// Place a stop order far above the market, watch it acknowledged, cancel it.
//
//   IB_USERNAME=... IB_PASSWORD=... hello_stop_order
//
// Paper account only. The price is far from the market: the order does
// not fill.

#include <vector>

#include "session.h"

struct App : Session {
  std::vector<std::pair<int, std::string>> statuses;

  void orderStatus(int orderId, const std::string& status, Decimal, Decimal, double, long long, int, double, int, const std::string&, double) override
  {
    std::printf("[status] id=%d %s\n", orderId, status.c_str());
    statuses.push_back({orderId, status});
  }
  bool seen(int orderId, const char* status) const
  {
    for (const auto& s : statuses) {
      if (s.first == orderId && s.second == status)
        return true;
    }
    return false;
  }
};

int main()
{
  App app;
  if (!app.connect())
    return 1;
  if (!app.paper()) {
    app.client.eDisconnect();
    return 3;
  }
  int id = app.nextId;
  Order order;
  order.action = "BUY";
  order.totalQuantity = DecimalFunctions::stringToDecimal("1");
  order.orderType = "STP";
  order.auxPrice = 9999.0;
  order.tif = "GTC";
  order.outsideRth = true;
  std::printf("placing BUY 1 AAPL STP 9999.00 (id=%d)\n", id);
  app.client.placeOrder(id, stock("AAPL", 265598), order);
  // Working, or held until the market opens.
  app.pump(15, [&] { return app.seen(id, "Submitted") || app.seen(id, "PreSubmitted"); });

  std::printf("cancelling id=%d\n", id);
  app.client.cancelOrder(id, OrderCancel());
  bool ok = app.pump(15, [&] { return app.seen(id, "Cancelled"); });
  app.client.eDisconnect();
  return ok ? 0 : 1;
}
