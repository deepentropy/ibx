// Contract details of AAPL: its conId and its primary exchange.
//
//   IB_USERNAME=... IB_PASSWORD=... hello_contract_details

#include "session.h"

struct App : Session {
  bool done = false;

  void contractDetails(int, const ContractDetails& d) override
  {
    std::printf("%s conId=%d primary=%s name=%s\n", d.contract.symbol.c_str(), d.contract.conId, d.contract.primaryExchange.c_str(),
                d.longName.c_str());
  }
  void contractDetailsEnd(int) override { done = true; }
};

int main()
{
  App app;
  if (!app.connect())
    return 1;
  app.client.reqContractDetails(1, stock("AAPL"));
  bool ok = app.pump(15, [&] { return app.done; });
  app.client.eDisconnect();
  return ok ? 0 : 1;
}
