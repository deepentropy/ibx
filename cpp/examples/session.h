// What the examples share: a session with its client, its signal and its
// reader, and a loop that gives the callbacks on the calling thread.
#pragma once
#ifndef IBX_EXAMPLES_SESSION_H
#define IBX_EXAMPLES_SESSION_H

#include <chrono>
#include <cstdio>
#include <functional>
#include <memory>
#include <string>

#include "DefaultEWrapper.h"
#include "EClientSocket.h"
#include "EReader.h"
#include "EReaderOSSignal.h"

class Session : public DefaultEWrapper {
public:
  Session() : signal(100), client(this, &signal) {}

  // Host and port name a local gateway for the official client; here the
  // login goes to the servers with the credentials of the environment
  // (IB_USERNAME, IB_PASSWORD), on the paper system.
  bool connect(int clientId = 0)
  {
    if (!client.eConnect("127.0.0.1", 4002, clientId))
      return false;
    reader.reset(new EReader(&client, &signal));
    reader->start();
    // The session starts with the accounts and the next valid order id.
    return pump(30, [this] { return nextId >= 0; });
  }

  // Give the callbacks until `done` says so or the time has passed.
  bool pump(double seconds, std::function<bool()> done = [] { return false; })
  {
    auto end = std::chrono::steady_clock::now() + std::chrono::milliseconds(static_cast<long long>(seconds * 1000));
    while (client.isConnected() && std::chrono::steady_clock::now() < end) {
      signal.waitForSignal();
      reader->processMsgs();
      if (done())
        return true;
    }
    return done();
  }

  // Orders go to a paper account only.
  bool paper() const
  {
    if (account.compare(0, 2, "DU") == 0)
      return true;
    std::printf("not a paper account: no order is sent\n");
    return false;
  }

  void nextValidId(int orderId) override { nextId = orderId; }
  void managedAccounts(const std::string& accountsList) override { account = accountsList.substr(0, accountsList.find(',')); }
  void error(int id, long long, int errorCode, const std::string& errorString, const std::string&) override
  {
    // The notices of the data connections are not errors.
    if (errorCode < 2100 || errorCode > 2199)
      std::printf("[error] id=%d code=%d %s\n", id, errorCode, errorString.c_str());
  }

  EReaderOSSignal signal;
  EClientSocket client;
  std::unique_ptr<EReader> reader;
  int nextId = -1;
  std::string account;
};

inline Contract stock(const char* symbol, int conId = 0)
{
  Contract c;
  c.conId = conId;
  c.symbol = symbol;
  c.secType = "STK";
  c.exchange = "SMART";
  c.currency = "USD";
  return c;
}

#endif
