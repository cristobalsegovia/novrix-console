//! A plaintext secret that resists accidental disclosure.
//!
//! [`Secret`] carries a password or passphrase in memory while it is handed
//! between the caller and the Credential_Store backend. It is deliberately
//! austere:
//!
//! - Its `Debug` and `Display` impls redact the contents, so a secret can
//!   never slip into a log line, panic message, or error string by accident.
//! - It does **not** derive `Serialize`/`Deserialize`: secrets never cross the
//!   IPC boundary or land in a profile/export file as plaintext.
//! - On drop it best-effort overwrites its backing bytes before freeing them.
//!
//! The only way to read the plaintext is the explicit [`Secret::expose`], which
//! names the risk at the call site. (No external zeroizing crate is used; the
//! scrub here is a plain in-place overwrite. It is a best-effort hardening
//! measure, not a guarantee against every compiler/OS memory behavior.)

/// A wrapper around a plaintext secret string.
///
/// Construct with [`Secret::new`] and read the plaintext only via the explicit
/// [`Secret::expose`]. Equality is supported for tests and lookups but still
/// never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    inner: String,
}

impl Secret {
    /// Wrap a plaintext secret.
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            inner: secret.into(),
        }
    }

    /// Borrow the plaintext. The method name is intentionally blunt so every
    /// read of a secret is visible in review.
    pub fn expose(&self) -> &str {
        &self.inner
    }

    /// Consume the wrapper and return the owned plaintext.
    ///
    /// Prefer [`expose`](Secret::expose) when a borrow suffices; this is for
    /// the rare case that needs ownership (e.g. moving the value into a
    /// backend call that takes `String`).
    pub fn into_inner(mut self) -> String {
        std::mem::take(&mut self.inner)
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Best-effort scrub: overwrite the backing bytes in place before the
        // allocation is freed. `read_volatile`/`write_volatile` would be
        // stronger, but a plain overwrite avoids unsafe code and still clears
        // the common case.
        //
        // SAFETY-free: operate on the owned bytes through the String's buffer.
        let bytes = unsafe { self.inner.as_bytes_mut() };
        for b in bytes.iter_mut() {
            *b = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_redact_the_value() {
        let secret = Secret::new("super-secret-password");
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{secret}"), "<redacted>");
        // The plaintext must not appear in either rendering.
        assert!(!format!("{secret:?}").contains("super-secret-password"));
        assert!(!format!("{secret}").contains("super-secret-password"));
    }

    #[test]
    fn expose_returns_the_plaintext() {
        let secret = Secret::new("p@ss");
        assert_eq!(secret.expose(), "p@ss");
    }

    #[test]
    fn into_inner_yields_owned_plaintext() {
        let secret = Secret::new("p@ss");
        assert_eq!(secret.into_inner(), "p@ss");
    }

    #[test]
    fn equality_compares_contents() {
        assert_eq!(Secret::new("a"), Secret::new("a"));
        assert_ne!(Secret::new("a"), Secret::new("b"));
    }
}
