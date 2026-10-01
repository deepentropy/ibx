"""ibx#426: server version and connection time after connect."""

import re

from ibx import EClient, EWrapper


def test_none_before_connect():
    c = EClient(EWrapper())
    assert c.server_version() is None
    assert c.tws_connection_time() is None


def test_after_connect_and_disconnect():
    c = EClient(EWrapper())
    c._test_connect("TEST123")
    assert c.server_version() == 214
    assert re.fullmatch(r"\d{8} \d{2}:\d{2}:\d{2} \S.*", c.tws_connection_time())
    c.disconnect()
    assert c.server_version() is None
    assert c.tws_connection_time() is None
