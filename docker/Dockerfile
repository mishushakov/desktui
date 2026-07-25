# A desktop and a VNC server, for testing vnctui against something real.
#
# TigerVNC on purpose: its -AcceptSetDesktopSize defaults to on, so a client asking
# for its own size is actually honoured. That is the path worth testing, and the one
# a server like x11vnc refuses.
FROM ubuntu:24.04

# Keep apt from asking about keyboard layouts and time zones mid-build.
ENV DEBIAN_FRONTEND=noninteractive

# --no-install-recommends, then name what a desktop actually needs. The recommends
# chain for xfce4 pulls in a display manager, a screensaver and a power manager,
# none of which mean anything in a container.
#
# The last two are here because dropping the recommends breaks the icons, and the
# failure does not look like a missing package: you get a panel of mismatched sizes
# rather than an error. elementary-xfce-icon-theme is the theme xfce4-settings
# already asks for by default on Ubuntu, and nothing pulls it in; without it the
# lookup falls through to hicolor, which holds application icons at fixed sizes and
# nothing else. librsvg2-common carries the gdk-pixbuf SVG loader, which the tail of
# the chain needs -- elementary-xfce inherits from Adwaita, and Adwaita is 720 SVGs
# to 51 PNGs. GTK only rescales an icon when the caller asks it to, so a lookup that
# lands on the wrong file renders at that file's own size.
RUN apt-get update && apt-get install -y --no-install-recommends \
        tigervnc-standalone-server \
        tigervnc-tools \
        xfce4-session \
        xfwm4 \
        xfdesktop4 \
        xfce4-panel \
        xfce4-settings \
        xfce4-terminal \
        thunar \
        dbus-x11 \
        x11-utils \
        x11-xserver-utils \
        fonts-dejavu-core \
        elementary-xfce-icon-theme \
        librsvg2-common \
        procps \
    && rm -rf /var/lib/apt/lists/*

# A desktop should not run as root: some XFCE components object, and a container
# that is trivially rootless may as well be.
RUN useradd --create-home --shell /bin/bash desktop

# The X server cannot create this itself as an unprivileged user, and while it
# happens to work without it, relying on that is asking for an intermittent failure.
RUN mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 755 /usr/local/bin/entrypoint.sh

# Only the first eight characters of this ever matter: that is the whole of the DES
# key VNC authentication uses. Override with -e VNC_PASSWORD=... if you like, but do
# not mistake it for protection -- the session that follows is in the clear, which is
# why the compose file and the Makefile publish this to 127.0.0.1 only.
ENV VNC_PASSWORD=vnctui \
    VNC_GEOMETRY=1280x800 \
    VNC_DISPLAY=:1 \
    VNC_PORT=5901

EXPOSE 5901
USER desktop
WORKDIR /home/desktop

# A shell of its own so `docker stop` reaches the X server rather than a wrapper.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
