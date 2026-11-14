// The recorded scenarios replayed through the C++ client, with the options
// the Python replay tests give them (tests/python/test_scenario_replay.py).
// One option per line; see ibx_test_replay in src/capi/testing.rs.

// Orders with the market closed: on 10.51 at level 225, and on 10.40.
{"gw1051/20261018/lmt_cancel", "compare=order\nframes=2"},
{"20260926/lmt_cancel", "compare=order\nframes=2"},
{"gw1051/20261018/oca_group", "compare=order\nframes=3"},
{"20260926/oca_group", "compare=order\nframes=3"},
{"gw1051/20261018/cancel_unknown", "compare=order"},
{"20260926/cancel_unknown", "compare=order"},
{"gw1051/20261018/modify_cancelled", "compare=order\nframes=2"},
{"20260926/modify_cancelled", "compare=order\nskip_seqs=2441"},
{"gw1051/20261018/bracket", "compare=order\nframes=4"},
{"gw1051/20261018b/bracket", "compare=order\nframes=4"},
{"20260926/bracket", "compare=order"},
{"20260926b/bracket", "compare=order"},

// Combo orders.
{"gw1051/20261018/i105_combo_stock_smart", ""},
{"gw1051/20261018b/i105_combo_stock_smart", "replies_from=gw1051/20261018/i105_combo_stock_smart\nframes=2"},
{"gw1051/20261018b/i105_combo_leg_prices", "replies_from=gw1051/20261018/i105_combo_stock_smart\nframes=2"},
{"gw1051/20261018c/i105_combo_symbol_conflict", ""},
{"20260926/i105_combo_stock_smart", ""},
{"20260926b/i105_combo_stock_smart", "replies_from=20260926/i105_combo_stock_smart"},
{"20260926b/i105_combo_leg_prices", "replies_from=20260926/i105_combo_stock_smart"},
{"20260926b/i105_combo_directed", ""},
{"gw1051/20261018b/i105_combo_directed", ""},
{"gw1051/20261019rth/i105_combo_fill", "compare=order\ndrop=position\ndrop_id=9501"},
{"20260930/i105_combo_fill", "compare=order\ndrop=position\ndrop_id=9501"},

// Orders of earlier sessions, fills, executions.
{"gw1051/20261019rth/global_cancel_replayed", "compare=order"},
{"20261001/global_cancel_replayed", "compare=order"},
{"gw1051/20261014/partial_fills", "compare=order\nuntil=2137"},
{"gw1051/20261014/modify_cancelled", "compare=order\nframes=2"},
{"gw1051/20261014/modify_cancelled_no_pending", "compare=order\nframes=2"},
{"20261005/b2_trail", "compare=order\nskip_seqs=15585,15586,15587,15588,15589,15590,15768,15769,15770,15771,15772,15773,15964,15965,15966,15967,15968,15969,16182,16183,16184,16185,16186,16187,16349,16350,16351,16352,16353,16354,16522,16523,16524,16525,16526,16527"},

// Order types by session.
{"20260928/overnight_tif", "compare=order"},
{"gw1051/20261019ov/i196_overnight_smart", "compare=order"},
{"gw1051/20261019ov/i196_overnight_exch", "compare=order"},
{"gw1051/20261019ov/i196_overnight_day", "compare=order"},
{"gw1051/20261019ov/i196_day_include_overnight", "compare=order"},
{"gw1051/20261019rth/rth_order_types", "skip_orders=15,16,17,18,19,20,21\nuntil=2217\nskip_frame=exchange_map\ndrop=commissionAndFeesReport"},
{"20260928/rth_order_types", "skip_orders=1,2,3,4,5,6,7\nuntil=6581\ndrop=commissionAndFeesReport"},

// Market data.
{"l1_spy_qqq_rth", "codec\nuntil=6581\ncompare=market_data"},
{"gw1051/20261019pre/premarket_order_types", "skip_seqs=2167,2168,2706,2707\nuntil=2773\ndrop_id=9002,9003"},
{"20260928/premarket_order_types", "skip_seqs=21961,21962,22575,22576\nuntil=22599\ndrop_id=9002,9003"},
{"20261005/b2_generic", "farms=cashfarm,usfuture"},
{"20261005/b2_tbt", "hmds_farms=cashfarm"},
{"20261005/b2_rtbars", ""},
{"gw1051/20261019b1/b1_458_news_dup", "until=16439\ncompare=market_data,historical\nskip_frame=exchange_map\ndrop=marketDataType|;tickReqParams|\nframes=4"},
{"20261002/b1_458_news_dup", "until=15737\ncompare=market_data,historical\nskip_frame=exchange_map\ndrop=marketDataType|;tickReqParams|\nframes=4"},
{"gw1051/20261019b1/b1_458_news_dup", "paced\ncompare=market_data,historical\nskip_frame=exchange_map\ndrop=marketDataType|;tickReqParams|\nframes=8"},

// Historical data.
{"gw1051/20261018/hist_keep_up_to_date", ""},
{"gw1051/20261018b/hist_keep_up_to_date", ""},
{"20260926b/hist_keep_up_to_date", ""},
{"gw1051/20261019b1/b1_429_keep_up_to_date", "until=3770"},
{"20261002/b1_429_keep_up_to_date", "until=3650"},
{"gw1051/20261019b1/b1_429_keep_up_to_date", "lookups_at_their_point\ndrop=error|9536|366|"},
{"20261002/b1_429_keep_up_to_date", "lookups_at_their_point\ndrop=error|9536|366|"},
{"gw1051/20261019b1/b1_431_hist_format", ""},
{"20261002/b1_431_hist_format", ""},

// Account and scanner.
{"20260926/account_summary", "after_account_updates"},
{"gw1051/20261018c/account_summary", "until=1413"},
{"gw1051/20261018/account_summary", "until=2717\nafter_account_updates"},
{"20260926b/scanner_two", "compare=scanner,lookup\ndrop=error|9006|162|\nframes=24"},
{"gw1051/20261018/scanner_two", "compare=scanner,lookup\ndrop=error|9006|162|\nframes=20"},
