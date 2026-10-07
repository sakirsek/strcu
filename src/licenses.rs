//! `strcu licenses`: the licenses of the third-party software built into strcu.exe, carried by the exe itself.

/// Made by cargo-about from Cargo.lock (see about.toml)
const CRATES: &str = include_str!("../THIRD-PARTY-NOTICES.txt");
/// The panel's embedded IBM Plex fonts
const FONTS: &str = include_str!("web/fonts/OFL.txt");

pub fn text() -> String {
    format!(
        "{}\n\n== SIL Open Font License 1.1 ==\nUsed by: IBM Plex Sans, IBM Plex Mono (the panel's fonts)\n\n{}\n",
        CRATES.trim_end(),
        FONTS.trim_end()
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// A dependency update leaves crate versions in the notices that Cargo.lock no longer has:
    /// then the file needs to be made again (about.toml says how).
    #[test]
    fn notices_match_cargo_lock() {
        let lock = include_str!("../Cargo.lock");
        let mut locked = BTreeSet::new();
        let mut name = None;
        for line in lock.lines() {
            if let Some(n) = line.strip_prefix("name = ") {
                name = Some(n.trim_matches('"'));
            } else if let (Some(v), Some(n)) = (line.strip_prefix("version = "), name.take()) {
                locked.insert(format!("{n} {}", v.trim_matches('"')));
            }
        }
        let listed: Vec<&str> = CRATES.lines().filter_map(|l| l.strip_prefix("Used by: ")).flat_map(|l| l.split(", ")).collect();
        assert!(listed.len() > 100, "{} crates listed", listed.len());
        let stale: Vec<_> = listed.iter().filter(|c| !locked.contains(**c)).collect();
        assert!(stale.is_empty(), "not in Cargo.lock: {stale:?}; run `cargo about generate about.hbs -o THIRD-PARTY-NOTICES.txt`");
    }

    #[test]
    fn fonts_included() {
        assert!(text().contains("SIL OPEN FONT LICENSE Version 1.1"));
    }
}
