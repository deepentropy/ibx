// The requests of a session. EClientSocket is the class a program creates.
#pragma once
#ifndef IBX_ECLIENT_H
#define IBX_ECLIENT_H

#include <cstdint>
#include <exception>
#include <memory>
#include <string>
#include <vector>

#include "CommonDefs.h"
#include "Contract.h"
#include "Execution.h"
#include "Order.h"
#include "OrderCancel.h"
#include "ScannerSubscription.h"
#include "TagValue.h"
#include "WshEventData.h"
#include "platformspecific.h"

#if defined(IBX_PROTOBUF)
#include "ibx_protobuf.h"
#endif

class EWrapper;
struct ibx_client;

class TWSAPIDLLEXP EClient {
public:
  explicit EClient(EWrapper* ptr);
  virtual ~EClient();

  virtual bool eConnect(const char* host, int port, int clientId = 0, bool extraAuth = false) = 0;
  virtual void eDisconnect(bool resetState) = 0;

  int clientId() const { return m_clientId; }
  const std::string& optionalCapabilities() const;
  // Settings of a socket session; kept, with no effect here.
  void setOptionalCapabilities(const std::string& optCapts);
  void setConnectOptions(const std::string& connectOptions);
  void disableUseV100Plus();
  bool usingV100Plus();
  // Whether the requests have their protobuf form: when the library is
  // built with it (IBX_PROTOBUF). The client has its own link to the
  // servers, so the form of a request changes nothing on the way.
  bool useProtoBuf(int msgId);

  enum ConnState { CS_DISCONNECTED, CS_CONNECTING, CS_CONNECTED };

  ConnState connState() const;
  bool isConnected() const;
  const std::string& host() const { return m_host; }
  int port() const { return m_port; }

  EWrapper* getWrapper() const;
  bool isInBufferEmpty() const;

  // The API level of the session, and the time it started.
  int serverVersion();
  std::string TwsConnectionTime();

#include "ibx_requests.inc"

#if defined(IBX_PROTOBUF)
#include "ibx_requests_proto.inc"
#endif

  // Starts the callbacks of a session of the official client; here the
  // session is started by eConnect.
  void startApi();

  // The account of the login (not part of the official client).
  std::string ibxAccountId() const;
  // The session, for the reader and the signal.
  ibx_client* ibxSession() const { return m_handle; }
  // Every ready answer, as callbacks on the calling thread.
  void ibxProcess();
  // Take a session made elsewhere as this client's (for the tests).
  void ibxAdopt(ibx_client* session);

protected:
  // Whether a request can go out; when not, the error 504 is given.
  bool ibxReady(int id);
  // The end of a request: the error 504 when the session has ended.
  void ibxDone(int id, int32_t status);

  void setClientId(int clientId) { m_clientId = clientId; }
  void setExtraAuth(bool extraAuth) { m_extraAuth = extraAuth; }
  void setHost(const std::string& host) { m_host = host; }
  void setPort(int port) { m_port = port; }
  bool extraAuth() { return m_extraAuth; }

  EWrapper* m_pEWrapper;
  ibx_client* m_handle;
  ConnState m_connState;
  int m_serverVersion;
  std::string m_TwsTime;

private:
  std::string m_host;
  int m_port;
  int m_clientId;
  bool m_extraAuth;
  std::string m_optionalCapabilities;
  std::string m_connectOptions;
};

#endif
