// The protobuf forms (IBX_PROTOBUF): what the generated part
// (proto_generated.cpp) and the part written by hand (proto.cpp) share.
#pragma once
#ifndef IBX_PROTO_H
#define IBX_PROTO_H

#include <google/protobuf/map.h>
#include <google/protobuf/message.h>

#include "TwsSocketClientErrors.h"
#include "ibx_runtime.h"

namespace ibx {
namespace pb {

// ── Values as a message carries them ──

// A number in a text field: the shortest digits that read back to it,
// with ".0" for a whole number.
std::string floatText(double v);
// A quantity in a text field: its plain decimal text.
std::string decimalText(Decimal v);
// A text that is a number, as a number; false when it is not one.
bool number(const std::string& text, double& out);
// The numbers and letters of the enumerations of the structures.
int exerciseCode(OptionExerciseType type);
const char* fundPolicyCode(FundDistributionPolicyIndicator policy);
const char* fundAssetCode(FundAssetType type);
// Whether the official client makes an error of this code itself.
bool clientError(int code);

// ── Request messages as the objects of the plain requests ──

// A number a structure keeps as a text: at most eight decimals, the zeros
// at the end cut; empty when unset.
std::string floatMaxText(double v);
// The contract of a message. The prices its legs carry go to `order`.
Contract contractOf(const protobuf::Contract& p, Order* order = nullptr);
// The order of a message, with its attached orders. False when a
// condition is of a kind the official client does not have: `refused`
// then says which.
bool orderOf(const protobuf::Order& p, const protobuf::AttachedOrders* attached, Order& o, std::string& refused);
// The options of a request, in the order of their names; null when none.
TagValueListSPtr tagsOf(const google::protobuf::Map<std::string, std::string>& map);

// ── The configuration ──

// The fields of a message that are set, as the text records the C entry
// layer takes for the configuration requests.
std::string recordsOf(const google::protobuf::Message& m);
// Sets the fields of a message from such records.
void fromRecords(ibx_str records, google::protobuf::Message& m);
// The answers of the configuration requests: their protobuf callbacks.
void configResponse(Sink& s, ibx_str fields);
void updateConfigResponse(Sink& s, ibx_str fields);

#include "ibx_proto_generated.inc"

}  // namespace pb
}  // namespace ibx

#endif
