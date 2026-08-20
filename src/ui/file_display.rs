use std::path::Path;

use crate::config::DisplayMode;
use crate::files::tree::{TreeNode, TreeNodeKind};

pub const fn ancestor(mode: DisplayMode, last: bool) -> &'static str {
    match (mode, last) {
        (DisplayMode::Ascii, true) => "   ",
        (DisplayMode::Ascii, false) => "|  ",
        (DisplayMode::Unicode | DisplayMode::Nerd, true) => "    ",
        (DisplayMode::Unicode | DisplayMode::Nerd, false) => "│   ",
    }
}

pub const fn branch(mode: DisplayMode, last: bool) -> &'static str {
    match (mode, last) {
        (DisplayMode::Ascii, true) => "`- ",
        (DisplayMode::Ascii, false) => "|- ",
        (DisplayMode::Unicode | DisplayMode::Nerd, true) => "└── ",
        (DisplayMode::Unicode | DisplayMode::Nerd, false) => "├── ",
    }
}

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
