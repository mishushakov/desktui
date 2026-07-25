#!/bin/bash
#
# Start the X server, wait for it, then start a session against it.
#
# Waiting for the display socket rather than sleeping is the difference between a
# desktop that comes up and one that comes up most of the time.

set -uo pipefail

: "${VNC_PASSWORD:=desktui}"
: "${VNC_GEOMETRY:=1280x800}"
: "${VNC_DISPLAY:=:1}"
: "${VNC_PORT:=5901}"

DISPLAY_NUM="${VNC_DISPLAY#:}"

# vncpasswd -f reads the password on stdin and writes the obfuscated blob to stdout.
# Only the first eight bytes are used, so say so rather than let it be a surprise.
if [ "${#VNC_PASSWORD}" -gt 8 ]; then
    printf 'note: only the first 8 characters of VNC_PASSWORD are used (DES key size)\n' >&2
fi

# vncpasswd lives in tigervnc-tools, not in the server package. Check for it rather
# than discovering the omission as an empty password file and a server that answers
# every client with "No password configured for VNC Auth".
command -v vncpasswd >/dev/null 2>&1 ||
    {
        printf 'error: vncpasswd is missing (it is in the tigervnc-tools package)\n' >&2
        exit 1
    }

mkdir -p "$HOME/.vnc"
chmod 700 "$HOME/.vnc"
printf '%s\n' "$VNC_PASSWORD" | vncpasswd -f >"$HOME/.vnc/passwd"
chmod 600 "$HOME/.vnc/passwd"

# An empty blob means the server will start and then refuse every connection, which
# is a worse failure than not starting at all: it looks like the client's fault.
if [ ! -s "$HOME/.vnc/passwd" ]; then
    printf 'error: could not write a VNC password file; refusing to start with authentication broken\n' >&2
    exit 1
fi

# A restarted container inherits nothing, but a re-run entrypoint might.
rm -f "/tmp/.X${DISPLAY_NUM}-lock" "/tmp/.X11-unix/X${DISPLAY_NUM}"

export XDG_RUNTIME_DIR="/tmp/runtime-$(id -u)"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

# -localhost is deliberately absent: inside a container "localhost" is the container,
# so binding to it would refuse the published port. The publish address is where this
# gets restricted -- see the Makefile.
Xtigervnc "$VNC_DISPLAY" \
    -geometry "$VNC_GEOMETRY" \
    -depth 24 \
    -desktop desktui-test \
    -rfbauth "$HOME/.vnc/passwd" \
    -rfbport "$VNC_PORT" \
    -SecurityTypes VncAuth \
    -AlwaysShared \
    -AcceptSetDesktopSize &

for _ in $(seq 1 100); do
    [ -e "/tmp/.X11-unix/X${DISPLAY_NUM}" ] && break
    sleep 0.1
done
if [ ! -e "/tmp/.X11-unix/X${DISPLAY_NUM}" ]; then
    printf 'error: X server never came up on %s\n' "$VNC_DISPLAY" >&2
    exit 1
fi

export DISPLAY="$VNC_DISPLAY"
# Something to look at, and a way to tell at a glance that the desktop is live.
xsetroot -solid '#1d3040' 2>/dev/null || true

dbus-run-session -- xfce4-session &

printf 'vnc ready on port %s, desktop %s at %s\n' "$VNC_PORT" "$VNC_DISPLAY" "$VNC_GEOMETRY"

# Whichever job ends first ends the container, so a dead X server is a dead
# container rather than a hung one.
wait -n
exit $?
