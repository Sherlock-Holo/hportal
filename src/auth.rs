use std::fmt::{Debug, Formatter};
use std::time::SystemTime;

use totp_rs::{Algorithm, TOTP};

pub struct Auth {
    totp: TOTP,
}

impl Debug for Auth {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("totp", &self.totp.to_string())
            .finish()
    }
}

impl Auth {
    pub fn new(secret: String) -> Result<Self, totp_rs::TotpUrlError> {
        Ok(Self {
            totp: TOTP::new(
                Algorithm::SHA512,
                8,
                1,
                30,
                secret.into_bytes(),
                None,
                "default_account".to_string(),
            )?,
        })
    }

    pub fn generate_token(&self) -> String {
        let time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("duration since UNIX_EPOCH should not fail")
            .as_secs();
        self.totp.generate(time)
    }

    pub fn auth(&self, token: &str) -> bool {
        self.totp.check_current(token).unwrap_or(false)
    }
}
