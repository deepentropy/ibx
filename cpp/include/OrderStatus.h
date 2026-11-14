// The status of an order as a value, read from the text a callback gives.
#pragma once
#ifndef IBX_ORDERSTATUS_H
#define IBX_ORDERSTATUS_H

#include <algorithm>
#include <cctype>
#include <string>

enum class OrderStatus {
  ApiPending,
  ApiCancelled,
  PreSubmitted,
  PendingCancel,
  Cancelled,
  Submitted,
  Filled,
  Inactive,
  PendingSubmit,
  Unknown
};

struct OrderStatusUtil {
  static const char* toString(OrderStatus status)
  {
    static const char* const names[] = {"ApiPending", "ApiCancelled", "PreSubmitted", "PendingCancel", "Cancelled",
                                        "Submitted",  "Filled",       "Inactive",     "PendingSubmit", "Unknown"};
    int i = static_cast<int>(status);
    return i >= 0 && i < 10 ? names[i] : "Unknown";
  }

  // The status of a text, whatever its case.
  static OrderStatus get(const std::string& apiString)
  {
    std::string lower = apiString;
    std::transform(lower.begin(), lower.end(), lower.begin(), [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
    for (int i = 0; i < 9; ++i) {
      std::string name = toString(static_cast<OrderStatus>(i));
      std::transform(name.begin(), name.end(), name.begin(), [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
      if (lower == name)
        return static_cast<OrderStatus>(i);
    }
    return OrderStatus::Unknown;
  }

  static bool isActive(OrderStatus status)
  {
    return status == OrderStatus::PreSubmitted || status == OrderStatus::PendingCancel || status == OrderStatus::Submitted
        || status == OrderStatus::PendingSubmit;
  }

  static bool isTerminal(OrderStatus status)
  {
    return status == OrderStatus::Filled || status == OrderStatus::Cancelled || status == OrderStatus::Inactive
        || status == OrderStatus::ApiCancelled;
  }
};

#endif
