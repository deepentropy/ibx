// The exception of the client library: an error pair and a text.
#pragma once
#ifndef IBX_ECLIENTEXCEPTION_H
#define IBX_ECLIENTEXCEPTION_H

#include <exception>
#include <iostream>

#include "TwsSocketClientErrors.h"

struct EClientException : public std::exception {
public:
  EClientException(CodeMsgPair error, std::string text) : m_error(error), m_text(text) {}
  CodeMsgPair error() const { return m_error; }
  std::string text() const { return m_text; }

private:
  CodeMsgPair m_error;
  std::string m_text;
};

#endif
