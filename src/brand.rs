//! What this build calls itself and where it keeps things.
//!
//! From 2.0 StrCu installs as a Windows service with its data in a folder only administrators may write to,
//! so a build under development must not share a name, a folder or a port with the installed one. Building
//! with `--features devbuild` gives "StrCuDev": its own service, its own folders, its own port. Everything
//! that names the product or picks a path asks here, so there is one place to look.

/// true in a development build (`--features devbuild`).
pub const DEV: bool = cfg!(feature = "devbuild");

/// Shown to people: the window title, the panel, the service's display name.
pub const NAME: &str = if DEV { "StrCuDev" } else { "StrCu" };

/// Used by Windows: the folder names, the service, the pipe. No spaces.
pub const KEY: &str = NAME;

/// The port the panel listens on. A development build takes another one so both can run at once.
pub const PORT: u16 = if DEV { 8766 } else { 8765 };

/// Where cloudflared reports its state (its "metrics" address). A development build asks its own
/// cloudflared, not the installed StrCu's, or it would read the other one's connection as its own.
pub const METRICS: u16 = if DEV { 8093 } else { 8092 };

/// Where 1.x kept settings, macros and the log: `%LOCALAPPDATA%\strcu`. It is read once, to copy them into
/// the folder of 2.0 (see paths.rs). A development build looks for its own name, so it never picks up the
/// settings of the installed StrCu.
pub fn old_data_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join(KEY.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_development_build_keeps_away_from_the_installed_one() {
        if DEV {
            assert_ne!(NAME, "StrCu");
            assert_ne!(PORT, 8765);
            assert_ne!(METRICS, 8092);
        } else {
            assert_eq!((NAME, PORT), ("StrCu", 8765));
        }
        // A development build never reads the installed StrCu's settings
        assert!(old_data_dir().ends_with(KEY.to_lowercase()));
        assert_eq!(old_data_dir().ends_with("strcu"), !DEV);
    }
}
