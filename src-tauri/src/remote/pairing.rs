//! Pairing codes, device tokens and the attempt limits that guard them.
//!
//! * A pairing code is 8 random digits, valid for [`CODE_TTL`] and once only;
//!   [`MAX_CODE_FAILURES`] wrong guesses throw it away.
//! * Every failed pairing or sign-in counts against the address it came from;
//!   [`MAX_FAILURES_PER_ADDRESS`] within [`FAILURE_WINDOW`] lock that address out
//!   until the window has passed.
//! * A device token is 32 random bytes (256 bits). Only its SHA-256 is stored;
//!   comparisons run in constant time.

use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

pub const CODE_DIGITS: usize = 8;
pub const CODE_TTL: Duration = Duration::from_secs(120);
pub const MAX_CODE_FAILURES: u32 = 5;
pub const MAX_FAILURES_PER_ADDRESS: usize = 10;
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// No code is waiting, it expired, or too many wrong guesses threw it away.
    NoPairing,
    /// Wrong code or unknown token.
    Rejected,
    /// Too many failures from this address lately.
    RateLimited,
    /// The pairing could not be saved.
    Unavailable,
}

impl AuthError {
    pub fn code(self) -> &'static str {
        match self {
            AuthError::NoPairing => "noPairing",
            AuthError::Rejected => "unauthorized",
            AuthError::RateLimited => "rateLimited",
            AuthError::Unavailable => "unavailable",
        }
    }
}

/// Fill a buffer from the operating system's random source.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).expect("the operating system's random source is unavailable");
    buf
}

/// A new pairing code: [`CODE_DIGITS`] uniformly random digits.
pub fn new_code() -> String {
    let mut code = String::with_capacity(CODE_DIGITS);
    while code.len() < CODE_DIGITS {
        // Reject 250..=255 so every digit is equally likely.
        for b in random_bytes::<16>() {
            if b < 250 && code.len() < CODE_DIGITS {
                code.push(char::from(b'0' + b % 10));
            }
        }
    }
    code
}

/// A new device token: 256 random bits, base64url without padding (43 characters).
pub fn new_token() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

/// What is stored for a token: hex SHA-256.
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Equal-length byte strings compared without an early exit; differing lengths fail.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Whether a presented token matches a stored hash.
pub fn token_matches(token: &str, stored_hash: &str) -> bool {
    constant_time_eq(hash_token(token).as_bytes(), stored_hash.as_bytes())
}

struct ActiveCode {
    code: String,
    expires: Instant,
    failures: u32,
}

/// The code waiting to be typed (or scanned), and the failures per address.
#[derive(Default)]
pub struct Pairing {
    code: Option<ActiveCode>,
    failures: HashMap<IpAddr, Vec<Instant>>,
}

impl Pairing {
    /// Start pairing: a fresh code replaces any earlier one.
    pub fn start(&mut self, now: Instant) -> (String, Instant) {
        let code = new_code();
        let expires = now + CODE_TTL;
        self.code = Some(ActiveCode {
            code: code.clone(),
            expires,
            failures: 0,
        });
        (code, expires)
    }

    pub fn cancel(&mut self) {
        self.code = None;
    }

    /// When the waiting code expires, if one is waiting.
    pub fn pending(&self, now: Instant) -> Option<Instant> {
        self.code
            .as_ref()
            .filter(|c| c.expires > now)
            .map(|c| c.expires)
    }

    /// Whether this address may try again right now.
    pub fn allowed(&mut self, ip: IpAddr, now: Instant) -> bool {
        let recent = self.failures.entry(ip).or_default();
        recent.retain(|t| now.saturating_duration_since(*t) < FAILURE_WINDOW);
        recent.len() < MAX_FAILURES_PER_ADDRESS
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) {
        self.failures.entry(ip).or_default().push(now);
        // Bound the table: drop addresses with nothing recent.
        if self.failures.len() > 256 {
            self.failures.retain(|_, v| {
                v.iter()
                    .any(|t| now.saturating_duration_since(*t) < FAILURE_WINDOW)
            });
        }
    }

    /// Try a code. Success uses it up; a wrong code counts against it and the address.
    pub fn attempt(&mut self, code: &str, ip: IpAddr, now: Instant) -> Result<(), AuthError> {
        if !self.allowed(ip, now) {
            return Err(AuthError::RateLimited);
        }
        let Some(active) = self.code.as_mut().filter(|c| c.expires > now) else {
            self.code = None;
            self.record_failure(ip, now);
            return Err(AuthError::NoPairing);
        };
        if constant_time_eq(active.code.as_bytes(), code.trim().as_bytes()) {
            self.code = None;
            return Ok(());
        }
        active.failures += 1;
        if active.failures >= MAX_CODE_FAILURES {
            self.code = None;
        }
        self.record_failure(ip, now);
        Err(AuthError::Rejected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const PHONE: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));
    const OTHER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 21));

    #[test]
    fn codes_are_eight_digits_and_tokens_256_bits() {
        let code = new_code();
        assert_eq!(code.len(), 8);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        let token = new_token();
        assert_eq!(token.len(), 43);
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&token)
                .unwrap()
                .len(),
            32
        );
        assert_ne!(new_token(), token, "tokens are random");
    }

    #[test]
    fn only_the_hash_matches_and_comparison_needs_equal_lengths() {
        let token = new_token();
        let stored = hash_token(&token);
        assert_eq!(stored.len(), 64);
        assert_ne!(stored, token);
        assert!(token_matches(&token, &stored));
        assert!(!token_matches(&new_token(), &stored));
        assert!(!token_matches("", &stored));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
    }

    #[test]
    fn a_code_works_once_and_not_after_it_expires() {
        let mut pairing = Pairing::default();
        let t0 = Instant::now();
        let (code, expires) = pairing.start(t0);
        assert_eq!(expires, t0 + CODE_TTL);
        assert_eq!(pairing.pending(t0), Some(expires));
        assert_eq!(
            pairing.attempt(&code, PHONE, t0 + Duration::from_secs(5)),
            Ok(())
        );
        assert_eq!(
            pairing.attempt(&code, PHONE, t0 + Duration::from_secs(6)),
            Err(AuthError::NoPairing),
            "used up"
        );
        assert_eq!(pairing.pending(t0), None);

        let (code, _) = pairing.start(t0);
        assert_eq!(
            pairing.attempt(&code, PHONE, t0 + CODE_TTL),
            Err(AuthError::NoPairing),
            "expired"
        );
        let (old, _) = pairing.start(t0);
        let (new, _) = pairing.start(t0);
        if old != new {
            assert_eq!(
                pairing.attempt(&old, OTHER, t0),
                Err(AuthError::Rejected),
                "a new code replaces the old one"
            );
        }
        assert_eq!(pairing.attempt(&new, OTHER, t0), Ok(()));
        pairing.start(t0);
        pairing.cancel();
        assert_eq!(pairing.pending(t0), None);
    }

    #[test]
    fn wrong_guesses_burn_the_code_and_lock_out_the_address() {
        let mut pairing = Pairing::default();
        let t0 = Instant::now();
        let (code, _) = pairing.start(t0);
        let wrong = if code == "00000000" {
            "11111111"
        } else {
            "00000000"
        };
        for _ in 0..MAX_CODE_FAILURES {
            assert_eq!(pairing.attempt(wrong, PHONE, t0), Err(AuthError::Rejected));
        }
        assert_eq!(
            pairing.attempt(&code, OTHER, t0),
            Err(AuthError::NoPairing),
            "five wrong guesses throw the code away"
        );

        // Keep failing from one address: it is locked out, others are not.
        for _ in 0..MAX_FAILURES_PER_ADDRESS {
            let _ = pairing.attempt(wrong, PHONE, t0);
        }
        let (code, _) = pairing.start(t0);
        assert_eq!(
            pairing.attempt(&code, PHONE, t0),
            Err(AuthError::RateLimited),
            "even the right code is refused while locked out"
        );
        assert!(
            pairing.pending(t0).is_some(),
            "a locked-out attempt does not spend the code"
        );
        assert!(!pairing.allowed(PHONE, t0 + FAILURE_WINDOW - Duration::from_millis(1)));
        assert!(
            pairing.allowed(PHONE, t0 + FAILURE_WINDOW),
            "the lockout ends with the window"
        );
        assert_eq!(pairing.attempt(&code, PHONE, t0 + FAILURE_WINDOW), Ok(()));
    }
}
