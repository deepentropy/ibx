// The client a program creates: it logs in to the servers itself, there is
// no local gateway to connect to.
#pragma once
#ifndef IBX_ECLIENTSOCKET_H
#define IBX_ECLIENTSOCKET_H

#include <functional>
#include <string>

#include "EClient.h"

class EWrapper;
class EReader;
struct EReaderSignal;

namespace ibx {

// The challenge of a typed second factor of a live login.
struct Challenge {
  std::string displayId;
  std::string url;
  // The factor, as the server names it; empty for the mobile key.
  std::string kind;
  std::string message;
  // A security code card: the picture of the two indexes to look up (PNG).
  std::string image;
  // A one-time passcode: how the server sent it.
  std::string delivery;
};

// What a login needs.
struct Login {
  std::string username;
  std::string password;
  // The server; empty for the default one.
  std::string host;
  // False logs in to the live system: the call then waits for the second
  // factor (the approval on the phone, or the code of codeProvider).
  bool paper = true;
  // The core the engine thread is pinned to; -1: not pinned.
  int coreId = -1;
  // Copy the waits of the reference that change nothing for a program.
  bool copiedWaits = false;
  // Gives the code of a typed second factor. Called once, on another
  // thread. An exception it throws ends the login.
  std::function<std::string(const Challenge&)> codeProvider;
};

}  // namespace ibx

class TWSAPIDLLEXP EClientSocket : public EClient {
public:
  explicit EClientSocket(EWrapper* ptr, EReaderSignal* pSignal = 0);
  virtual ~EClientSocket();

  // The form of the official client. `host` and `port` name a local
  // gateway there and are kept only for host() and port(): the login
  // takes the user name and the password of the environment (IB_USERNAME,
  // IB_PASSWORD), the server of IB_HOST when set. Paper or live: IB_PAPER
  // when set (0 or false: live), else the port (4001 and 7496: live),
  // else paper.
  bool eConnect(const char* host, int port, int clientId = 0, bool extraAuth = false) override;
  // The form of this client: the login given in full.
  bool eConnect(const ibx::Login& login, int clientId = 0);
  void eDisconnect(bool resetState = true) override;

  bool isSocketOK() const;
  // Kept for programs of the official client: the login is made by
  // eConnect whatever the value.
  bool asyncEConnect() const;
  void asyncEConnect(bool val);

  void registerEReader(EReader* reader);

private:
  EReaderSignal* m_pSignal;
  bool m_asyncEConnect;
  EReader* m_pEReader{nullptr};
  std::function<std::string(const ibx::Challenge&)> m_codeProvider;
};

#endif
