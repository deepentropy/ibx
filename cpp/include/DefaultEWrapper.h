// A wrapper whose callbacks do nothing: derive and override what is needed.
//
// With the protobuf forms (IBX_PROTOBUF), a protobuf callback that is not
// overridden costs one message: the empty callback of this class notes that
// it was reached, and the message of that callback is no longer built for
// this wrapper. An override must therefore not call the callback of this
// class it replaces.
#pragma once
#ifndef IBX_DEFAULTEWRAPPER_H
#define IBX_DEFAULTEWRAPPER_H

#include "EWrapper.h"

class TWSAPIDLLEXP DefaultEWrapper : public EWrapper {
public:
  EWrapper_declare(EWrapper_methods_override)

#if defined(IBX_PROTOBUF)
  const ibx::ProtoCallbackSet* ibxUnusedProtoCallbacks() const override { return &m_ibxUnused; }

private:
  // The protobuf callbacks that reached the empty ones of this class.
  ibx::ProtoCallbackSet m_ibxUnused;
#endif
};

#endif
