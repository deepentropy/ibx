// Gives the answers of a session to the wrapper: processMsgs, on the
// thread that calls it.
#pragma once
#ifndef IBX_EREADER_H
#define IBX_EREADER_H

#include "EReaderOSSignal.h"
#include "platformspecific.h"

#include <atomic>
#include <thread>

class EClientSocket;
struct EReaderSignal;

class TWSAPIDLLEXP EReader {
public:
  EReader(EClientSocket* clientSocket, EReaderSignal* signal);
  ~EReader(void);

  // Every answer that is ready, as callbacks on the calling thread. An
  // exception a callback throws is thrown again from here, after the
  // other answers of the pass were given.
  void processMsgs(void);
  // Kept for programs of the official client; nothing is queued here.
  bool putMessageToQueue();
  void start();
  void stop();

private:
  void forward();

  EClientSocket* m_pClientSocket;
  EReaderSignal* m_pEReaderSignal;
  std::atomic<bool> m_isAlive;
  std::thread m_thread;
};

#endif
