"""Issue #208: the second-factor code provider of connect() is a Python
callable, called on its own thread during the login with the challenge as a
dict, returning the code. No server needed: the provider is run the way the
login runs it.
"""
import pytest

from ibx import EClient, EWrapper


def test_code_provider_gets_the_challenge_and_returns_the_code():
    seen = []

    def provider(challenge):
        seen.append(challenge)
        return "12345678"

    c = EClient(EWrapper())
    assert c._test_code_provider(provider, "580 820", "https://example.com/s") == "12345678"
    assert seen == [{"display_id": "580 820", "avth_url": "https://example.com/s"}]


def test_an_exception_of_the_code_provider_ends_the_login():
    def provider(challenge):
        raise ValueError("no code")

    c = EClient(EWrapper())
    with pytest.raises(RuntimeError, match="code_provider raised"):
        c._test_code_provider(provider, "1", "")
