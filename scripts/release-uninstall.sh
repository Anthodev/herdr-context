#!/bin/sh
set -eu

plugin_id=herdr-context
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
    *) printf '%s\n' "uninstall: HERDR_CONTEXT_INSTALL_DIR must be absolute" >&2; exit 2 ;;
esac
if [ "$install_root" = "/" ] || [ -L "$install_root" ]; then
    printf '%s\n' "uninstall: unsafe installation root: $install_root" >&2
    exit 2
fi
if [ ! -e "$install_root" ]; then
    printf '%s\n' "$plugin_id is not installed in $install_root"
    exit 0
fi
if [ ! -d "$install_root" ] ||
    [ ! -f "$install_root/.herdr-context-owned" ] ||
    [ "$(cat "$install_root/.herdr-context-owned")" != "$plugin_id" ]; then
    printf '%s\n' "uninstall: refusing to remove unowned path: $install_root" >&2
    exit 2
fi

if ! run_herdr plugin unlink "$plugin_id" >/dev/null; then
    printf '%s\n' "uninstall: Herdr refused to unlink $plugin_id; files were kept" >&2
    exit 1
fi
rm -rf -- "$install_root"
printf '%s\n' "Uninstalled $plugin_id; Herdr config and state were preserved"
