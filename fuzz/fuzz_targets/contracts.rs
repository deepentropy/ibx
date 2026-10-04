#![no_main]

// ibx#488: how to run, in fuzz/Cargo.toml.
libfuzzer_sys::fuzz_target!(|data: &[u8]| ibx::test_support::decoders::contracts(data));
