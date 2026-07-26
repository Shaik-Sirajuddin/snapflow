# ACP Node runtime matrix: bare Ubuntu without nodejs, plus tools for
# official Node tarball extract and acpx-server e2e.
FROM ubuntu:24.04

RUN apt-get update -qq && \
    apt-get install -y -qq --no-install-recommends \
        curl ca-certificates tar xz-utils coreutils procps bash \
        python3 && \
    rm -rf /var/lib/apt/lists/*

# Deliberately do NOT install nodejs/npm from apt — M1/M2 require absence.
RUN useradd -m -s /bin/bash tester
USER tester
WORKDIR /home/tester

# Runtime scripts + entrypoint from repo (build context = repo root).
COPY --chown=tester:tester scripts/lib/acp-node-runtime.sh /home/tester/lib/acp-node-runtime.sh
COPY --chown=tester:tester docker-tests/acp-node-runtime/entrypoint-test.sh /home/tester/entrypoint-test.sh
RUN chmod +x /home/tester/entrypoint-test.sh /home/tester/lib/acp-node-runtime.sh

# acpx-server binary bind-mounted at runtime as /acpx-server (run-test.sh).
ENTRYPOINT ["/home/tester/entrypoint-test.sh"]
