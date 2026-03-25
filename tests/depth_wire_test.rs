/// Raw farm depth test — bypasses hot loop to test wire format directly.
use ibx::gateway::{Gateway, GatewayConfig};
use ibx::protocol::fixcomp;
use std::time::{Duration, Instant};

fn config() -> GatewayConfig {
    GatewayConfig {
        username: std::env::var("IB_USERNAME").expect("IB_USERNAME"),
        password: std::env::var("IB_PASSWORD").expect("IB_PASSWORD"),
        host: std::env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string()),
        paper: true,
        accept_invalid_certs: false,
    }
}

#[test]
#[ignore]
fn raw_farm_subscribe_test() {
    let cfg = config();
    let (_gw, mut farm, _ccp, _hmds, _cash, _usfut, _eu, _j) =
        Gateway::connect(&cfg).expect("Gateway connect failed");

    eprintln!("Farm connected, seq={}", farm.seq);

    eprintln!("sign_iv={:02x?}", &farm.sign_iv[..8]);
    farm.seq = 0;

    // L1: 35=V|263=1|146=1|262=1|6008=265598|207=BEST|167=CS|264=0|9830=1|
    let r1 = farm.send_fixcomp(&[
        (35, "V"), (263, "1"), (146, "1"), (262, "1"),
        (6008, "265598"), (207, "BEST"), (167, "CS"), (264, "0"), (9830, "1"),
    ]);
    eprintln!("L1 subscribe: {:?} seq={}", r1, farm.seq);

    // Depth: 35=V|263=1|146=1|262=2|6008=265598|207=NASDAQ|167=CS|264=626|
    let r2 = farm.send_fixcomp(&[
        (35, "V"), (263, "1"), (146, "1"), (262, "2"),
        (6008, "265598"), (207, "NASDAQ"), (167, "CS"), (264, "626"),
    ]);
    eprintln!("Depth subscribe: {:?} seq={}", r2, farm.seq);

    // Poll for 15 seconds
    let start = Instant::now();
    let mut total_msgs = 0;
    while start.elapsed() < Duration::from_secs(15) {
        match farm.try_recv() {
            Ok(n) if n > 0 => eprintln!("  recv {} bytes", n),
            _ => {}
        }

        let frames = farm.extract_frames();
        for frame in &frames {
            total_msgs += 1;
            let raw = match &frame {
                ibx::protocol::connection::Frame::Fix(d) |
                ibx::protocol::connection::Frame::FixComp(d) |
                ibx::protocol::connection::Frame::Binary(d) => d.as_slice(),
            };
            // Decompress if FIXCOMP
            let msgs = if raw.starts_with(b"8=FIXCOMP") {
                let (unsigned, _) = farm.unsign(raw);
                fixcomp::fixcomp_decompress(&unsigned)
            } else {
                let (unsigned, _) = farm.unsign(raw);
                vec![unsigned]
            };
            for msg in &msgs {
                let text = String::from_utf8_lossy(msg);
                // Extract 35=
                let mt = text.split('\x01')
                    .find(|s| s.starts_with("35="))
                    .map(|s| &s[3..])
                    .unwrap_or("?");
                if mt == "P" || mt == "Y" {
                    eprintln!("[{}] 35={} ({} bytes) — DATA!", total_msgs, mt, msg.len());
                } else if mt == "Q" {
                    eprintln!("[{}] 35=Q ack: {}", total_msgs, &text[..text.len().min(120)]);
                } else if mt == "3" {
                    eprintln!("[{}] 35=3 REJECT: {}", total_msgs, &text[..text.len().min(200)]);
                } else {
                    eprintln!("[{}] 35={} ({} bytes)", total_msgs, mt, msg.len());
                }
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    eprintln!("\nTotal: {} messages in 15s", total_msgs);
}
