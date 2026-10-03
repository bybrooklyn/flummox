#!/usr/bin/env bash
# Run as root inside a disposable Arch container, with the package as $1.
set -euo pipefail
package=$1
pacman -Sy --noconfirm
pacman -U --noconfirm "$package"
pacman -S --noconfirm --needed \
    desktop-file-utils procps-ng xorg-server-xvfb vulkan-swrast \
    ttf-dejavu imagemagick xorg-xwd
useradd -m flummox-test
runuser -u flummox-test -- flummox --version
desktop-file-validate /usr/share/applications/flummox.desktop
systemd-analyze verify /usr/lib/systemd/user/flummox-watch.service
runuser -u flummox-test -- flummox --json jobs
previous=$(pgrep -u flummox-test -f '^/usr/bin/flummox __coordinator$')
pacman -U --noconfirm "$package"
kill -0 "$previous"
runuser -u flummox-test -- flummox jobs restart
replacement=$(pgrep -u flummox-test -f '^/usr/bin/flummox __coordinator$')
if [ "$previous" = "$replacement" ]; then
    echo 'Restart did not replace the coordinator' >&2
    exit 1
fi
runuser -u flummox-test -- flummox --json jobs

Xvfb :99 -screen 0 1280x900x24 -ac > /tmp/xvfb.log 2>&1 &
display_pid=$!
trap 'kill "$display_pid" "$replacement" 2>/dev/null || true' EXIT
sleep 2
runuser -u flummox-test -- env DISPLAY=:99 XDG_RUNTIME_DIR=/tmp \
    flummox-gui > /tmp/gui.log 2>&1 &
gui_pid=$!
sleep 10
if ! kill -0 "$gui_pid"; then
    cat /tmp/gui.log >&2
    exit 1
fi
DISPLAY=:99 xwd -root -silent -out /tmp/gui.xwd
magick /tmp/gui.xwd /validation/arch-gui.png
cat /tmp/gui.log
kill "$gui_pid"
echo 'Arch install, running-coordinator upgrade, restart, and GUI startup passed'
