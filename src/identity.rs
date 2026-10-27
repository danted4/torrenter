pub const REPO: &str = "danted4/torrenter";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const VERSION_TAG: &str = concat!("v", env!("CARGO_PKG_VERSION"));

pub fn banner() -> String {
    format!("{REPO} {VERSION_TAG}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppKind {
    Cli,
    Gui,
}

pub fn current_asset_name(kind: AppKind) -> Option<&'static str> {
    match (kind, std::env::consts::OS, std::env::consts::ARCH) {
        (AppKind::Cli, "linux", "x86_64") => Some("torrenter-x86_64-unknown-linux-gnu"),
        (AppKind::Gui, "linux", "x86_64") => Some("torrenter-gui-x86_64-unknown-linux-gnu"),
        (AppKind::Cli, "macos", "aarch64") => Some("torrenter-aarch64-apple-darwin"),
        (AppKind::Gui, "macos", "aarch64") => Some("torrenter-gui-aarch64-apple-darwin"),
        (AppKind::Cli, "macos", "x86_64") => Some("torrenter-x86_64-apple-darwin"),
        (AppKind::Gui, "macos", "x86_64") => Some("torrenter-gui-x86_64-apple-darwin"),
        (AppKind::Cli, "windows", "x86_64") => Some("torrenter-x86_64-pc-windows-msvc.exe"),
        (AppKind::Gui, "windows", "x86_64") => Some("torrenter-gui-x86_64-pc-windows-msvc.exe"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_includes_repo_and_version() {
        let text = banner();
        assert!(text.starts_with("danted4/torrenter v"), "{text}");
        assert!(text.contains(VERSION), "{text}");
    }

    #[test]
    fn asset_names_match_release_layout() {
        if let Some(name) = current_asset_name(AppKind::Cli) {
            assert!(name.starts_with("torrenter"));
            assert!(!name.starts_with("torrenter-gui"));
            assert!(!name.contains(".app"));
        }
        if let Some(name) = current_asset_name(AppKind::Gui) {
            assert!(name.starts_with("torrenter-gui"));
            assert!(!name.contains(".app"));
            assert!(!name.ends_with(".zip"));
        }
    }
}
