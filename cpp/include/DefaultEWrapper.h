// A wrapper whose callbacks do nothing: derive and override what is needed.
#pragma once
#ifndef IBX_DEFAULTEWRAPPER_H
#define IBX_DEFAULTEWRAPPER_H

#include "EWrapper.h"

class TWSAPIDLLEXP DefaultEWrapper : public EWrapper {
public:
  EWrapper_declare(EWrapper_methods_override)
};

#endif
