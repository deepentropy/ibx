// The errors the client library raises itself, as code and text.
#pragma once
#ifndef IBX_SOCKETCLIENTERRORS_H
#define IBX_SOCKETCLIENTERRORS_H

#include <string>

static const int NO_VALID_ID = -1;
static const int NO_VALID_ERROR_CODE = 0;

class CodeMsgPair {
public:
  CodeMsgPair(int code, std::string msg) : m_errorCode(code), m_errorMsg(msg) {}
  int code() const { return m_errorCode; }
  const std::string& msg() const { return m_errorMsg; }

private:
  int m_errorCode;
  std::string m_errorMsg;
};

static const CodeMsgPair ALREADY_CONNECTED(501, "Already connected.");
static const CodeMsgPair CONNECT_FAIL(502, "Couldn't connect to the server. ");
static const CodeMsgPair UPDATE_TWS(503, "The server is out of date and must be upgraded.");
static const CodeMsgPair NOT_CONNECTED(504, "Not connected");
static const CodeMsgPair UNKNOWN_ID(505, "Fatal Error: Unknown message id.");
static const CodeMsgPair UNSUPPORTED_VERSION(506, "Unsupported version");
static const CodeMsgPair BAD_LENGTH(507, "Bad message length");
static const CodeMsgPair BAD_MESSAGE(508, "Bad message");
static const CodeMsgPair SOCKET_EXCEPTION(509, "Exception caught while reading socket - ");
static const CodeMsgPair FAIL_CREATE_SOCK(520, "Failed to create socket");
static const CodeMsgPair INVALID_SYMBOL(579, "Invalid symbol in string - ");
static const CodeMsgPair FA_PROFILE_NOT_SUPPORTED(585, "FA Profile is not supported anymore, use FA Group instead - ");
static const CodeMsgPair ERROR_ENCODING_PROTOBUF(588, "Error encoding protobuf - ");

#endif
