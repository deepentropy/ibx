// The client, the reader and the signal of a session.

#if defined(_MSC_VER)
#define _CRT_SECURE_NO_WARNINGS  // getenv
#endif

#include "ibx_runtime.h"

#include <chrono>
#include <cstdlib>

#include "EClientSocket.h"
#include "EReader.h"
#include "EReaderOSSignal.h"
#include "TwsSocketClientErrors.h"

// ── EClient ──

EClient::EClient(EWrapper* ptr)
  : m_pEWrapper(ptr)
  , m_handle(nullptr)
  , m_connState(CS_DISCONNECTED)
  , m_serverVersion(0)
  , m_port(0)
  , m_clientId(-1)
  , m_extraAuth(false)
{}

EClient::~EClient()
{
  if (m_handle)
    ibx_free(m_handle);
}

const std::string& EClient::optionalCapabilities() const { return m_optionalCapabilities; }

void EClient::setOptionalCapabilities(const std::string& optCapts) { m_optionalCapabilities = optCapts; }

void EClient::setConnectOptions(const std::string& connectOptions) { m_connectOptions = connectOptions; }

void EClient::disableUseV100Plus() {}

bool EClient::usingV100Plus() { return true; }

bool EClient::useProtoBuf(int msgId)
{
  (void)msgId;
#if defined(IBX_PROTOBUF)
  return true;
#else
  return false;
#endif
}

EClient::ConnState EClient::connState() const
{
  // A session the servers ended is seen here, not only at the next pass.
  if (m_connState == CS_CONNECTED && !(m_handle && ibx_is_connected(m_handle)))
    return CS_DISCONNECTED;
  return m_connState;
}

bool EClient::isConnected() const { return connState() == CS_CONNECTED; }

EWrapper* EClient::getWrapper() const { return m_pEWrapper; }

bool EClient::isInBufferEmpty() const { return true; }

int EClient::serverVersion() { return m_serverVersion; }

std::string EClient::TwsConnectionTime() { return m_TwsTime; }

void EClient::startApi() {}

std::string EClient::ibxAccountId() const
{
  char text[64];
  size_t n = m_handle ? ibx_account_id(m_handle, text, sizeof text) : 0;
  return std::string(text, n < sizeof text ? n : sizeof text);
}

bool EClient::ibxReady(int id)
{
  if (m_handle && m_connState == CS_CONNECTED)
    return true;
  m_pEWrapper->error(id, Utils::currentTimeMillis(), NOT_CONNECTED.code(), NOT_CONNECTED.msg(), "");
  return false;
}

void EClient::ibxDone(int id, int32_t status)
{
  if (status != 0)
    m_pEWrapper->error(id, Utils::currentTimeMillis(), NOT_CONNECTED.code(), NOT_CONNECTED.msg(), "");
}

void EClient::ibxProcess()
{
  // Nothing more is given once the program has ended the session.
  if (!m_handle || m_connState != CS_CONNECTED)
    return;
  ibx::Sink sink{m_pEWrapper, nullptr};
  ibx_process(m_handle, &ibx::callbackTable, &sink);
  // The servers ended the session: the pass gave connectionClosed.
  if (!ibx_is_connected(m_handle))
    m_connState = CS_DISCONNECTED;
  if (sink.error)
    std::rethrow_exception(sink.error);
}

void EClient::ibxAdopt(ibx_client* session)
{
  if (m_handle)
    ibx_free(m_handle);
  m_handle = session;
  m_connState = session ? CS_CONNECTED : CS_DISCONNECTED;
  m_serverVersion = session ? ibx_server_version(session) : 0;
}

// ── EClientSocket ──

namespace {

std::string environment(const char* name)
{
  const char* value = std::getenv(name);
  return value ? std::string(value) : std::string();
}

// The code provider of a login, as the C entry layer calls it.
int32_t provideCode(void* user, const ibx_challenge* challenge, char* code, size_t capacity)
{
  const std::function<std::string(const ibx::Challenge&)>* provider =
      static_cast<const std::function<std::string(const ibx::Challenge&)>*>(user);
  try {
    ibx::Challenge c;
    c.displayId = ibx::str(challenge->display_id);
    c.url = ibx::str(challenge->url);
    c.kind = ibx::str(challenge->kind);
    c.message = ibx::str(challenge->message);
    c.image = ibx::str(challenge->image);
    c.delivery = ibx::str(challenge->delivery);
    std::string text = (*provider)(c);
    size_t n = text.size() < capacity ? text.size() : capacity;
    std::memcpy(code, text.data(), n);
    return static_cast<int32_t>(n);
  } catch (...) {
    return -1;
  }
}

}  // namespace

EClientSocket::EClientSocket(EWrapper* ptr, EReaderSignal* pSignal) : EClient(ptr), m_pSignal(pSignal), m_asyncEConnect(false) {}

EClientSocket::~EClientSocket()
{
  // The reader lets go of the session before it is released.
  if (m_pEReader)
    m_pEReader->stop();
}

bool EClientSocket::asyncEConnect() const { return m_asyncEConnect; }

void EClientSocket::asyncEConnect(bool val) { m_asyncEConnect = val; }

bool EClientSocket::isSocketOK() const { return isConnected(); }

void EClientSocket::registerEReader(EReader* reader) { m_pEReader = reader; }

bool EClientSocket::eConnect(const char* host, int port, int clientId, bool extraAuth)
{
  setHost(host ? host : "");
  setPort(port);
  setExtraAuth(extraAuth);
  ibx::Login login;
  login.username = environment("IB_USERNAME");
  login.password = environment("IB_PASSWORD");
  login.host = environment("IB_HOST");
  std::string paper = environment("IB_PAPER");
  if (!paper.empty())
    login.paper = !(paper == "0" || paper == "false" || paper == "FALSE" || paper == "no");
  else
    login.paper = !(port == 4001 || port == 7496);
  return eConnect(login, clientId);
}

bool EClientSocket::eConnect(const ibx::Login& login, int clientId)
{
  if (m_connState == CS_CONNECTED && m_handle && ibx_is_connected(m_handle)) {
    m_pEWrapper->error(NO_VALID_ID, Utils::currentTimeMillis(), ALREADY_CONNECTED.code(), ALREADY_CONNECTED.msg(), "");
    return false;
  }
  // The session of an earlier connection is released first.
  if (m_pEReader)
    m_pEReader->stop();
  if (m_handle) {
    ibx_free(m_handle);
    m_handle = nullptr;
  }
  m_connState = CS_CONNECTING;
  setClientId(clientId);

  ibx_connect_config config;
  config.username = ibx::view(login.username);
  config.password = ibx::view(login.password);
  config.host = ibx::view(login.host);
  config.paper = login.paper;
  config.copied_waits = login.copiedWaits;
  config.client_id = clientId;
  config.core_id = login.coreId;
  m_codeProvider = login.codeProvider;
  config.code_provider = m_codeProvider ? &provideCode : nullptr;
  config.code_user = &m_codeProvider;

  char reason[512] = {0};
  m_handle = ibx_connect(&config, reason, sizeof reason - 1);
  if (!m_handle) {
    m_connState = CS_DISCONNECTED;
    m_pEWrapper->error(NO_VALID_ID, Utils::currentTimeMillis(), CONNECT_FAIL.code(), CONNECT_FAIL.msg() + reason, "");
    return false;
  }
  m_serverVersion = ibx_server_version(m_handle);
  char time[64];
  size_t n = ibx_connection_time(m_handle, time, sizeof time);
  m_TwsTime.assign(time, n < sizeof time ? n : sizeof time);
  m_connState = CS_CONNECTED;
  m_pEWrapper->connectAck();
  return true;
}

void EClientSocket::eDisconnect(bool resetState)
{
  if (m_pEReader)
    m_pEReader->stop();
  if (m_handle)
    ibx_disconnect(m_handle);
  if (resetState) {
    m_connState = CS_DISCONNECTED;
    m_serverVersion = 0;
    m_TwsTime.clear();
    setClientId(-1);
    setExtraAuth(false);
  }
  // A thread that waits for an answer is let go.
  if (m_pSignal)
    m_pSignal->issueSignal();
}

// ── EReaderOSSignal ──

EReaderOSSignal::EReaderOSSignal(unsigned long waitTimeout) : m_waitTimeout(waitTimeout), m_session(nullptr), m_open(false) {}

EReaderOSSignal::~EReaderOSSignal(void) {}

void EReaderOSSignal::ibxAttach(ibx_client* session)
{
  m_session.store(session);
  // A wait made before the session was known ends, and waits on it next.
  issueSignal();
}

void EReaderOSSignal::issueSignal()
{
  if (ibx_client* session = m_session.load())
    ibx_notify(session);
  {
    std::lock_guard<std::mutex> lock(m_mutex);
    m_open = true;
  }
  m_ready.notify_all();
}

void EReaderOSSignal::waitForSignal()
{
  if (ibx_client* session = m_session.load()) {
    // The engine's own wait. Without a timeout it is made in rounds: the
    // session may be ended meanwhile.
    {
      std::lock_guard<std::mutex> lock(m_mutex);
      if (m_open) {
        m_open = false;
        return;
      }
    }
    if (m_waitTimeout != INFINITE) {
      ibx_wait(session, static_cast<uint32_t>(m_waitTimeout));
      return;
    }
    while (m_session.load() == session && ibx_is_connected(session)) {
      if (ibx_wait(session, 1000))
        return;
    }
    return;
  }
  std::unique_lock<std::mutex> lock(m_mutex);
  if (!m_open) {
    if (m_waitTimeout == INFINITE)
      m_ready.wait(lock, [this] { return m_open; });
    else
      m_ready.wait_for(lock, std::chrono::milliseconds(m_waitTimeout), [this] { return m_open; });
  }
  m_open = false;
}

// ── EReader ──

EReader::EReader(EClientSocket* clientSocket, EReaderSignal* signal)
  : m_pClientSocket(clientSocket), m_pEReaderSignal(signal), m_isAlive(false)
{
  if (m_pClientSocket)
    m_pClientSocket->registerEReader(this);
}

EReader::~EReader(void)
{
  bool started = m_isAlive.load() || m_thread.joinable();
  stop();
  if (m_pClientSocket) {
    m_pClientSocket->registerEReader(nullptr);
    if (started)
      m_pClientSocket->eDisconnect();
  }
}

void EReader::start()
{
  if (m_isAlive.exchange(true))
    return;
  ibx_client* session = m_pClientSocket ? m_pClientSocket->ibxSession() : nullptr;
  if (EReaderOSSignal* signal = dynamic_cast<EReaderOSSignal*>(m_pEReaderSignal)) {
    // The signal waits on the session itself: no thread in between.
    signal->ibxAttach(session);
    return;
  }
  // A signal of the program's own: a thread passes the engine's wake on.
  if (m_pEReaderSignal && session)
    m_thread = std::thread(&EReader::forward, this);
}

void EReader::forward()
{
  while (m_isAlive.load()) {
    ibx_client* session = m_pClientSocket->ibxSession();
    if (!session)
      break;
    if (ibx_wait(session, 100))
      m_pEReaderSignal->issueSignal();
    if (!ibx_is_connected(session)) {
      m_pEReaderSignal->issueSignal();
      break;
    }
  }
}

void EReader::stop()
{
  m_isAlive = false;
  if (m_thread.joinable() && m_thread.get_id() != std::this_thread::get_id())
    m_thread.join();
  // The signal no longer waits on the session: it may be released.
  if (EReaderOSSignal* signal = dynamic_cast<EReaderOSSignal*>(m_pEReaderSignal))
    signal->ibxAttach(nullptr);
}

bool EReader::putMessageToQueue() { return false; }

void EReader::processMsgs(void)
{
  if (m_pClientSocket)
    m_pClientSocket->ibxProcess();
}
