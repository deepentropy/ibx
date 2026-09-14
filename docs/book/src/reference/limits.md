# Limits

What a caller has to know before writing against this client, sorted by what
kind of thing it is — because most of what lands on a page like this is not a
limit of the client at all.

Nothing here is a call that returns as though it acted. A call this protocol
cannot carry reports why.

# The venue's answer, not this client's

A gateway answers every one of these the same way. They are written down
because a program meeting one for the first time reads it as a fault in the
client, and checking the wrong thing costs a session.

## A bid of -1 is the venue saying there is none

Some instruments carry no bid and no ask. The venue says so by sending `-1` on
both, and this client passes it on as sent rather than turning it into a zero.

Which instruments those are is the venue's to say and not something this client
can work out. Indices are where it was met: most of the ones below carry no
quote, and one of them does, so the kind of instrument is not the rule.

It is not an entitlement. Three things separate them, and all three are worth
checking before concluding a subscription is missing:

* **Not entitled** — no ticks at all, and no `market_data_type` callback
  either. Nothing arrives to say what feed you are on, because you are on
  none. Asking for delayed data then answers normally.
* **Entitled, and the instrument has no quote** — `-1` on bid and ask, no bid
  or ask *size*, and a `market_data_type` of 1 saying the feed is real time,
  while the last, high, low, close and open arrive and move.
* **Entitled, and it is quoted** — a real bid and ask.

Measured together with the market open: `SPX` on `CBOE` quotes; `VIX` on the
same exchange and the same entitlement does not, and reads `-1` while its
volume ticks up on a real-time feed. `RUT`, `INDU` and `TICK-NYSE` read `-1`
the same way. `NDX` real time is the first case — silence, no feed stated —
and it answers as soon as delayed data is asked for.

Delayed data carries no index quote at all: on `market_data_type` 3 every one
of them reads `-1`, `SPX` included. A quote that is on the real-time feed and
not the delayed one is a property of the feed rather than of the account.

## Market depth depends on the entitlement

A book is asked for at a named venue, and a level carries the maker the venue
names on it — empty where it names none, which is the ordinary shape on a venue
that quotes no makers. Which venues answer is the account's entitlement, not
this client's:

* A venue the account is not entitled to refuses by name, and the refusal
  reaches the caller.
* A book asked for on no particular venue is acknowledged and then produces
  nothing, which is what an account with no aggregate entitlement is answered
  with. It does not error.

Check what came back rather than assuming a subscription that was accepted is a
subscription that will deliver. Measured on one account with the market open:
a share on ARCA, a share on ISLAND and a future on CME are each refused by
name, while `EUR.USD`, `GBP.USD` and `USD.JPY` on `IDEALPRO` all deliver, and
a twenty-level book delivers as readily as a five-level one. Smart depth on a
currency produces nothing, which is the venue saying a currency has one venue
and nothing to aggregate.

A book of the size asked for stays that size. The venue sends every level it
holds and says nothing about the one that moves when a level inside the asked
size goes away, so this client keeps the book and states that move itself: a
withdrawal inside the book is followed by the level that came up into the place
it left, and a level arriving inside it is preceded by the withdrawal of the one
it pushed out. A caller applying the operations in the order they arrive holds
the size it asked for.

## A broad lookup takes longer than one contract

A lookup naming a whole class is a different question from one naming a single
contract. `SPY` options across every expiry is 13,580 definitions over nineteen
exchanges, and the venue takes about ten seconds to send the first of them.

The wait measures silence, not the length of an answer: it is reset each time
the venue speaks, so an answer that runs for as long as that one does is not cut
off part-way through. A lookup that does run out says how many definitions
arrived before it did, since a partial answer and no answer are different facts
and only the first says to ask a narrower question — by naming an expiry, or a
single venue.

## What a crypto order needs

A crypto is quoted around the clock and priced and sized differently from a
share.

| | |
| --- | --- |
| Time in force | Immediate-or-cancel, or the one measured in minutes. A day order is refused: *"The crypto buy order must be Minutes or IOC"* |
| Price | On the venue's grid. One that is not is refused as a price, *"Invalid Price"*, rather than rounded — this client sends prices as they were given |
| Quantity | A fraction, counted in hundred-millionths. A thousandth of a coin is an ordinary size |

## Things an entitlement decides, not this client

* **News headlines** need a news subscription. Without one, the providers list
  is returned and every query comes back empty.
* **Corporate events content** needs a Wall Street Horizon subscription. The
  calendar's schema and event types are delivered either way; the events
  themselves come back empty without it.

# Not settled here yet

Something the protocol may well carry, which no session has established. Each
says what would settle it. None of them is a call that returns as though it
acted: a request this client will not send says so.

## One order a modify cannot restate

A modify is a full statement of the order, rebuilt from what was placed. One
kind cannot be stated that way and the call is refused rather than sent: a
what-if preview, which is a margin preview rather than a resting order, so
there is nothing on the book for a replace to act on.

Everything else is replaced as itself, including the ones that carry more than
a type and a price: hidden, all-or-none, iceberg, discretionary, sweep-to-fill,
an OCA group, a good-till date, a bracket child, an algo, a conditional order,
an adjustable stop, and the trailing, pegged, midpoint and limit-if-touched
types. An adjustable stop is an ordinary stop defined by what it becomes, and
the replace states that conversion again: the marker, the type it adjusts to,
the trigger, the adjusted stop, its limit and the trailing amount, the same
numbers the placement wrote and in the same order. A modify of one
of those names its defining number in the field the placement used: a trail, a
peg offset or a snap offset on the auxiliary price, a cap on the limit price, a
trailing stop limit's limit offset on `lmtPriceOffset`. The one number a modify
cannot move is a trailing percent, and a modify naming a new one is refused
rather than sent. A modify of an order this session did not place — one the
venue named at connect — is restated from the caller's own statement of it,
which is what the reference client sends — links included, on every replace,
so a statement that omits the group drops it at the venue whatever the type,
as it does on the reference client. A change of type on an order with a
parent or a group is refused: the replace carries neither across a change of
type, and the venue reads their absence as their removal. A replace stating a
parent or a group other than the one an order placed here was placed with is
refused too, for the same reason.

A relative order is refused a modify as well. It answers both ways: sometimes
the venue takes the replace and the order goes on working, and sometimes
neither the replace nor a withdrawal after it draws any answer at all. A modify
that strands the order some of the time is worse than one that is refused.

## Implied volatility and option price

The venue computes its option model and publishes it per option, on a
subscription of its own. Volatility and greeks read off a quote are the venue's
own numbers.

What this protocol carries no request for is the inversion: an option price or a
volatility the caller supplies, for the venue to work back from. That one is
solved here, against the venue's published model for the same contract.

Where the venue has published no model, nothing is answered — rather than a
number derived from a rate nobody stated. Asked before the model has arrived,
the question waits on the subscription that asking opens.

## Numbered ticks this client does not deliver

Ninety-six numbered market-data ticks reach a caller of the reference client.
Eighty-seven of them reach one here. These nine do not, and each for a reason
that can be checked rather than taken on trust.

| Ticks | What | Why not |
| --- | --- | --- |
| 10, 11, 12 and the delayed 80, 81, 82 | The option model struck against the bid, the ask and the last | Not sent. The reference terminal solves each of them itself: for a side it takes that side's option price and finds the volatility at which the model prices the option there, then evaluates the greeks at it. The venue sends one model per option and that one is delivered, on 13 and on 83 where the feed is delayed. This client's own model now reads the venue's schedule of ex-dates and reproduces the venue's published price exactly, its delta within half a per cent and its gamma within one; what one day costs is within seven. What one point of volatility is worth is not, and the venue's own figures cannot settle how far out it is — at one strike its call and its put state vegas seven per cent apart, which is further than this client is from either. Until that is settled, publishing these would be this client's arithmetic wearing the venue's name |
| The delayed 103, 104 | Bid and ask yield on a delayed feed | The yields themselves arrive and are delivered, on 50, 51 and 52. The delayed feed states its prices under numbers of its own and no session here has seen it state a yield |
| 85 | The moment a chargeable snapshot was taken | Nobody sends it. The venue states no such figure, and the reference client publishes no tick under this number either: every numbered tick it delivers is one it was told, and this is not among them. A number invented here would be a time nobody recorded |

Everything else the venue publishes and a caller can ask for is delivered, on
the callback the reference client delivers it on.

# Where this client behaves differently

Not a gap in what it can ask for. A program written against the other
behaviour will still be wrong, which is why they are here.

## Callbacks nothing fires

One callback exists so a program written against the reference client compiles
and runs. No message reaches it.

| Callback | Why |
| --- | --- |
| `delta_neutral_validation` | Nothing produces it. Not "nothing on this client's connections": nothing in the protocol at all reaches the sender, neither a market-data request nor a placed order carrying a hedge, so there is no first request after which it could honestly be fired |

`order_bound` was the other, and the reason given for it was that the work had
not been done rather than that it could not be: an order the caller did not
place is given a number here when the venue replays it at connect, and the
pairing of that number with the permanent id the venue keeps is exactly what
the callback carries. It is stated now, once per pairing — the venue replays
the same orders on every reconnect, and a caller counting them would be
counting reconnects.

Every other call in the reference client's surface is served on both languages.
The call-by-call matrix is [generated from the source](./coverage.md).

## Order fields the protocol has nowhere to put

An order carries 154 fields. 118 go out under a tag. 29 have no field in this
protocol to carry them, and each says so on itself rather than being quietly
dropped. 6 more are what the venue fills in on the way back, which an order
being placed does not carry out.

The 29 are not a gap in this client. The protocol numbers 288 order fields, and
not one of them carries a basis-point offset, a bond's accrued interest, an
auction strategy, an origin, a shareholder, a smart-combo routing parameter, a
scale table, a price randomisation, a what-if kind, a parent's permanent id, a
bracket preset's legs, the hedging leg's clearing, settling, short-sale or
designated location, or the percentage constraints an order would set aside.
They are fields a caller can state and nothing on the other side can receive,
which is the same answer a gateway gives.

One of them was settled the other way round, on a session rather than on the
vocabulary: a caller's own name for an algo has a number in the protocol, and
the venue refuses an order carrying it — *"Invalid value in field # 8016"* —
whether or not the order runs an algo.

None is silently dropped, and that is checked rather than claimed:
`python scripts/gen_order_field_reach.py` recounts all four figures from the
order builders and exits non-zero if any field becomes settable and unread.

## An account summary is this session's account, whatever group is named

`reqAccountSummary` takes a group. The venue selects which accounts a summary
covers from that group and from the model code beside it, and answers with the
rows it picked. This client does neither: it filters the account stream it is
already receiving for the tags the caller asked for, and the group and the
model are taken and not applied.

For a login holding one account and no model that is the same answer, which is
the case this was written and measured against. For an advisor login where a
group names accounts beyond the one this session holds, it is not: the summary
covers the session's account rather than the group's members.

The rows themselves are encoded the same way either way. What differs is which
accounts they are for.

## Executions and fills are the day's, not the account's

`fills()`, `executions()` and `reqExecutions()` answer with the executions this
session has seen **and the ones the venue restated when the session opened**.

The venue restates the day's executions at every logon. Those are filed for a
caller that asks and announced to nobody — a restarted program is answered with
the fills it made before it restarted, including fills on orders that had
already completed, which it never tracked and knows nothing else about.

What is still absent is anything the venue does not restate. The wrapper this
follows asks the venue for the account's executions, whoever made them and
whenever; this protocol carries no such question, and the filter's time is a
test applied here rather than a question put to the venue. So an answer holds
today's executions on this account, not its history. A program reconciling
against more than a day needs another source for it, and an empty answer means
the venue restated none and this session has seen none.

`reqCompletedOrders` is not in that position. It asks the venue, which answers
with what it has finished rather than with what this session watched finish,
and the call waits for that answer before handing it back.

## 4,096 instruments at a time

The engine holds a slot for 4,096 distinct contracts concurrently. Registering
one past that is refused with a message naming the limit.

Concurrent, not cumulative: cancelling a market-data subscription frees its slot
and the slot is reused. A long-running process that subscribes and never cancels
will reach it; one that cancels what it is done with will not.

The number is this client's own allocation, not a limit the venue states, and
it is not the one a caller meets first. The venue states how many quote lines
the account may hold on the logon, and this client counts its open streams
against that allowance: a subscription past it is refused for want of a line,
which happens well before a slot runs out.

The slot table is sized well clear of it. One option chain asked for at once is
282 live subscriptions on a single underlying, and the venue served all of
them.

## A recovery attempt outlives the call that stops it, and opens nothing

`disconnect` stops the engine and returns. An attempt to reopen a connection
that was already dialling when it did is told to stop — a flag every worker
reads between the phases of a handshake — but it is not interrupted inside one,
so it finishes whatever call it is in before it reads the flag.

Nothing it opens is used. The trading connection's attempt is waited for, and a
session that landed after the stop is logged out rather than dropped: on this
protocol an authenticated session is a session open at the venue, and somebody
may have approved a second factor for it. The other three are not waited for.
Their socket closes when the receiver that would have taken it is gone, and the
engine refuses to install a connection that arrives after the stop.

What the window costs a live login has not been measured. A paper session
presents no second factor, so nothing on this account reaches the case where it
would.

# What this client has that a gateway does not

## Calls with nowhere to go on a gateway

The session data on [what the API does not forward](./beyond-the-api.md) — the
account's grants, the order types the venue will take, its algorithms, the
round-trip time — reaches this client because it is on the wire the venue
speaks, and has no message in the API a gateway offers.

It is a superset, not a gap: nothing a program asks a gateway for is missing
here for this reason. What it costs is the other direction. A program that
calls one of these runs here and does not run against a gateway, so a program
that has to run against both leaves them alone.

# Not possible from here

Two, and neither is a matter of work left to do.

## What authenticates a farm connection

A market-data or trading connection is opened with a request carrying the
session's own token, and the venue may answer it in one of two ways: by asking
this session to authenticate in full, or by acknowledging the logon against the
token it was already given. Both are the venue accepting a credential.

Where it asks in full, the answer it sends back is checked: the venue states a
proof of the session key that only a party holding the account's verifier can
compute, and a logon whose proof does not match is refused. The group that
exchange runs in is this venue's own and no other, because a peer names it
before it has proved anything.

What none of that establishes is that the party holding the channel keys is the
party that answered the logon. The logon runs beside the channel rather than
inside it, so a peer that relays it to the venue in real time collects a proof
it did not compute. Binding the two needs the venue to state something over
both, and nothing on this wire does.

## The protocol is not published

This client speaks a protocol the venue does not document and can change
without notice. That is the standing risk of not running the vendor's own
process, and no amount of testing removes it. What the repository does about it
is regenerate the coverage matrices from the source on every commit and fail the
build when a claim stops matching the code.
