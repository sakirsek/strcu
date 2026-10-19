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

/// The port the panel listens on. A development build takes another one so both can run at once.
pub const PORT: u16 = if DEV { 8766 } else { 8765 };

/// Where settings, macros and the log live today. From B1 they move to `%ProgramData%\StrCu`, with an ACL
/// of SYSTEM and administrators only (S2 in the plan), and this is read once to migrate them.
pub fn old_data_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("strcu")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_development_build_keeps_away_from_the_installed_one() {
        if DEV {
            assert_ne!(NAME, "StrCu");
            assert_ne!(PORT, 8765);
        } else {
            assert_eq!((NAME, PORT), ("StrCu", 8765));
        }
        assert!(old_data_dir().ends_with("strcu"));
    }
}
