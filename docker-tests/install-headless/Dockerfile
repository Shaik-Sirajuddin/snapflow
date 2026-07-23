# Minimal Ubuntu image to test scripts/install.sh's real curl|bash flow and
# confirm snapflowd launches and stays up headlessly (no display, no desktop
# session). Deliberately close to a bare "just installed Ubuntu" system --
# no video-editor build deps here, only what install.sh itself needs
# (curl, tar, bash, a real unprivileged user).
FROM ubuntu:24.04

RUN apt-get update -qq && \
    apt-get install -y -qq --no-install-recommends \
        curl ca-certificates tar coreutils procps && \
    rm -rf /var/lib/apt/lists/*

# install.sh should work as a normal user, not root -- create one and run
# everything as them, same as a real desktop Linux install.
RUN useradd -m -s /bin/bash tester
USER tester
WORKDIR /home/tester

# The real install.sh from this checkout (not a hardcoded copy) -- build
# context is the repo root, see run-test.sh.
COPY --chown=tester:tester scripts/install.sh /home/tester/install.sh
COPY --chown=tester:tester docker-tests/install-headless/entrypoint-test.sh /home/tester/entrypoint-test.sh
RUN chmod +x /home/tester/entrypoint-test.sh /home/tester/install.sh

# The downloaded CI artifact is bind-mounted at runtime (run-test.sh),
# not baked into the image -- keeps rebuilds fast, avoids duplicating a
# ~300MB tarball into image layers.
ENTRYPOINT ["/home/tester/entrypoint-test.sh"]
