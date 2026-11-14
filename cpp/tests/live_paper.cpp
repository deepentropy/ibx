// A session on the paper account: about 45 different requests, every
// callback written as one row. The rows are compared with the ones the
// official client gets from the reference for the same requests.
//
//   IB_USERNAME=... IB_PASSWORD=... live_paper [--client 198]
//
// Orders go to the paper account only: the program stops when the account
// of the login is not a paper one. The one order it places is a limit far
// from the market, cancelled before the end.
//
// Exit code: 0 when the session ran and the answers every session gives
// came; 1 otherwise; 3 when the account is not a paper account.

#include <chrono>
#include <condition_variable>
#include <atomic>
#include <cstdio>
#include <ctime>
#include <functional>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include "EClientSocket.h"
#include "EReader.h"
#include "EReaderOSSignal.h"
#include "ibx_runtime.h"
#include "log_support.h"

#if defined(IBX_PROTOBUF)
#include <google/protobuf/util/json_util.h>
#endif

namespace {

typedef std::chrono::steady_clock Clock;

class App : public ibxlog::LogWrapper {
public:
  App() : m_signal(200), client(this, &m_signal), m_start(Clock::now()), nextId(-1), m_quiet(true), m_running(false) {}

  long long ms() const { return std::chrono::duration_cast<std::chrono::milliseconds>(Clock::now() - m_start).count(); }

  void row(const std::string& text) override
  {
    std::lock_guard<std::mutex> lock(m_mutex);
    if (text.compare(0, 16, "managedAccounts\t") == 0)
      account = text.substr(16, text.find(',', 16) == std::string::npos ? std::string::npos : text.find(',', 16) - 16);
    if (text.compare(0, 12, "nextValidId\t") == 0)
      nextId = std::atoi(text.c_str() + 12);
    if (!m_quiet) {
      m_log.push_back(text);
      std::printf("%7lld %s\n", ms(), text.c_str());
      std::fflush(stdout);
    }
    m_changed.notify_all();
  }

  void note(const std::string& text) { row("-- " + text); }

#if defined(IBX_PROTOBUF)
  // A protobuf callback as a row: its name, then its message as JSON with
  // the field names of the .proto files.
  void protoRow(const char* name, const google::protobuf::Message& m) override
  {
    google::protobuf::util::JsonPrintOptions options;
    options.preserve_proto_field_names = true;
    std::string json;
    if (!google::protobuf::util::MessageToJsonString(m, &json, options).ok())
      json = "?";
    row(std::string(name) + "\t" + json);
  }
#endif

  int count(const std::string& prefix)
  {
    std::lock_guard<std::mutex> lock(m_mutex);
    return countLocked(prefix);
  }

  // True when n logged rows start with the prefix.
  bool wait(const std::string& prefix, int n = 1, int seconds = 20)
  {
    bool ok;
    {
      std::unique_lock<std::mutex> lock(m_mutex);
      ok = m_changed.wait_for(lock, std::chrono::seconds(seconds), [&] { return countLocked(prefix) >= n; });
    }
    if (!ok)
      note("no " + prefix + " x" + std::to_string(n) + " in " + std::to_string(seconds) + " s");
    return ok;
  }

  bool waitFor(std::function<bool()> ready, int seconds)
  {
    std::unique_lock<std::mutex> lock(m_mutex);
    return m_changed.wait_for(lock, std::chrono::seconds(seconds), ready);
  }

  // The first row that starts with the prefix.
  std::string first(const std::string& prefix)
  {
    std::lock_guard<std::mutex> lock(m_mutex);
    for (const std::string& l : m_log) {
      if (l.compare(0, prefix.size(), prefix) == 0)
        return l;
    }
    return std::string();
  }

  void loud()
  {
    std::lock_guard<std::mutex> lock(m_mutex);
    m_quiet = false;
    m_log.clear();
    m_start = Clock::now();
  }

  // The callbacks come on a thread of their own, the requests on the main one.
  void start()
  {
    m_running = true;
    m_thread = std::thread([this] {
      EReader reader(&client, &m_signal);
      reader.start();
      while (m_running && client.isConnected()) {
        m_signal.waitForSignal();
        reader.processMsgs();
      }
      reader.stop();
    });
  }

  void stop()
  {
    m_running = false;
    m_signal.issueSignal();
    if (m_thread.joinable())
      m_thread.join();
  }

private:
  int countLocked(const std::string& prefix) const
  {
    int n = 0;
    for (const std::string& l : m_log)
      n += l.compare(0, prefix.size(), prefix) == 0;
    return n;
  }

  EReaderOSSignal m_signal;

public:
  EClientSocket client;
  std::string account;

private:
  Clock::time_point m_start;

public:
  int nextId;

private:
  bool m_quiet;
  std::atomic<bool> m_running;
  std::thread m_thread;
  std::mutex m_mutex;
  std::condition_variable m_changed;
  std::vector<std::string> m_log;
};

void pause(double seconds) { std::this_thread::sleep_for(std::chrono::milliseconds(static_cast<int>(seconds * 1000))); }

Contract stock(const char* symbol)
{
  Contract c;
  c.symbol = symbol;
  c.secType = "STK";
  c.exchange = "SMART";
  c.currency = "USD";
  return c;
}

std::vector<std::string> split(const std::string& text, char by)
{
  std::vector<std::string> out;
  size_t at = 0;
  for (;;) {
    size_t end = text.find(by, at);
    out.push_back(text.substr(at, end == std::string::npos ? std::string::npos : end - at));
    if (end == std::string::npos)
      return out;
    at = end + 1;
  }
}

}  // namespace

int main(int argc, char** argv)
{
  int clientId = 198;
  for (int k = 1; k + 1 < argc; ++k) {
    if (std::string(argv[k]) == "--client")
      clientId = std::atoi(argv[k + 1]);
  }
  const int AAPL = 265598, IWM = 9579970;
  App app;
  EClientSocket& c = app.client;
  if (!c.eConnect("127.0.0.1", 4002, clientId)) {
    std::printf("no session\n");
    return 1;
  }
  app.start();
  if (!app.waitFor([&] { return app.nextId >= 0; }, 40)) {
    std::printf("no session\n");
    c.eDisconnect();
    app.stop();
    return 1;
  }
  if (app.account.compare(0, 2, "DU") != 0) {  // orders and tests only ever on the paper account
    c.eDisconnect();
    app.stop();
    std::printf("NOT A PAPER ACCOUNT: stopped\n");
    return 3;
  }
  pause(3.0);  // the connect notices settle
  app.loud();
  std::time_t now = std::time(nullptr);
  char stamp[32];
  std::strftime(stamp, sizeof stamp, "%d/%m/%Y %H:%M:%S", std::localtime(&now));
  std::printf("== live_paper, client %d, %s, level %d\n", clientId, stamp, c.serverVersion());
  const std::string acct = app.account;
  TagValueListSPtr none;

  app.note("A1 time, ids, accounts, user");
  c.reqCurrentTime();
  c.reqCurrentTimeInMillis();
  c.reqIds(1);
  c.reqManagedAccts();
  c.reqUserInfo(1);
  app.wait("userInfo\t", 1, 5);
  pause(0.5);

  app.note("A2 contract details");
  c.reqContractDetails(10, stock("AAPL"));
  app.wait("contractDetailsEnd");
  Contract opt;
  opt.symbol = "AAPL";
  opt.secType = "OPT";
  opt.exchange = "SMART";
  opt.currency = "USD";
  opt.lastTradeDateOrContractMonth = "20261120";
  opt.strike = 250.0;
  opt.right = "C";
  c.reqContractDetails(9, opt);
  app.wait("contractDetailsEnd", 2);

  app.note("A3 snapshot");
  c.reqMktData(11, stock("AAPL"), "", true, false, none);
  app.wait("tickSnapshotEnd", 1, 15);

  app.note("A4 stream 4 s with generic ticks 233,236");
  c.reqMktData(12, stock("AAPL"), "233,236", false, false, none);
  pause(4);
  c.cancelMktData(12);
  pause(0.5);

  app.note("A4b an option stream 4 s");
  c.reqMktData(8, opt, "", false, false, none);
  pause(4);
  c.cancelMktData(8);
  pause(0.5);

  app.note("A5 tick by tick Last, BidAsk, MidPoint 3 s each");
  const char* kinds[] = {"Last", "BidAsk", "MidPoint"};
  for (int k = 0; k < 3; ++k) {
    c.reqTickByTickData(13 + k, stock("AAPL"), kinds[k], 0, false);
    pause(3);
    c.cancelTickByTickData(13 + k);
    pause(0.3);
  }

  app.note("A6 five-second bars 7 s");
  c.reqRealTimeBars(16, stock("AAPL"), 5, "TRADES", true, none);
  pause(7);
  c.cancelRealTimeBars(16);

  app.note("A7 depth exchanges, depth 3 s");
  c.reqMktDepthExchanges();
  app.wait("mktDepthExchanges\t", 1, 8);
  c.reqMktDepth(17, stock("AAPL"), 3, true, none);
  pause(3);
  c.cancelMktDepth(17, true);
  pause(0.5);

  app.note("A8 historical data, head timestamp, ticks, histogram");
  c.reqHistoricalData(18, stock("AAPL"), "", "1 D", "1 hour", "TRADES", 1, 1, false, none);
  app.wait("historicalDataEnd\t");
  c.reqHeadTimestamp(19, stock("AAPL"), "TRADES", 1, 1);
  app.wait("headTimestamp\t");
  std::time_t yesterday = now - 86400;
  char day[16];
  std::strftime(day, sizeof day, "%Y%m%d", std::localtime(&yesterday));
  std::string from = std::string(day) + " 15:00:00 US/Eastern";
  c.reqHistoricalTicks(20, stock("AAPL"), from, "", 3, "TRADES", 1, true, none);
  app.wait("historicalTicksLast\t");
  c.reqHistoricalTicks(21, stock("AAPL"), from, "", 3, "BID_ASK", 1, true, none);
  app.wait("historicalTicksBidAsk\t");
  c.reqHistoricalTicks(22, stock("AAPL"), from, "", 3, "MIDPOINT", 1, true, none);
  app.wait("historicalTicks\t");
  c.reqHistogramData(23, stock("AAPL"), true, "1 day");
  app.wait("histogramData\t");

  app.note("A8b bars kept up to date 7 s, the schedule");
  c.reqHistoricalData(38, stock("AAPL"), "", "1 D", "1 min", "TRADES", 1, 1, true, none);
  pause(7);
  c.cancelHistoricalData(38);
  c.reqHistoricalData(39, stock("AAPL"), "", "2 D", "1 day", "SCHEDULE", 1, 1, false, none);
  app.wait("historicalSchedule\t", 1, 8);

  app.note("A9 account summary, positions, account updates, multi");
  c.reqAccountSummary(24, "All", "NetLiquidation,BuyingPower");
  app.wait("accountSummaryEnd\t");
  c.cancelAccountSummary(24);
  c.reqPositions();
  app.wait("positionEnd");
  c.cancelPositions();
  c.reqAccountUpdates(true, acct);
  app.wait("accountDownloadEnd\t");
  c.reqAccountUpdates(false, acct);
  c.reqPositionsMulti(25, acct, "");
  app.wait("positionMultiEnd\t");
  c.cancelPositionsMulti(25);
  c.reqAccountUpdatesMulti(26, acct, "", true);
  app.wait("accountUpdateMultiEnd\t");
  c.cancelAccountUpdatesMulti(26);

  app.note("A10 P&L 3 s each");
  c.reqPnL(27, acct, "");
  pause(3);
  c.cancelPnL(27);
  c.reqPnLSingle(28, acct, "", IWM);
  pause(3);
  c.cancelPnLSingle(28);

  app.note("A11 option parameters, tiers, family codes, symbols, components, rule");
  c.reqSecDefOptParams(29, "AAPL", "", "STK", AAPL);
  app.wait("securityDefinitionOptionalParameterEnd\t");
  c.reqSoftDollarTiers(30);
  app.wait("softDollarTiers\t", 1, 8);
  c.reqFamilyCodes();
  app.wait("familyCodes\t", 1, 8);
  c.reqMatchingSymbols(31, "AAP");
  app.wait("symbolSamples\t", 1, 8);
  c.reqSmartComponents(32, "9c0001");
  app.wait("smartComponents\t", 1, 8);
  c.reqMarketRule(4563);  // a rule of AAPL's contract details
  app.wait("marketRule\t", 1, 8);

  app.note("A12 news");
  c.reqNewsProviders();
  app.wait("newsProviders\t", 1, 8);
  c.reqHistoricalNews(33, AAPL, "BRFG+DJNL", "", "", 3, none);
  app.wait("historicalNewsEnd\t", 1, 10);
  std::string news = app.first("historicalNews\t33\t");
  if (!news.empty()) {
    // historicalNews, 33, time, provider, article id, headline
    std::vector<std::string> parts = split(news, '\t');
    if (parts.size() >= 5) {
      c.reqNewsArticle(40, parts[3], parts[4], none);
      app.wait("newsArticle\t", 1, 10);
    }
  }
  c.reqNewsBulletins(true);
  pause(1);
  c.cancelNewsBulletins();

  app.note("A13 scanner");
  c.reqScannerParameters();
  app.wait("scannerParameters\t", 1, 15);
  ScannerSubscription sub;
  sub.instrument = "STK";
  sub.locationCode = "STK.US.MAJOR";
  sub.scanCode = "TOP_PERC_GAIN";
  sub.numberOfRows = 3;
  c.reqScannerSubscription(34, sub, none, none);
  app.wait("scannerDataEnd\t", 1, 15);
  c.cancelScannerSubscription(34);

  app.note("A14 display groups");
  c.queryDisplayGroups(35);
  app.wait("displayGroupList\t", 1, 5);
  c.subscribeToGroupEvents(41, 1);
  app.wait("displayGroupUpdated\t", 1, 5);
  c.unsubscribeFromGroupEvents(41);

  app.note("A15 an order placed, listed, cancelled; completed orders; executions");
  int oid = app.nextId;
  int statuses = app.count("orderStatus\t");
  Order o;
  o.action = "BUY";
  o.totalQuantity = DecimalFunctions::stringToDecimal("1");
  o.orderType = "LMT";
  o.lmtPrice = 100.0;
  o.tif = "DAY";
  c.placeOrder(oid, stock("AAPL"), o);
  app.wait("orderStatus\t", statuses + 1, 15);
  pause(1.5);
  c.reqOpenOrders();
  app.wait("openOrderEnd", 1, 10);
  c.reqAllOpenOrders();
  app.wait("openOrderEnd", 2, 10);
  c.cancelOrder(oid, OrderCancel());
  bool cancelled = app.wait("orderStatus\t" + std::to_string(oid) + "\tCancelled", 1, 15);
  pause(1);
  c.reqCompletedOrders(true);
  app.wait("completedOrdersEnd", 1, 10);
  c.reqExecutions(36, ExecutionFilter());
  app.wait("execDetailsEnd\t", 1, 10);

  app.note("A16 calendar meta data 3 s");
  c.reqWshMetaData(37);
  pause(3);

#if defined(IBX_PROTOBUF)
  // The configuration has a protobuf form only: read it, change nothing.
  app.note("A17 the configuration");
  protobuf::ConfigRequest configRequest;
  configRequest.set_reqid(42);
  c.reqConfigProtoBuf(configRequest);
  app.wait("configResponseProtoBuf\t", 1, 10);
#endif

  pause(2.0);  // an answer that follows the last request is lost without this wait
  bool ok = cancelled && app.count("contractDetails\t") >= 2 && app.count("historicalData\t") >= 1 && app.count("accountSummary\t") >= 1
         && app.count("positionEnd") >= 1 && app.count("openOrder\t") >= 1 && app.count("securityDefinitionOptionalParameter\t") >= 1
         && app.count("symbolSamples\t") == 1 && app.count("scannerParameters\t") == 1 && app.count("-- no ") == 0;
  int missing = app.count("-- no ");
  c.eDisconnect();
  app.stop();
  std::printf("== end: %s, %d answers that did not come\n", ok ? "ok" : "NOT OK", missing);
  return ok ? 0 : 1;
}
