// A soft dollar tier: its name, its value and the name shown.
#pragma once
#ifndef IBX_SOFTDOLLARTIER_H
#define IBX_SOFTDOLLARTIER_H

#include "platformspecific.h"

#include <string>

class TWSAPIDLLEXP SoftDollarTier {
public:
  SoftDollarTier(const std::string& name = "", const std::string& val = "", const std::string& displayName = "")
    : m_name(name), m_val(val), m_displayName(displayName) {}

  std::string name() const { return m_name; }
  std::string val() const { return m_val; }
  std::string displayName() const { return m_displayName; }

private:
  std::string m_name, m_val, m_displayName;
};

#endif
