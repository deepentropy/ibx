//! The restart token kept on disk between two programs of the same machine
//! (ibx#762).
//!
//! A login with the password announces a restart token. A later program of
//! the same machine presents it instead of the password and the second
//! factor, as the reference does when it restarts itself each day, and the
//! server takes it for days after the login with the password. The file
//! holds a login credential: it is written only when the program asks for
//! it ([`crate::gateway::GatewayConfig::restart_token_file`]), readable by
//! its owner only where the system allows it.

use std::io::{self, Write};
use std::path::Path;

use num_bigint::BigUint;

const HEADER: &str = "ibx-restart-token 1";

/// A restart token and the login it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedRestartToken {
    pub username: String,
    pub paper: bool,
    /// The server session id the token was announced in: a login with the
    /// token presents it.
    pub session_id: String,
    /// The machine string the token is tied to.
    pub hw_info: String,
    pub token: BigUint,
}

impl SavedRestartToken {
    /// The file's text.
    pub fn to_text(&self) -> String {
        format!(
            "{}\nuser={}\npaper={}\nsession={}\nhw={}\ntoken={:x}\n",
            HEADER, self.username, u8::from(self.paper), self.session_id, self.hw_info, self.token
        )
    }

    /// The token of a file's text, `None` when it is not one.
    pub fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()?.trim() != HEADER {
            return None;
        }
        let (mut username, mut paper, mut session_id, mut hw_info, mut token) = (None, None, None, None, None);
        for line in lines {
            let Some((key, value)) = line.split_once('=') else { continue };
            match key {
                "user" => username = Some(value.to_string()),
                "paper" => paper = Some(value == "1"),
                "session" => session_id = Some(value.to_string()),
                "hw" => hw_info = Some(value.to_string()),
                "token" => token = BigUint::parse_bytes(value.trim().as_bytes(), 16),
                _ => {}
            }
        }
        Some(Self { username: username?, paper: paper?, session_id: session_id?, hw_info: hw_info?, token: token? })
    }

    /// The token saved at `path` for this user, account kind and machine:
    /// a file that is missing, unreadable or for another login gives `None`
    /// (the login then takes the password).
    pub fn read(path: &Path, username: &str, paper: bool, hw_info: &str) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let saved = Self::parse(&text);
        match saved {
            Some(s) if s.username == username && s.paper == paper && s.hw_info == hw_info && !s.session_id.is_empty() => Some(s),
            Some(_) => {
                log::info!("Restart token file {} is for another login or machine: not used", path.display());
                None
            }
            None => {
                log::warn!("Restart token file {} is not readable as a restart token: not used", path.display());
                None
            }
        }
    }

    /// Write the token to `path`, replacing the file at once (a new file
    /// renamed over the old one), readable by its owner only on Unix.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("tmp");
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(self.to_text().as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    /// [`Self::write`], with a failure only logged: the session goes on,
    /// the next program logs in with the password.
    pub fn save(&self, path: &Path) {
        match self.write(path) {
            Ok(()) => log::info!("Restart token saved to {}", path.display()),
            Err(e) => log::warn!("Restart token not saved to {}: {}", path.display(), e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved() -> SavedRestartToken {
        SavedRestartToken {
            username: "user1".into(),
            paper: true,
            session_id: "6afd346d.00000022".into(),
            hw_info: "0a1b2c3d|00:11:22:33:44:55".into(),
            token: BigUint::parse_bytes(b"0fa1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3", 16).unwrap(),
        }
    }

    #[test]
    fn text_round_trip() {
        let s = saved();
        assert_eq!(SavedRestartToken::parse(&s.to_text()), Some(s));
        assert_eq!(SavedRestartToken::parse("something else\nuser=x\n"), None);
        assert_eq!(SavedRestartToken::parse("ibx-restart-token 1\nuser=x\n"), None, "fields missing");
    }

    #[test]
    fn read_only_for_the_same_login_and_machine() {
        let dir = std::env::temp_dir().join(format!("ibx-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        let s = saved();
        s.write(&path).unwrap();
        assert_eq!(SavedRestartToken::read(&path, "user1", true, &s.hw_info), Some(s.clone()));
        assert_eq!(SavedRestartToken::read(&path, "user2", true, &s.hw_info), None);
        assert_eq!(SavedRestartToken::read(&path, "user1", false, &s.hw_info), None);
        assert_eq!(SavedRestartToken::read(&path, "user1", true, "other|00:00:00:00:00:01"), None);
        assert_eq!(SavedRestartToken::read(&dir.join("missing"), "user1", true, &s.hw_info), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
