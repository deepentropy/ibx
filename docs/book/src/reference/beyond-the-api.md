# What the session states, and the API does not forward

This client speaks the protocol a gateway speaks. The gateway receives more
than it forwards: some of what the venue states at logon and on a contract has
no message in the API a gateway offers its own clients, so a program written
against that API has no way to ask for it.

Everything below is stated by the venue on an ordinary session. The figures are
from one paper session and will differ with the account; what does not differ is
that none of it is reachable through `ib_async` or the TWS API.

The examples are Python. Every one of these is on the Rust `EClient` under the
same name — see the [table at the end](#the-same-calls-in-rust).

## What the account may do

The venue states its grants at logon — one token per capability.

```python
grants = client.enabled_features()          # 302 on the session this was written from
"ISLAND2NASDAQ" in grants                   # True
```

They decide behaviour. `island_for_nasdaq` — whether a US stock trading on
Nasdaq is named by the older spelling — takes this grant as well as the
setting, which is how it is decided: the same token is read off
the same list at logon and holds it beside the setting.

## Which orders the venue will take

Stated at logon, per security type, before a single order is sent.

```python
perms = client.order_permissions()           # 21 security types
len(perms["STK"])                            # 92 order types and attributes
client.permitted_order_types("FUT")          # or None, if the type is not permitted at all
```

Through the API a program discovers this by being refused. A security type
absent from this map is one the venue answers with an Inactive order and no
text at all, which is why this client refuses it first and says why.

## Which algorithms this account may use

Keyed by provider and security type, also stated at logon.

```python
client.algorithms()                          # 13 keys: 'IBALGO/STK', 'FOXRIVER/STK', …
client.algorithms_for("STK")                 # ['FOXRIVER-AE', 'IBALGO-AE', 'JONES-AE', …]
```

An algorithm absent here is one an order naming it is refused for.

## Which order defaults the account holds

The venue keeps a set of order defaults per security type and fills parts of an
order the caller left unstated from them: the size a compete order competes
with and the offset it competes by, where neither was named. So the same call
on two accounts is not the same order.

```python
client.order_presets()      # [('s=CASH', 'v=1&a=1'), ('s=FUT', 'v=1&a=1'), ('s=STK', 'v=1&a=1'), …]
```

The key is the venue's own, and a currency can split one: `s=CASH&tc=EUR` sits
beside `s=CASH`. The version is what that set is on. The values inside a set
are asked for separately and are not carried here.

## What the session itself is

```python
client.get_account_id()                      # the account this session acts for
client.next_order_id()                       # the first id it may use
client.ccp_session_id()                      # what a web endpoint expects as a header
client.misc_url("region_dam")                # hosts the venue pushed at logon
```

## How far away the venue is

```python
client.req_ping()
client.last_rtt_ms()                         # 10.96 on the session this was written from
```

The API has no notion of this. A program that wants to know whether it is
close to the venue has to time a request that does something else.

## What the venue states about a contract, beyond its quote

A quote subscription can carry far more than prices. The venue publishes dozens
of series against a contract, each under a number of its own, and the API
forwards a handful of them. Ask for one by putting its number in the generic
tick list, then read what it stated.

Some carry the venue's own named fields as text — analyst ratings, ownership,
fund terms, screening scores, margin, technical readings, company accounts:

```python
client.req_mkt_data(1, contract, "454,505,631,678,705", False, False, [])
client.company_data_series(265598)      # [434, 454, 505, 548, 631, 678, 699, 705, …]
client.company_data(265598, 454)        # [('IITPCHL', '47.7348'), … ('IISHOS', '14594180000')]
client.company_data(756733, 505)        # [('EXPRATIO', '0.0945'), ('TOTALASSETVALUE', '811937033817.46'), …]
client.company_data(265598, 705)        # [('TRESGS', '7.10404'), ('TRESGCS', '3.65619'), …]
```

The keys are the venue's own and are handed on unchanged, so a field it adds
arrives with the rest instead of being dropped for not being recognised here.
One of these series answered with 212 fields on a single contract.

Others carry figures rather than text — a bond's accrued interest, what one
contract delivers, volatility, the volume a contract usually opens and closes
on, what margin a future takes:

```python
client.stated_figures_series(1)         # [398, 399, 402, 459, 493, 504, 527, 584, …]
client.stated_figures(1, 527)           # [0.0154349]   volatility over twenty days
client.stated_figures(1, 504)           # [383820.0, 1.0]  what one contract delivers
client.stated_figures(1, 407)           # [24340.8, 18338.7, 34772.6, 26198.1]
```

The figures arrive in the venue's order and at its widths, and a figure it holds
nothing for arrives as the largest number its field carries. Nothing is
republished under a tick number of this client's own choosing.

Four series number their figures themselves, in two tables — whole numbers and
fractional ones:

```python
client.numbered_figures(1, 562, fractional=False)   # [(7, 20260911.0), (30, 20260814.0), … (1826, 20210917.0)]
client.numbered_figures(1, 562, fractional=True)    # [(7, 765.96), (30, 776.34), … (1826, 419.586)]
client.numbered_figures(1, 757, fractional=True)    # [(30, 766.106)]  the average close over thirty days
```

The first of those is a price history: the number is a span of days, the whole
table holds the date each was taken and the fractional table the price on it,
back to five years — from a quote subscription, with no historical-data request.

Two more state a run of paired figures, and one carries the venue's option model
as the contract closed rather than as it stands — every greek it states,
including several the API has no field for:

```python
client.paired_figures(1, 546)           # [(coordinate, volatility), …]
client.closing_option_model(1)          # {'delta': …, 'rho': …, 'fugit': …, …}
```

### Series a subscription can be refused by silence

Not every series is served to every account. The venue acknowledges the
subscription, assigns it a tag, and then states nothing — it answers a series an
account cannot see with silence rather than with a refusal, so there is no error
to read. On the session this was written from, six behaved that way: 490, 546,
669, 700, 726 and 733. The same six on the same account, minutes apart, over
both a paper and a live login.

An empty result from any of the calls above is therefore two things at once: a
series the venue has nothing to say about for that contract, or one this account
is not entitled to. The subscription list in account management is what tells
them apart.

## Why this is not in the API

The API is a protocol between a gateway and a program on the same machine. What
the gateway does with any of this is its own and not visible from here; what is
visible is that none of it was ever framed as a message to forward. A
client that speaks the venue's protocol receives it directly, so it is here.

Two consequences worth stating plainly:

- **None of this is derived.** Every figure above is stated by the venue,
  read off the session. Nothing here computes what the venue did not say.
- **It is not portable.** A program using these calls will not run against a
  gateway, because a gateway has no message to carry them. They are the part of
  this client that is not a drop-in, and they are named again under
  [Limits](./limits.md) for that reason.

## The same calls in Rust

| Python | Rust |
| --- | --- |
| `client.enabled_features()` | `client.enabled_features()` |
| `client.order_permissions()` | `client.order_permissions()` |
| `client.permitted_order_types(sec_type)` | `client.permitted_order_types(sec_type)` |
| `client.algorithms()` | `client.algorithms()` |
| `client.algorithms_for(sec_type)` | `client.algorithms_for(sec_type)` |
| `client.get_account_id()` | `client.account_id` (a field) |
| `client.next_order_id()` | `client.next_order_id()` |
| `client.ccp_session_id()` | `client.ccp_session_id()` |
| `client.misc_url(key)` | `client.misc_url(key)` |
| `client.company_data(con_id, series)` | `client.company_data(con_id, series)` |
| `client.company_data_series(con_id)` | `client.company_data_series(con_id)` |
| `client.stated_figures(req_id, series)` | `client.stated_figures(req_id, series)` |
| `client.stated_figures_series(req_id)` | `client.stated_figures_series(req_id)` |
| `client.numbered_figures(req_id, series, fractional)` | `client.numbered_figures(req_id, series, fractional)` |
| `client.numbered_figures_series(req_id)` | `client.numbered_figures_series(req_id)` |
| `client.paired_figures(req_id, series)` | `client.paired_figures(req_id, series)` |
| `client.paired_figures_series(req_id)` | `client.paired_figures_series(req_id)` |
| `client.closing_option_model(req_id)` | `client.closing_option_model(req_id)` |
| `client.req_ping()` | `client.req_ping()` |
| `client.last_rtt_ms()` | `client.shared_state().last_ccp_rtt()`, a `Duration` |
