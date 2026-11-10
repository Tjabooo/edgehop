#!/usr/bin/env bash
# Install edgehop for the current user: binary in ~/.local/bin, systemd user
# unit, example config if none exists. Re-run after `git pull` to update.
# Nothing here needs root, but the user must be able to read /dev/input/event*
# and write /dev/uinput (see README → Permissions).
#
# EDGEHOP_BINARY=/path/to/edgehop ./install.sh   install a binary built elsewhere
set -euo pipefail
src=$(cd "$(dirname "$0")" && pwd)
bindir=$HOME/.local/bin
units=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
conf=${XDG_CONFIG_HOME:-$HOME/.config}/edgehop

if [ -n "${EDGEHOP_BINARY:-}" ]; then
    bin=$EDGEHOP_BINARY
else
    (cd "$src" && cargo build --release --locked)
    bin=$src/target/release/edgehop
fi
mkdir -p "$bindir" "$units" "$conf"
install -m 755 "$bin" "$bindir/edgehop.new" && mv -f "$bindir/edgehop.new" "$bindir/edgehop"
install -m 644 "$src/systemd/edgehop.service" "$units/edgehop.service"

if [ ! -f "$conf/config.toml" ]; then
    sed "s|^psk = .*|psk = \"$("$bindir/edgehop" keygen)\"|" "$src/config.example.toml" > "$conf/config.toml"
    chmod 600 "$conf/config.toml"
    echo "wrote $conf/config.toml: set name and [[peers]], copy the psk to the other machine, then:"
    echo "  systemctl --user enable --now edgehop"
    exit 0
fi
chmod 600 "$conf/config.toml"
systemctl --user daemon-reload
systemctl --user enable edgehop >/dev/null 2>&1
systemctl --user restart edgehop
echo "edgehop $("$bindir/edgehop" --version | cut -d' ' -f2) installed and running"
