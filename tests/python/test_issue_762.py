"""ibx#762: a login with the password saves its restart token to ``restart_token_file``; a later ``connect()`` of the
same user on the same machine presents it instead of the password and the second factor.

Live, paper account: requires IB_USERNAME and IB_PASSWORD.
Run with: pytest tests/python/test_issue_762.py -v
"""

import os
import threading
import time

import pytest
from ibx import EClient, EWrapper

pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)


class Ids(EWrapper):
    def __init__(self):
        super().__init__()
        self.next_id = threading.Event()

    def next_valid_id(self, order_id):
        self.next_id.set()


def session(password, token_file):
    wrapper = Ids()
    client = EClient(wrapper)
    client.connect(username=os.environ["IB_USERNAME"], password=password,
                   host=os.environ.get("IB_HOST", "cdc1.ibllc.com"), paper=True,
                   restart_token_file=str(token_file))
    thread = threading.Thread(target=client.run, daemon=True)
    thread.start()
    assert wrapper.next_id.wait(30), "the session started"
    return client, thread


def test_a_later_connect_presents_the_saved_restart_token(tmp_path):
    token_file = tmp_path / "restart-token"
    client, thread = session(os.environ["IB_PASSWORD"], token_file)
    assert token_file.read_text().startswith("ibx-restart-token 1\n"), "the login saved its restart token"
    time.sleep(3)
    client.disconnect()
    thread.join(timeout=5)
    time.sleep(5)

    # A wrong password: only the saved restart token can let this login in.
    client, thread = session("not the password", token_file)
    client.disconnect()
    thread.join(timeout=5)
