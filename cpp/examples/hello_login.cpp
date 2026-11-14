// Log in, read the accounts and the next valid order id, log out.
//
//   IB_USERNAME=... IB_PASSWORD=... hello_login

#include "session.h"

int main()
{
  Session s;
  if (!s.connect())
    return 1;
  std::printf("logged in. account = %s, next valid id = %d, level = %d\n", s.account.c_str(), s.nextId, s.client.serverVersion());
  s.client.eDisconnect();
  return 0;
}
