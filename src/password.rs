// Argon2 wrap for the numeric passcode.
//
// - Hash format : PHC string `$argon2id$v=19$m=…,t=…,p=…$<salt>$<hash>`
//   — self-describing, so we can bump parameters later without a schema
//   migration.
// - Salt : 16 bytes from OsRng.
// - Params : argon2id defaults (m=19456 KiB, t=2, p=1). Adequate for a
//   4-12 digit passcode that only gates a local config UI ; not
//   protecting a bank vault.

use anyhow::{anyhow, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand_core_06::OsRng;

pub fn hash(passcode: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(passcode.as_bytes(), &salt)
        .map_err(|e| anyhow!("argon2 hash: {e}"))?
        .to_string();
    Ok(hash)
}

#[allow(dead_code)] // called by password-gated config + uninstall flows
pub fn verify(passcode: &str, hash: &str) -> Result<bool> {
    let parsed = PasswordHash::new(hash).map_err(|e| anyhow!("parse argon2 hash: {e}"))?;
    Ok(Argon2::default()
        .verify_password(passcode.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_matches() {
        let h = hash("123456").unwrap();
        assert!(verify("123456", &h).unwrap());
    }

    #[test]
    fn wrong_passcode_rejects() {
        let h = hash("123456").unwrap();
        assert!(!verify("654321", &h).unwrap());
    }

    #[test]
    fn hash_is_phc_argon2id_format() {
        let h = hash("4242").unwrap();
        assert!(h.starts_with("$argon2id$"), "unexpected hash format: {h}");
    }

    #[test]
    fn same_input_yields_different_hashes() {
        // Salt randomness → identical passcodes must produce different
        // hashes. Sanity check on OsRng threading.
        let a = hash("1234").unwrap();
        let b = hash("1234").unwrap();
        assert_ne!(a, b);
        assert!(verify("1234", &a).unwrap());
        assert!(verify("1234", &b).unwrap());
    }

    #[test]
    fn malformed_hash_errors_gracefully() {
        assert!(verify("1234", "not-a-hash").is_err());
    }
}
