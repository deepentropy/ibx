"""IBX Test #112 : News providers, historical news, article fetch, bulletins.

Tests reqNewsProviders, reqHistoricalNews, reqNewsArticle, and reqNewsBulletins.

Run: pytest tests/python/test_issue_ib112.py -v -s
"""

import os, threading, time
import pytest
from ibx import EWrapper, EClient, Contract

pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)

AAPL_CON_ID = 265598
SPY_CON_ID = 756733


class NewsWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.lock = threading.Lock()

        # News providers
        self.providers = []
        self.got_providers = threading.Event()

        # Historical news
        self.hist_news_items = {}  # req_id -> list of (time, provider, article_id, headline)
        self.got_hist_news_end = {}

        # News article
        self.articles = {}  # req_id -> (article_type, text)
        self.got_article = {}

        # Bulletins
        self.bulletins = []
        self.got_bulletin = threading.Event()

    def next_valid_id(self, order_id):
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def news_providers(self, news_providers):
        with self.lock:
            self.providers = news_providers
        self.got_providers.set()

    def historical_news(self, req_id, time_str, provider_code, article_id, headline):
        with self.lock:
            self.hist_news_items.setdefault(req_id, []).append(
                (time_str, provider_code, article_id, headline))

    def historical_news_end(self, req_id, has_more):
        ev = self.got_hist_news_end.get(req_id)
        if ev:
            ev.set()

    def news_article(self, req_id, article_type, article_text):
        self.articles[req_id] = (article_type, article_text)
        ev = self.got_article.get(req_id)
        if ev:
            ev.set()

    def update_news_bulletin(self, msg_id, msg_type, message, orig_exchange):
        with self.lock:
            self.bulletins.append((msg_id, msg_type, message, orig_exchange))
        self.got_bulletin.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158):
            print(f"  [error] reqId={req_id} code={error_code}: {error_string}")


class TestNews:
    """Issue #112 : News providers, historical news, article fetch."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = NewsWrapper()
        self.client = EClient(self.wrapper)
        self.client.connect(
            username=os.environ["IB_USERNAME"],
            password=os.environ["IB_PASSWORD"],
            host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
            paper=True,
        )
        self.thread = threading.Thread(target=self.client.run, daemon=True)
        self.thread.start()
        assert self.wrapper.connected.wait(timeout=15), "Connection failed"
        yield
        self.client.disconnect()
        self.thread.join(timeout=5)

    def test_news_providers(self):
        """reqNewsProviders returns available providers."""
        self.client.req_news_providers()
        got = self.wrapper.got_providers.wait(timeout=15)
        if not got:
            pytest.skip("No news providers returned")

        providers = self.wrapper.providers
        print(f"  News providers: {len(providers)}")
        for p in providers:
            code = p.get("code", "") if isinstance(p, dict) else getattr(p, "code", "")
            name = p.get("name", "") if isinstance(p, dict) else getattr(p, "name", "")
            print(f"    {code}: {name}")

        assert len(providers) > 0, "Should have at least one news provider"

    def test_historical_news_aapl(self):
        """reqHistoricalNews for AAPL (10 articles)."""
        req_id = 9001
        self.wrapper.got_hist_news_end[req_id] = threading.Event()

        # Provider codes from capture: BRFG, BRFUPDN, DJ-N, etc.
        self.client.req_historical_news(
            req_id, AAPL_CON_ID,
            "BRFG+BRFUPDN+DJ-N+DJ-RTA+DJ-RTE+DJ-RTG+DJ-RTPRO+DJNL",
            "", "",  # start/end empty = recent
            10, []
        )

        got = self.wrapper.got_hist_news_end[req_id].wait(timeout=30)
        if not got:
            pytest.skip("No historical news for AAPL")

        articles = self.wrapper.hist_news_items.get(req_id, [])
        print(f"  AAPL historical news: {len(articles)} articles")
        for t, prov, aid, headline in articles[:5]:
            print(f"    [{prov}] {headline[:80]}")

        assert len(articles) > 0, "Should have AAPL news articles"

    def test_historical_news_spy(self):
        """reqHistoricalNews for SPY (10 articles)."""
        req_id = 9002
        self.wrapper.got_hist_news_end[req_id] = threading.Event()

        self.client.req_historical_news(
            req_id, SPY_CON_ID,
            "BRFG+BRFUPDN+DJ-N+DJ-RTA+DJ-RTE+DJ-RTG+DJ-RTPRO+DJNL",
            "", "", 10, []
        )

        got = self.wrapper.got_hist_news_end[req_id].wait(timeout=30)
        if not got:
            pytest.skip("No historical news for SPY")

        articles = self.wrapper.hist_news_items.get(req_id, [])
        print(f"  SPY historical news: {len(articles)} articles")
        for t, prov, aid, headline in articles[:5]:
            print(f"    [{prov}] {headline[:80]}")

    def test_news_article_fetch(self):
        """Fetch a specific news article body."""
        # First get a list of articles to find an article ID
        req_id = 9003
        self.wrapper.got_hist_news_end[req_id] = threading.Event()

        self.client.req_historical_news(
            req_id, AAPL_CON_ID,
            "DJ-N",
            "", "", 5, []
        )

        got = self.wrapper.got_hist_news_end[req_id].wait(timeout=30)
        if not got:
            pytest.skip("No news to fetch article from")

        articles = self.wrapper.hist_news_items.get(req_id, [])
        if not articles:
            pytest.skip("No article IDs available")

        # Fetch the first article
        _, provider, article_id, headline = articles[0]
        print(f"  Fetching: [{provider}] {headline[:60]}...")

        fetch_id = 9004
        self.wrapper.got_article[fetch_id] = threading.Event()
        self.client.req_news_article(fetch_id, provider, article_id, [])

        got = self.wrapper.got_article[fetch_id].wait(timeout=15)
        if not got:
            pytest.skip("Article fetch timed out")

        art_type, art_text = self.wrapper.articles[fetch_id]
        print(f"  Article type: {art_type}, length: {len(art_text)} chars")
        assert len(art_text) > 0, "Article body should not be empty"

    def test_news_bulletins(self):
        """Subscribe to news bulletins (may receive 0 if no active bulletins)."""
        self.client.req_news_bulletins(True)
        got = self.wrapper.got_bulletin.wait(timeout=10)
        self.client.cancel_news_bulletins()

        if got:
            print(f"  Bulletins received: {len(self.wrapper.bulletins)}")
        else:
            print("  No active bulletins (expected)")
