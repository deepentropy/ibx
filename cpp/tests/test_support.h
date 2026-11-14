// What the tests share: the hooks of the Rust client built with its test
// helpers, and values made from a name by the same rule on both sides.
#pragma once
#ifndef IBX_TEST_SUPPORT_H
#define IBX_TEST_SUPPORT_H

#include <cstdio>
#include <set>
#include <string>
#include <vector>

#include "ibx_runtime.h"

extern "C" {
typedef void (*ibx_test_sink)(void* user, ibx_str data);

bool ibx_test_make(ibx_str kind, int64_t seed, ibx_test_sink sink, void* user);
bool ibx_test_check(ibx_str kind, ibx_str data, int64_t seed, ibx_test_sink sink, void* user);
bool ibx_test_describe(ibx_str kind, ibx_str data, ibx_test_sink sink, void* user);
bool ibx_test_sample(ibx_str kind, ibx_test_sink sink, void* user);
void ibx_test_fire(const ibx_callbacks* callbacks, void* user);
void ibx_test_ticks(const ibx_callbacks* callbacks, void* user, uint64_t count);

typedef struct ibx_test_driver {
  void* user;
  void (*adopt)(void* user, ibx_client* session);
  bool (*request)(void* user, ibx_str name, ibx_str fields);
  void (*dispatch)(void* user, ibx_test_sink sink, void* sink_user);
} ibx_test_driver;

bool ibx_test_replay(ibx_str name, ibx_str options, const ibx_test_driver* driver, ibx_test_sink sink, void* user);
}

namespace ibxtest {

typedef std::vector<std::string> Failures;

inline unsigned hashName(const char* s)
{
  unsigned h = 0;
  for (; *s; ++s)
    h = h * 31u + static_cast<unsigned char>(*s);
  return h;
}

inline long long tvI(const char* name, int seed) { return static_cast<long long>(hashName(name) % 997u) + seed; }
inline double tvF(const char* name, int seed) { return static_cast<double>(tvI(name, seed)) + 0.25; }
inline bool tvB(const char* name, int seed) { return ((static_cast<long long>(hashName(name)) + seed) % 2) == 1; }
inline std::string tvS(const char* name, int seed) { return std::string(name) + std::to_string(seed); }

inline void expect(Failures& out, const std::string& at, bool ok)
{
  if (!ok)
    out.push_back(at);
}

inline TagValueListSPtr tags(const char* name, int seed)
{
  TagValueListSPtr list = std::make_shared<TagValueList>();
  for (int k = 1; k <= 2; ++k) {
    std::string tag = std::string(name) + ".tag";
    std::string value = std::string(name) + ".value";
    list->push_back(std::make_shared<TagValue>(tvS(tag.c_str(), seed + k), tvS(value.c_str(), seed + k)));
  }
  return list;
}

inline bool sameTags(const TagValueListSPtr& list, const char* name, int seed)
{
  if (!list || list->size() != 2)
    return false;
  for (int k = 1; k <= 2; ++k) {
    std::string tag = std::string(name) + ".tag";
    std::string value = std::string(name) + ".value";
    const TagValueSPtr& item = (*list)[k - 1];
    if (!item || item->tag != tvS(tag.c_str(), seed + k) || item->value != tvS(value.c_str(), seed + k))
      return false;
  }
  return true;
}

// A number of a recorded request: "MAX" is the unset value.
inline double number(const std::string& text) { return text == "MAX" ? UNSET_DOUBLE : std::stod(text); }

inline void takeText(void* user, ibx_str data) { static_cast<std::string*>(user)->append(data.p ? data.p : "", data.n); }

#include "generated_checks.inc"

}  // namespace ibxtest

#endif
