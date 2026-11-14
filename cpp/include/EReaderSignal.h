// What wakes the thread that gives the callbacks.
#pragma once
#ifndef IBX_EREADERSIGNAL_H
#define IBX_EREADERSIGNAL_H

struct EReaderSignal {
  virtual void issueSignal() = 0;
  virtual void waitForSignal() = 0;
  virtual ~EReaderSignal() {}
};

#endif
