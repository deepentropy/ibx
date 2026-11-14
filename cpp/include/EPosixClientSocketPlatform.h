// Kept so that a program of the official client compiles: the client has
// no socket to a local gateway, it logs in to the servers itself.
#pragma once
#ifndef IBX_EPOSIXCLIENTSOCKETPLATFORM_H
#define IBX_EPOSIXCLIENTSOCKETPLATFORM_H

inline bool SocketsInit() { return true; }
inline bool SocketsDestroy() { return true; }

#endif
