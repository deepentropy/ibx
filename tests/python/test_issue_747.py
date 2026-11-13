"""ibx#747: two configuration settings ibx acts on after an update.

The zone the times are written in (`sendInstrumentTimezone`) and the account
values given as whole numbers (`roundAccountValuesToNearestWholeNumber`) are
taken: the update is answered "success" with the field named and reqConfig
gives the new value back (the reference 10.51 at level 225, paper, regular
hours 13/11/2026). What they change in the answers is tested in the Rust
unit tests of the session's configuration and was read on paper on both
sides.
"""

import pytest

from ibx import EClient, EWrapper


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.events = []

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.events.append(("error", req_id, error_code, error_string))

    def config_response_proto_buf(self, config_response_proto):
        self.events.append(("config", config_response_proto))

    def update_config_response_proto_buf(self, update_config_response_proto):
        self.events.append(("update", update_config_response_proto))


def connected():
    w = Recorder()
    c = EClient(w)
    c._test_connect("TEST123")
    c._test_apply_logon(None, "APIELOG,SECDEFTA", 199, None, None)
    return c, w


def test_the_time_zone_and_the_rounding_are_taken_and_given_back():
    UpdateConfigRequest = pytest.importorskip("ibapi.protobuf.UpdateConfigRequest_pb2").UpdateConfigRequest
    ConfigRequest = pytest.importorskip("ibapi.protobuf.ConfigRequest_pb2").ConfigRequest
    c, w = connected()

    def update(req_id, fill):
        r = UpdateConfigRequest()
        r.reqId = req_id
        fill(r.api.settings)
        c._test_update_config_at(225, r)
        c._test_dispatch_once()
        kind, answer = w.events.pop()
        assert kind == "update" and answer.reqId == req_id, (kind, answer)
        return answer

    def settings():
        c._test_req_config_at(225, ConfigRequest(reqId=7790))
        c._test_dispatch_once()
        kind, answer = w.events.pop()
        assert kind == "config"
        return answer.api.settings

    assert settings().sendInstrumentTimezone == "instrument"
    assert settings().roundAccountValuesToNearestWholeNumber is False
    a = update(7701, lambda s: setattr(s, "sendInstrumentTimezone", "UTC"))
    assert (a.status, list(a.changedFields)) == ("success", ["configuration.api.settings.sendInstrumentTimezone"])
    assert settings().sendInstrumentTimezone == "utc"
    a = update(7702, lambda s: setattr(s, "roundAccountValuesToNearestWholeNumber", True))
    assert (a.status, list(a.changedFields)) == ("success", ["configuration.api.settings.roundAccountValuesToNearestWholeNumber"])
    assert settings().roundAccountValuesToNearestWholeNumber is True
    # A zone the reference does not know is refused by its own check.
    a = update(7703, lambda s: setattr(s, "sendInstrumentTimezone", "local"))
    assert a.status != "success"
    assert settings().sendInstrumentTimezone == "utc"
    # A setting ibx does not act on is still refused with its name.
    a = update(7704, lambda s: setattr(s, "sendZeroPositionsForTodayOnly", True))
    assert a.status == "error" and list(a.errors) == ["configuration.api.settings.sendZeroPositionsForTodayOnly"]
    assert not w.events
