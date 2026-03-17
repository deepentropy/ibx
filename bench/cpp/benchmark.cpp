#define NOMINMAX
// IB Gateway Benchmark — Official TWS API via IB Gateway
//
// Measures the same 4 metrics as the Rust engine benchmark:
// 1. Connection time (connect to IB Gateway on localhost:4002)
// 2. Tick decode latency (tickPrice/tickSize callback timing)
// 3. Hot loop iteration (inter-tick time / message processing throughput)
// 4. Order round-trip (submit market order → execDetails callback)
//
// Usage:
//   ib_benchmark.exe [host] [port] [con_id] [collect_ticks] [order_mode]
//   Defaults: "" 4002 756733 10000 0
//   order_mode: 0=none, 1=market (needs market hours), 2=limit submit+cancel

#include "StdAfx.h"
#include "EClientSocket.h"
#include "EReader.h"
#include "EReaderOSSignal.h"
#include "DefaultEWrapper.h"
#include "Contract.h"
#include "Order.h"
#include "Execution.h"

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <vector>
#include <algorithm>
#include <string>
#include <atomic>
#include <numeric>

using Clock = std::chrono::steady_clock;
using TimePoint = Clock::time_point;
using Duration = std::chrono::nanoseconds;

// ─── Helpers ───

static std::string format_ns(uint64_t ns) {
    char buf[64];
    if (ns < 1000ULL) {
        snprintf(buf, sizeof(buf), "%lluns", (unsigned long long)ns);
    } else if (ns < 1000000ULL) {
        snprintf(buf, sizeof(buf), "%.1fus", ns / 1000.0);
    } else if (ns < 1000000000ULL) {
        snprintf(buf, sizeof(buf), "%.2fms", ns / 1000000.0);
    } else {
        snprintf(buf, sizeof(buf), "%.3fs", ns / 1000000000.0);
    }
    return buf;
}

static uint64_t percentile(std::vector<uint64_t>& sorted, double p) {
    if (sorted.empty()) return 0;
    size_t idx = std::min((size_t)(sorted.size() * p), sorted.size() - 1);
    return sorted[idx];
}

static double elapsed_secs(TimePoint start) {
    auto ns = std::chrono::duration_cast<Duration>(Clock::now() - start).count();
    return ns / 1e9;
}

// ─── Benchmark phases ───

enum Phase : int {
    WARMUP = 0,
    COLLECTING = 1,
    ORDER_BUY = 2,
    WAIT_BUY = 3,
    WAIT_SELL = 4,
    // Limit order phases
    LIMIT_SUBMIT = 10,
    WAIT_LIMIT_ACK = 11,
    LIMIT_CANCEL = 12,
    WAIT_LIMIT_CANCEL = 13,
    DONE = 99,
};

// ─── Benchmark EWrapper ───

class BenchmarkWrapper : public DefaultEWrapper {
public:
    // Config
    int con_id;
    int warmup_ticks;
    int collect_ticks;
    int order_mode; // 0=none, 1=market, 2=limit

    TimePoint start;

    // Phase
    std::atomic<int> phase{WARMUP};
    std::atomic<bool> done{false};

    // Samples
    std::vector<uint64_t> decode_latencies_ns;
    std::vector<uint64_t> inter_tick_ns;

    // Tick tracking
    TimePoint last_tick_time{};
    bool has_last_tick = false;
    int tick_count = 0;

    // Market order tracking
    OrderId next_order_id = 0;
    TimePoint buy_submit_time{};
    TimePoint sell_submit_time{};
    uint64_t buy_rtt_ns = 0;
    uint64_t sell_rtt_ns = 0;
    bool buy_filled = false;
    bool sell_filled = false;

    // Limit order tracking
    OrderId limit_order_id = 0;
    TimePoint limit_submit_time{};
    TimePoint limit_cancel_time{};
    uint64_t limit_ack_rtt_ns = 0;
    uint64_t limit_cancel_rtt_ns = 0;
    bool limit_acked = false;
    bool limit_cancelled = false;

    // Connection
    TimePoint connect_time{};
    uint64_t connect_duration_ns = 0;
    bool connected = false;

    // Message-level timing: set before each processMessages batch
    TimePoint batch_start{};

    // Client reference for placing orders
    EClientSocket* client = nullptr;

    BenchmarkWrapper(int con_id, int warmup, int collect, int order_mode)
        : con_id(con_id)
        , warmup_ticks(warmup)
        , collect_ticks(collect)
        , order_mode(order_mode)
        , start(Clock::now())
    {
        decode_latencies_ns.reserve(collect);
        inter_tick_ns.reserve(collect);
    }

    // Called when connection handshake completes
    void connectAck() override {
        connected = true;
        connect_duration_ns = std::chrono::duration_cast<Duration>(
            Clock::now() - connect_time).count();
        printf("[%.3fs] Connected to IB Gateway in %s\n",
            elapsed_secs(start), format_ns(connect_duration_ns).c_str());
    }

    void nextValidId(OrderId orderId) override {
        next_order_id = orderId;
        printf("[%.3fs] Next valid order ID: %ld\n", elapsed_secs(start), orderId);

        // Subscribe to market data
        Contract contract;
        contract.conId = con_id;
        contract.exchange = "SMART";
        contract.secType = "STK";
        contract.currency = "USD";
        client->reqMktData(1, contract, "", false, false, {});
        printf("[%.3fs] Subscribed to conId=%d\n", elapsed_secs(start), con_id);
    }

    void tickPrice(TickerId tickerId, TickType field, double price,
                   const TickAttrib& attrib) override {
        process_tick();
    }

    void tickSize(TickerId tickerId, TickType field, Decimal size) override {
        process_tick();
    }

    void process_tick() {
        auto now = Clock::now();
        int p = phase.load(std::memory_order_relaxed);

        switch (p) {
        case WARMUP: {
            if (tick_count == 0) {
                printf("[%.3fs] First tick received, warming up (%d ticks)...\n",
                    elapsed_secs(start), warmup_ticks);
            }
            tick_count++;
            if (tick_count >= warmup_ticks) {
                tick_count = 0;
                phase.store(COLLECTING, std::memory_order_relaxed);
                printf("[%.3fs] Collecting %d tick samples...\n",
                    elapsed_secs(start), collect_ticks);
            }
            break;
        }

        case COLLECTING: {
            // Decode latency: time from processMessages entry to this callback
            uint64_t decode_ns = std::chrono::duration_cast<Duration>(
                now - batch_start).count();
            decode_latencies_ns.push_back(decode_ns);

            // Inter-tick time
            if (has_last_tick) {
                uint64_t inter = std::chrono::duration_cast<Duration>(
                    now - last_tick_time).count();
                inter_tick_ns.push_back(inter);
            }
            last_tick_time = now;
            has_last_tick = true;

            tick_count++;
            if (tick_count % 2000 == 0) {
                printf("[%.3fs] Collected %d/%d samples...\n",
                    elapsed_secs(start), tick_count, collect_ticks);
            }

            if (tick_count >= collect_ticks) {
                if (order_mode == 1) {
                    phase.store(ORDER_BUY, std::memory_order_relaxed);
                } else if (order_mode == 2) {
                    phase.store(LIMIT_SUBMIT, std::memory_order_relaxed);
                } else {
                    phase.store(DONE, std::memory_order_relaxed);
                    done.store(true, std::memory_order_relaxed);
                }
            }
            break;
        }

        case ORDER_BUY: {
            printf("[%.3fs] Submitting market BUY 1 share...\n", elapsed_secs(start));

            Contract contract;
            contract.conId = con_id;
            contract.exchange = "SMART";
            contract.secType = "STK";
            contract.currency = "USD";

            Order order;
            order.action = "BUY";
            order.totalQuantity = stringToDecimal("1");
            order.orderType = "MKT";

            buy_submit_time = Clock::now();
            client->placeOrder(next_order_id++, contract, order);
            phase.store(WAIT_BUY, std::memory_order_relaxed);
            break;
        }

        case LIMIT_SUBMIT: {
            printf("[%.3fs] Submitting limit BUY 1 share at $1.00...\n", elapsed_secs(start));

            Contract contract;
            contract.conId = con_id;
            contract.exchange = "SMART";
            contract.secType = "STK";
            contract.currency = "USD";

            Order order;
            order.action = "BUY";
            order.totalQuantity = stringToDecimal("1");
            order.orderType = "LMT";
            order.lmtPrice = 1.00;
            order.tif = "GTC"; // GTC works after hours

            limit_order_id = next_order_id++;
            limit_submit_time = Clock::now();
            client->placeOrder(limit_order_id, contract, order);
            phase.store(WAIT_LIMIT_ACK, std::memory_order_relaxed);
            break;
        }

        default:
            break;
        }
    }

    void orderStatus(OrderId orderId, const std::string& status, Decimal filled,
                     Decimal remaining, double avgFillPrice, int permId, int parentId,
                     double lastFillPrice, int clientId, const std::string& whyHeld,
                     double mktCapPrice) override {
        int p = phase.load(std::memory_order_relaxed);
        printf("[%.3fs] orderStatus: id=%ld status=%s\n",
            elapsed_secs(start), orderId, status.c_str());

        // Limit order: wait for Submitted/PreSubmitted ack
        if (p == WAIT_LIMIT_ACK && orderId == limit_order_id && !limit_acked) {
            if (status == "Submitted" || status == "PreSubmitted") {
                auto now = Clock::now();
                limit_ack_rtt_ns = std::chrono::duration_cast<Duration>(
                    now - limit_submit_time).count();
                limit_acked = true;
                printf("[%.3fs] Limit order ACKED in %s\n",
                    elapsed_secs(start), format_ns(limit_ack_rtt_ns).c_str());

                // Now cancel it
                printf("[%.3fs] Cancelling limit order %ld...\n",
                    elapsed_secs(start), limit_order_id);
                limit_cancel_time = Clock::now();
                client->cancelOrder(limit_order_id, "");
                phase.store(WAIT_LIMIT_CANCEL, std::memory_order_relaxed);
            }
        }

        // Limit order: wait for Cancelled
        if (p == WAIT_LIMIT_CANCEL && orderId == limit_order_id && !limit_cancelled) {
            if (status == "Cancelled") {
                auto now = Clock::now();
                limit_cancel_rtt_ns = std::chrono::duration_cast<Duration>(
                    now - limit_cancel_time).count();
                limit_cancelled = true;
                printf("[%.3fs] Limit order CANCELLED in %s\n",
                    elapsed_secs(start), format_ns(limit_cancel_rtt_ns).c_str());

                phase.store(DONE, std::memory_order_relaxed);
                done.store(true, std::memory_order_relaxed);
            }
        }
    }

    void execDetails(int reqId, const Contract& contract,
                     const Execution& execution) override {
        int p = phase.load(std::memory_order_relaxed);

        if (p == WAIT_BUY && !buy_filled) {
            auto now = Clock::now();
            buy_rtt_ns = std::chrono::duration_cast<Duration>(
                now - buy_submit_time).count();
            buy_filled = true;
            printf("[%.3fs] BUY filled in %s\n",
                elapsed_secs(start), format_ns(buy_rtt_ns).c_str());

            // Submit sell to close
            printf("[%.3fs] Submitting market SELL 1 share...\n", elapsed_secs(start));
            Contract c;
            c.conId = con_id;
            c.exchange = "SMART";
            c.secType = "STK";
            c.currency = "USD";

            Order order;
            order.action = "SELL";
            order.totalQuantity = stringToDecimal("1");
            order.orderType = "MKT";

            sell_submit_time = Clock::now();
            client->placeOrder(next_order_id++, c, order);
            phase.store(WAIT_SELL, std::memory_order_relaxed);
        }
        else if (p == WAIT_SELL && !sell_filled) {
            auto now = Clock::now();
            sell_rtt_ns = std::chrono::duration_cast<Duration>(
                now - sell_submit_time).count();
            sell_filled = true;
            printf("[%.3fs] SELL filled in %s\n",
                elapsed_secs(start), format_ns(sell_rtt_ns).c_str());

            phase.store(DONE, std::memory_order_relaxed);
            done.store(true, std::memory_order_relaxed);
        }
    }

    void error(int id, int errorCode, const std::string& errorString,
               const std::string& advancedOrderRejectJson) override {
        // Suppress non-critical messages (market data farm connections, etc.)
        if (errorCode == 2104 || errorCode == 2106 || errorCode == 2158) return;
        printf("[%.3fs] ERROR id=%d code=%d: %s\n",
            elapsed_secs(start), id, errorCode, errorString.c_str());
    }

    void connectionClosed() override {
        printf("[%.3fs] Connection closed\n", elapsed_secs(start));
        done.store(true, std::memory_order_relaxed);
    }

    // ─── Report ───

    void print_report() {
        printf("\n");
        printf("========================================\n");
        printf("  IB Gateway (TWS API) Benchmark Results\n");
        printf("========================================\n");
        printf("\n");

        printf("CONNECTION\n");
        printf("  Total:          %s\n", format_ns(connect_duration_ns).c_str());
        printf("\n");

        if (!decode_latencies_ns.empty()) {
            auto& s = decode_latencies_ns;
            std::sort(s.begin(), s.end());
            size_t n = s.size();
            uint64_t sum = std::accumulate(s.begin(), s.end(), 0ULL);
            uint64_t mean = sum / n;

            printf("TICK DECODE LATENCY (%zu samples)\n", n);
            printf("  Min:            %s\n", format_ns(s[0]).c_str());
            printf("  P50:            %s\n", format_ns(percentile(s, 0.50)).c_str());
            printf("  P95:            %s\n", format_ns(percentile(s, 0.95)).c_str());
            printf("  P99:            %s\n", format_ns(percentile(s, 0.99)).c_str());
            printf("  P99.9:          %s\n", format_ns(percentile(s, 0.999)).c_str());
            printf("  Max:            %s\n", format_ns(s[n - 1]).c_str());
            printf("  Mean:           %s\n", format_ns(mean).c_str());
            printf("\n");
        }

        if (!inter_tick_ns.empty()) {
            auto& s = inter_tick_ns;
            std::sort(s.begin(), s.end());
            size_t n = s.size();
            uint64_t sum = std::accumulate(s.begin(), s.end(), 0ULL);
            uint64_t mean = sum / n;
            double throughput = 1e9 / mean;

            printf("INTER-TICK TIME (%zu samples)\n", n);
            printf("  Min:            %s\n", format_ns(s[0]).c_str());
            printf("  P50:            %s\n", format_ns(percentile(s, 0.50)).c_str());
            printf("  P95:            %s\n", format_ns(percentile(s, 0.95)).c_str());
            printf("  P99:            %s\n", format_ns(percentile(s, 0.99)).c_str());
            printf("  P99.9:          %s\n", format_ns(percentile(s, 0.999)).c_str());
            printf("  Max:            %s\n", format_ns(s[n - 1]).c_str());
            printf("  Mean:           %s\n", format_ns(mean).c_str());
            printf("  Throughput:     ~%.0f ticks/sec\n", throughput);
            printf("\n");
        }

        if (buy_filled || sell_filled) {
            printf("MARKET ORDER ROUND-TRIP\n");
            if (buy_filled)
                printf("  Buy:            %s\n", format_ns(buy_rtt_ns).c_str());
            if (sell_filled)
                printf("  Sell:           %s\n", format_ns(sell_rtt_ns).c_str());
            if (buy_filled && sell_filled)
                printf("  Mean:           %s\n", format_ns((buy_rtt_ns + sell_rtt_ns) / 2).c_str());
            printf("\n");
        } else if (order_mode == 1) {
            printf("MARKET ORDER ROUND-TRIP\n");
            printf("  (no fills received - market may be closed)\n");
            printf("\n");
        }

        if (limit_acked || limit_cancelled) {
            printf("LIMIT ORDER ROUND-TRIP\n");
            if (limit_acked)
                printf("  Submit→Ack:     %s\n", format_ns(limit_ack_rtt_ns).c_str());
            if (limit_cancelled)
                printf("  Cancel→Conf:    %s\n", format_ns(limit_cancel_rtt_ns).c_str());
            if (limit_acked && limit_cancelled)
                printf("  Total:          %s\n", format_ns(limit_ack_rtt_ns + limit_cancel_rtt_ns).c_str());
            printf("\n");
        } else if (order_mode == 2) {
            printf("LIMIT ORDER ROUND-TRIP\n");
            printf("  (no ack received - check IB Gateway connection)\n");
            printf("\n");
        }

        printf("========================================\n");
    }
};

// ─── Main ───

int main(int argc, char** argv) {
    const char* host = argc > 1 ? argv[1] : "127.0.0.1";
    int port = argc > 2 ? atoi(argv[2]) : 4002;
    int con_id = argc > 3 ? atoi(argv[3]) : 756733;  // SPY
    int collect_ticks = argc > 4 ? atoi(argv[4]) : 10000;
    int order_mode = argc > 5 ? atoi(argv[5]) : 0;
    int warmup_ticks = 200;

    const char* sym = "?";
    if (con_id == 756733) sym = "SPY";
    else if (con_id == 265598) sym = "AAPL";
    else if (con_id == 272093) sym = "MSFT";

    const char* order_desc = "none";
    if (order_mode == 1) order_desc = "MARKET (needs market hours)";
    else if (order_mode == 2) order_desc = "LIMIT submit+cancel (works after hours)";

    printf("========================================\n");
    printf("  IB Gateway (TWS API) Benchmark\n");
    printf("========================================\n");
    printf("  Host:           %s:%d\n", host[0] ? host : "localhost", port);
    printf("  Contract:       %s (con_id=%d)\n", sym, con_id);
    printf("  Warmup ticks:   %d\n", warmup_ticks);
    printf("  Collect ticks:  %d\n", collect_ticks);
    printf("  Order test:     %s\n", order_desc);
    printf("========================================\n\n");

    EReaderOSSignal signal(2000); // 2s timeout
    BenchmarkWrapper wrapper(con_id, warmup_ticks, collect_ticks, order_mode);
    EClientSocket client(&wrapper, &signal);
    wrapper.client = &client;

    printf("Connecting to IB Gateway at %s:%d...\n", host, port);
    fflush(stdout);
    wrapper.connect_time = Clock::now();
    bool ok = client.eConnect(host, port, 0, false);
    printf("eConnect returned: %s\n", ok ? "true" : "false");
    fflush(stdout);
    if (!ok) {
        printf("FATAL: Could not connect to IB Gateway at %s:%d\n", host, port);
        printf("Make sure IB Gateway is running and accepting connections.\n");
        return 1;
    }

    // Start reader thread
    printf("Starting EReader...\n");
    fflush(stdout);
    EReader reader(&client, &signal);
    reader.start();
    printf("EReader started, entering message loop...\n");
    fflush(stdout);

    // Main message loop
    auto deadline = Clock::now() + std::chrono::minutes(5);

    while (!wrapper.done.load(std::memory_order_relaxed) && Clock::now() < deadline) {
        signal.waitForSignal();
        wrapper.batch_start = Clock::now();
        reader.processMsgs();
    }

    if (!wrapper.done.load(std::memory_order_relaxed)) {
        int p = wrapper.phase.load(std::memory_order_relaxed);
        printf("\nBenchmark timed out in phase %d - are markets open?\n", p);
    }

    // Cancel market data and disconnect
    client.cancelMktData(1);
    client.eDisconnect();

    wrapper.print_report();
    return 0;
}
