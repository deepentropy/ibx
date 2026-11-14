// The signal of a session: waitForSignal returns when an answer is ready,
// when issueSignal is called, or when the timeout has passed.
#pragma once
#ifndef IBX_EREADEROSSIGNAL_H
#define IBX_EREADEROSSIGNAL_H

#include "EReaderSignal.h"
#include "platformspecific.h"

#include <atomic>
#include <condition_variable>
#include <mutex>
#include <stdexcept>

#if !defined(INFINITE)
#define INFINITE ((unsigned long)-1)
#endif

struct ibx_client;

class TWSAPIDLLEXP EReaderOSSignal : public EReaderSignal {
public:
  // The timeout is in milliseconds.
  EReaderOSSignal(unsigned long waitTimeout = INFINITE);
  virtual ~EReaderOSSignal(void);

  virtual void issueSignal();
  virtual void waitForSignal();

  // The session the signal waits on, set by the reader: the wait is then
  // the engine's own, with no thread in between.
  void ibxAttach(ibx_client* session);

private:
  unsigned long m_waitTimeout;
  std::atomic<ibx_client*> m_session;
  std::mutex m_mutex;
  std::condition_variable m_ready;
  bool m_open;
};

#endif
