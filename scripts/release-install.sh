#!/bin/sh
set -eu

umask 077
plugin_id=herdr-context
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
herdr_bin=${HERDR_BIN:-herdr}
data_home=${XDG_DATA_HOME:-${HOME:?HOME is required}/.local/share}
install_root=${HERDR_CONTEXT_INSTALL_DIR:-$data_home/herdr-context/plugin}
run_herdr() {
    if [ -n "${HERDR_SESSION:-}" ]; then
        "$herdr_bin" --session "$HERDR_SESSION" "$@"
    else
        "$herdr_bin" "$@"
    fi
}


case "$install_root" in
    /*) ;;
    *) printf '%s\n' "install: HERDR_CONTEXT_INSTALL_DIR must be absolute" >&2; exit 2 ;;
esac
if [ "$install_root" = "/" ] || [ -L "$install_root" ]; then
    printf '%s\n' "install: unsafe installation root: $install_root" >&2
    exit 2
fi

for file in herdr-context herdr-plugin.toml install.sh uninstall.sh README.md LICENSE; do
    if [ ! -f "$source_dir/$file" ] || [ -L "$source_dir/$file" ]; then
        printf '%s\n' "install: missing or unsafe package member: $file" >&2
        exit 2
    fi
done
if [ ! -x "$source_dir/herdr-context" ]; then
    printf '%s\n' "install: packaged herdr-context is not executable" >&2
    exit 2
fi

parent=$(dirname -- "$install_root")
mkdir -p -- "$parent"
temporary=$(mktemp -d "$parent/.herdr-context.install.XXXXXX")
backup=
cleanup() {
    if [ -n "$temporary" ] && [ -e "$temporary" ]; then
        rm -rf -- "$temporary"
    fi
    if [ -n "$backup" ] && [ -e "$backup" ]; then
        rm -rf -- "$install_root"
        mv -- "$backup" "$install_root"
    fi
}
trap cleanup EXIT HUP INT TERM

if [ -e "$install_root" ]; then
    if [ ! -d "$install_root" ] || [ -L "$install_root" ] ||
        [ ! -f "$install_root/.herdr-context-owned" ] ||
        [ "$(cat "$install_root/.herdr-context-owned")" != "$plugin_id" ]; then
        printf '%s\n' "install: refusing to replace unowned path: $install_root" >&2
        exit 2
    fi
    backup="$parent/.herdr-context.backup.$$"
    if [ -e "$backup" ]; then
        printf '%s\n' "install: stale backup path exists: $backup" >&2
        exit 2
    fi
    mv -- "$install_root" "$backup"
fi

cp -- "$source_dir/herdr-context" "$temporary/herdr-context"
cp -- "$source_dir/herdr-plugin.toml" "$temporary/herdr-plugin.toml"
cp -- "$source_dir/install.sh" "$temporary/install.sh"
cp -- "$source_dir/uninstall.sh" "$temporary/uninstall.sh"
cp -- "$source_dir/README.md" "$temporary/README.md"
cp -- "$source_dir/LICENSE" "$temporary/LICENSE"
printf '%s\n' "$plugin_id" > "$temporary/.herdr-context-owned"
chmod 755 "$temporary/herdr-context" "$temporary/install.sh" "$temporary/uninstall.sh"
chmod 644 "$temporary/herdr-plugin.toml" "$temporary/README.md" "$temporary/LICENSE" "$temporary/.herdr-context-owned"
mv -- "$temporary" "$install_root"
temporary=

run_herdr plugin unlink "$plugin_id" >/dev/null 2>&1 || true
if ! run_herdr plugin link "$install_root" >/dev/null; then
    rm -rf -- "$install_root"
    if [ -n "$backup" ] && [ -e "$backup" ]; then
        mv -- "$backup" "$install_root"
        backup=
        if ! run_herdr plugin link "$install_root" >/dev/null; then
            printf '%s\n' \
                "install: rollback restored files but Herdr registration failed" >&2
            exit 1
        fi
    fi
    printf '%s\n' "install: Herdr refused the packaged plugin" >&2
    exit 1
fi

if [ -n "$backup" ]; then
    rm -rf -- "$backup"
    backup=
fi
trap - EXIT HUP INT TERM
printf '%s\n' "Installed $plugin_id in $install_root"
