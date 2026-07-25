# A desktop and a VNC server, for testing desktui against something real.
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

# Ubuntu's own firefox and chromium-browser packages are empty shells whose only job
# is to install a snap, and snapd does not run in a container: apt reports success and
# the desktop is left with no browser at all. Mozilla's repository ships a real deb.
# The pin is the part that matters -- the transitional package is versioned 1:1snap1
# with an epoch, so it wins on version number alone, and without a priority above the
# archive's 500 apt keeps handing back the stub.
RUN apt-get update && apt-get install -y --no-install-recommends \
        wget \
        ca-certificates \
    && install -d -m 0755 /etc/apt/keyrings \
    && wget -qO /etc/apt/keyrings/packages.mozilla.org.asc \
        https://packages.mozilla.org/apt/repo-signing-key.gpg \
    && echo 'deb [signed-by=/etc/apt/keyrings/packages.mozilla.org.asc] https://packages.mozilla.org/apt mozilla main' \
        > /etc/apt/sources.list.d/mozilla.list \
    && printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n' \
        > /etc/apt/preferences.d/mozilla \
    && apt-get update && apt-get install -y --no-install-recommends firefox \
    && rm -rf /var/lib/apt/lists/*

# Installing a browser is not the same as being able to find one, and two separate
# lookups have to succeed. The panel's launcher asks exo for the WebBrowser helper,
# which Debian defaults to sensible-browser; sensible-browser walks the x-www-browser
# alternative, which Mozilla's deb does not register. So the launcher fails with EIO --
# "Input/output error" -- rather than anything that names a missing package. Name the
# helper outright, and register the alternative for everything that goes the other way
# round, xdg-open and any application opening a link of its own.
RUN sed -i 's/^WebBrowser=.*/WebBrowser=firefox/' /etc/xdg/xfce4/helpers.rc \
    && update-alternatives --install /usr/bin/x-www-browser x-www-browser /usr/bin/firefox 200

# The container starts with no profile, so every run is a first run, and Firefox's
# first run is a modal sitting on top of whatever page was asked for. Enterprise policy
# is the supported way to answer it in advance, and /etc/firefox/policies is read
# before a profile exists -- which is the point, since anything written into a profile
# would be too late for the window that opens with it.
#
# What the four settings answer: the terms-of-use and data-collection notice, the
# about:welcome content behind it, the about:welcome tab a first launch opens on its
# own, and the offer to become the default browser, which is not a question worth
# asking in a container holding exactly one.
COPY firefox-policies.json /etc/firefox/policies/policies.json

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
ENV VNC_PASSWORD=desktui \
    VNC_GEOMETRY=1280x800 \
    VNC_DISPLAY=:1 \
    VNC_PORT=5901

EXPOSE 5901
USER desktop
WORKDIR /home/desktop

# A shell of its own so `docker stop` reaches the X server rather than a wrapper.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
