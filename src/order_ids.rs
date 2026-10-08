//! The highest order id each API client used, kept on disk across sessions.
//!
//! The reference keeps this value per client id in its saved settings, so
//! `nextValidId` after a new start is above every order the client placed
//! before, the cancelled ones included: the server's replay of a logon shows
//! only the orders that are working or filled that day (ibx#518).
//!
//! One text file, one line per account and client id:
//! `<account>.<clientId>=<highest order id>`. A value only goes up.
//!
//! The file is `%USERPROFILE%\ibx_order_ids` on Windows and
//! `$HOME/.ibx_order_ids` elsewhere. `IBX_ORDER_IDS_PATH` names another
//! file; set to an empty value, nothing is read or written.
//!
//! Nothing here runs on the order path: a client's [`Saver`] thread writes
//! the value a short time after it moved, and once more when it stops.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How long after an order the value is on disk at the latest.
const SAVE_PERIOD: Duration = Duration::from_millis(250);

/// The file of the saved ids; None when saving is turned off.
pub fn default_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("IBX_ORDER_IDS_PATH") {
        return if p.is_empty() { None } else { Some(PathBuf::from(p)) };
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    Some(home.join(if cfg!(windows) { "ibx_order_ids" } else { ".ibx_order_ids" }))
}

fn key(account: &str, client_id: i64) -> String {
    format!("{account}.{client_id}")
}

fn entries(path: &Path) -> Vec<(String, i64)> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter_map(|line| {
            let (k, v) = line.trim().rsplit_once('=')?;
            Some((k.to_string(), v.parse().ok()?))
        })
        .collect()
}

/// The saved highest order id of a client; 0 when none.
pub fn load(path: &Path, account: &str, client_id: i64) -> i64 {
    let key = key(account, client_id);
    entries(path).into_iter().find(|(k, _)| *k == key).map(|(_, v)| v).unwrap_or(0)
}

/// Save the highest order id of a client. The value on disk only goes up,
/// and the other clients' lines are kept. Returns the value now on disk.
pub fn save(path: &Path, account: &str, client_id: i64, highest: i64) -> std::io::Result<i64> {
    let key = key(account, client_id);
    let mut all = entries(path);
    let value = match all.iter_mut().find(|(k, _)| *k == key) {
        Some(entry) if entry.1 >= highest => return Ok(entry.1),
        Some(entry) => {
            entry.1 = highest;
            highest
        }
        None => {
            all.push((key, highest));
            highest
        }
    };
    let text: String = all.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    // Written whole to a file beside it, then put in place: a reader never
    // sees half a file.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(value)
}

/// Keeps one client's value on disk while the client is connected.
pub struct Saver {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Saver {
    /// Raise `highest` to the saved value, then save it whenever it moved.
    pub fn start(path: PathBuf, account: &str, client_id: i64, highest: Arc<AtomicI64>) -> std::io::Result<Self> {
        let mut saved = load(&path, account, client_id);
        highest.fetch_max(saved, Ordering::AcqRel);
        let stop = Arc::new(AtomicBool::new(false));
        let (stopped, account) = (stop.clone(), account.to_string());
        let thread = std::thread::Builder::new().name("ibx-order-ids".into()).spawn(move || loop {
            // Read before the save, so the last pass after a stop request
            // still saves what was placed until then.
            let last = stopped.load(Ordering::Acquire);
            let now = highest.load(Ordering::Acquire);
            if now > saved {
                match save(&path, &account, client_id, now) {
                    Ok(on_disk) => {
                        saved = on_disk;
                        // Another process of the same client placed above us.
                        highest.fetch_max(on_disk, Ordering::AcqRel);
                    }
                    Err(e) => log::warn!("Order ids not saved to {}: {}", path.display(), e),
                }
            }
            if last {
                break;
            }
            std::thread::park_timeout(SAVE_PERIOD);
        })?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ibx_order_ids_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("ids")
    }

    #[test]
    fn a_value_is_kept_per_account_and_client_and_only_goes_up() {
        let path = file("values");
        assert_eq!(load(&path, "DU1", 198), 0, "no file");
        assert_eq!(save(&path, "DU1", 198, 137).unwrap(), 137);
        assert_eq!(save(&path, "DU1", 7, 500).unwrap(), 500);
        assert_eq!(save(&path, "DU2", 198, 3).unwrap(), 3);
        assert_eq!(save(&path, "DU1", 198, 12).unwrap(), 137, "a lower value does not replace it");
        assert_eq!((load(&path, "DU1", 198), load(&path, "DU1", 7), load(&path, "DU2", 198)), (137, 500, 3));
        assert_eq!(load(&path, "DU1", 19), 0, "another client id");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "DU1.198=137\nDU1.7=500\nDU2.198=3\n");
    }

    #[test]
    fn a_damaged_line_is_left_out() {
        let path = file("damaged");
        std::fs::write(&path, "DU1.198=137\nnot a line\nDU1.7=x\n\nDU1.3=9\n").unwrap();
        assert_eq!((load(&path, "DU1", 198), load(&path, "DU1", 7), load(&path, "DU1", 3)), (137, 0, 9));
    }

    // ibx#518: order 1 placed and cancelled, then a new login of the same
    // client: its next valid id is 2, not 1 again.
    #[test]
    fn the_next_session_starts_above_the_orders_of_the_last() {
        let path = file("sessions");
        let first = Arc::new(AtomicI64::new(0));
        let saver = Saver::start(path.clone(), "DU1", 198, first.clone()).unwrap();
        assert_eq!(first.load(Ordering::Acquire), 0);
        first.fetch_max(1, Ordering::AcqRel);
        drop(saver);
        assert_eq!(load(&path, "DU1", 198), 1, "saved when the client stops");

        let second = Arc::new(AtomicI64::new(0));
        let saver = Saver::start(path.clone(), "DU1", 198, second.clone()).unwrap();
        assert_eq!(second.load(Ordering::Acquire), 1, "the new session knows order 1");
        second.fetch_max(5, Ordering::AcqRel);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while load(&path, "DU1", 198) != 5 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(load(&path, "DU1", 198), 5, "saved while the client runs");
        drop(saver);

        let other = Arc::new(AtomicI64::new(0));
        let _saver = Saver::start(path, "DU1", 7, other.clone()).unwrap();
        assert_eq!(other.load(Ordering::Acquire), 0, "another client id starts at its own value");
    }
}
