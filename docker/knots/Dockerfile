# Bitcoin Knots regtest image for the e2e harness.
#
# Binaries are copied from a local build rather than compiled here, so the
# image is not reproducible on another machine. That is acceptable while
# this runs locally; see mission 02.1 open question 2 before using it in CI.
#
# Build with docker/knots/build.sh, which stages the binaries into a
# context directory.
#
# The base must match the glibc the binaries were built against.
FROM ubuntu:26.04

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libevent-core-2.1-7t64 \
      libevent-extra-2.1-7t64 \
      libevent-pthreads-2.1-7t64 \
      libsqlite3-0 \
 && rm -rf /var/lib/apt/lists/*

COPY bitcoind bitcoin-cli /usr/local/bin/

# Fail the build rather than the test run if the binaries do not execute.
RUN bitcoind -version | head -1

# regtest RPC and p2p
EXPOSE 18443 18444

ENTRYPOINT ["bitcoind"]
