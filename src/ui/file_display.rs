//! Row vocabulary for Files across display modes: the state column that
//! carries expansion, the typed nerd icon, and its truecolor accents.
//!
//! Rows are `[marker][space][indent][state glyph][icon?][space][name]`;
//! indentation is two columns per depth level and there are no tree guide
//! connectors in any mode.

use std::path::Path;

use crate::config::DisplayMode;
use crate::files::tree::{TreeNode, TreeNodeKind};

pub fn icon(mode: DisplayMode, node: &TreeNode, expanded: bool) -> &'static str {
    match mode {
        DisplayMode::Ascii => match node.kind() {
            TreeNodeKind::Directory if expanded => "-",
            TreeNodeKind::Directory => "+",
            TreeNodeKind::File => "f",
            TreeNodeKind::Symlink => "l",
            TreeNodeKind::Virtual => "!",
        },
        DisplayMode::Unicode => match node.kind() {
            TreeNodeKind::Directory if expanded => "▾",
            TreeNodeKind::Directory => "▸",
            TreeNodeKind::File => "•",
            TreeNodeKind::Symlink => "↗",
            TreeNodeKind::Virtual => "×",
        },
        DisplayMode::Nerd => match node.kind() {
            TreeNodeKind::Directory if expanded => "",
            TreeNodeKind::Directory => "",
            TreeNodeKind::File => nerd_file_icon(node.path()),
            TreeNodeKind::Symlink => "",
            TreeNodeKind::Virtual => "",
        },
    }
}

/// The expansion/state column between the depth indent and the name. Ascii
/// and unicode fold kind into one glyph; nerd reserves a chevron column so
/// the typed [`icon`] glyphs stay vertically aligned with files.
#[must_use]
pub const fn state_glyph(mode: DisplayMode, kind: TreeNodeKind, expanded: bool) -> &'static str {
    match mode {
        DisplayMode::Ascii => match kind {
            TreeNodeKind::Directory if expanded => "-",
            TreeNodeKind::Directory => "+",
            TreeNodeKind::File => "f",
            TreeNodeKind::Symlink => "l",
            TreeNodeKind::Virtual => "!",
        },
        DisplayMode::Unicode => match kind {
            TreeNodeKind::Directory if expanded => "▾",
            TreeNodeKind::Directory => "▸",
            TreeNodeKind::File => "•",
            TreeNodeKind::Symlink => "↗",
            TreeNodeKind::Virtual => "×",
        },
        DisplayMode::Nerd => match kind {
            TreeNodeKind::Directory if expanded => "▾ ",
            TreeNodeKind::Directory => "▸ ",
            // Files keep a same-width blank so icons line up across kinds.
            _ => "   ",
        },
    }
}

/// Deterministic truecolor accent for the nerd icon of a node, or `None` to
/// inherit the row style (monochrome configuration, ignored rows). Groups
/// mirror [`nerd_file_icon`] so a colored glyph always has its color here.
#[must_use]
pub fn icon_rgb(kind: TreeNodeKind, path: &Path) -> Option<(u8, u8, u8)> {
    if kind == TreeNodeKind::Directory {
        return Some((232, 191, 92));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        name.as_str(),
        ".gitignore" | ".gitattributes" | ".gitmodules"
    ) {
        return Some((241, 78, 50));
    }
    if matches!(
        name.as_str(),
        "cargo.toml" | "cargo.lock" | "rust-toolchain.toml"
    ) {
        return Some((222, 165, 132));
    }
    if matches!(
        name.as_str(),
        "package.json" | "package-lock.json" | "npm-shrinkwrap.json"
    ) {
        return Some((203, 62, 53));
    }
    if name == "dockerfile" || name.starts_with("compose.") {
        return Some((36, 150, 237));
    }
    if matches!(name.as_str(), "makefile" | "justfile") {
        return Some((137, 171, 103));
    }

    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    Some(match extension.as_str() {
        "rs" => (222, 165, 132),
        "py" | "pyi" => (83, 114, 165),
        "js" | "jsx" | "mjs" | "cjs" => (214, 202, 88),
        "ts" | "tsx" | "mts" | "cts" => (73, 120, 198),
        "md" | "mdx" | "markdown" => (81, 154, 186),
        "json" | "jsonc" => (203, 203, 101),
        "toml" => (156, 99, 66),
        "yaml" | "yml" => (179, 98, 58),
        "html" | "htm" => (227, 76, 38),
        "css" | "scss" | "sass" | "less" => (142, 100, 182),
        "sh" | "bash" | "zsh" | "fish" => (137, 224, 81),
        "c" | "h" => (168, 185, 204),
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => (243, 75, 125),
        "go" => (0, 173, 216),
        "java" => (176, 114, 25),
        "kt" | "kts" => (167, 123, 255),
        "rb" => (204, 82, 111),
        "php" => (119, 123, 179),
        "swift" => (240, 81, 56),
        "lua" => (81, 101, 180),
        "sql" | "db" | "sqlite" | "sqlite3" => (109, 154, 214),
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "ico" => (160, 116, 196),
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" => (172, 138, 90),
        "pdf" => (217, 84, 89),
        "txt" | "log" => (168, 168, 168),
        "lock" => (140, 140, 140),
        _ => return None,
    })
}

fn nerd_file_icon(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if equals_any(name, &["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"]) {
        return "";
    }
    if equals_any(
        name,
        &["package.json", "package-lock.json", "npm-shrinkwrap.json"],
    ) {
        return "";
    }
    if name.eq_ignore_ascii_case("Dockerfile")
        || name.eq_ignore_ascii_case("compose.yaml")
        || name.eq_ignore_ascii_case("compose.yml")
        || name.eq_ignore_ascii_case("docker-compose.yaml")
        || name.eq_ignore_ascii_case("docker-compose.yml")
    {
        return "";
    }
    if equals_any(name, &[".gitignore", ".gitattributes", ".gitmodules"]) {
        return "";
    }
    if equals_any(name, &["Makefile", "Justfile"]) {
        return "";
    }

    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    if extension_is(extension, &["rs"]) {
        ""
    } else if extension_is(extension, &["py", "pyi"]) {
        ""
    } else if extension_is(extension, &["js", "jsx", "mjs", "cjs"]) {
        ""
    } else if extension_is(extension, &["ts", "tsx", "mts", "cts"]) {
        ""
    } else if extension_is(extension, &["md", "mdx", "markdown"]) {
        ""
    } else if extension_is(extension, &["json", "jsonc"]) {
        ""
    } else if extension_is(extension, &["toml"]) {
        ""
    } else if extension_is(extension, &["yaml", "yml"]) {
        ""
    } else if extension_is(extension, &["html", "htm"]) {
        ""
    } else if extension_is(extension, &["css", "scss", "sass", "less"]) {
        ""
    } else if extension_is(extension, &["sh", "bash", "zsh", "fish"]) {
        ""
    } else if extension_is(extension, &["c", "h"]) {
        ""
    } else if extension_is(extension, &["cc", "cpp", "cxx", "hh", "hpp", "hxx"]) {
        ""
    } else if extension_is(extension, &["go"]) {
        ""
    } else if extension_is(extension, &["java"]) {
        ""
    } else if extension_is(extension, &["kt", "kts"]) {
        ""
    } else if extension_is(extension, &["rb"]) {
        ""
    } else if extension_is(extension, &["php"]) {
        ""
    } else if extension_is(extension, &["swift"]) {
        ""
    } else if extension_is(extension, &["lua"]) {
        ""
    } else if extension_is(extension, &["sql", "db", "sqlite", "sqlite3"]) {
        ""
    } else if extension_is(
        extension,
        &["png", "jpg", "jpeg", "gif", "svg", "webp", "bmp", "ico"],
    ) {
        ""
    } else if extension_is(
        extension,
        &["zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar"],
    ) {
        ""
    } else if extension_is(extension, &["pdf"]) {
        ""
    } else if extension_is(extension, &["txt", "log"]) {
        ""
    } else if extension_is(extension, &["lock"]) {
        ""
    } else {
        ""
    }
}

fn equals_any(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn extension_is(extension: &str, candidates: &[&str]) -> bool {
    equals_any(extension, candidates)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::nerd_file_icon;

    #[test]
    fn nerd_icons_cover_known_types_and_fall_back_for_unknown_files() {
        assert_eq!(nerd_file_icon(Path::new("src/main.rs")), "");
        assert_eq!(nerd_file_icon(Path::new("web/app.tsx")), "");
        assert_eq!(nerd_file_icon(Path::new("Dockerfile")), "");
        assert_eq!(nerd_file_icon(Path::new("data.sqlite")), "");
        assert_eq!(nerd_file_icon(Path::new("unknown.xyz")), "");
    }
}
