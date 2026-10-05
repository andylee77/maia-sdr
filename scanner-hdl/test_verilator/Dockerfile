# Verilator for the radio core's whole-field simulations (doc/changes/085): Ubuntu 24.04's package,
# a C++ compiler and make. The tests mount the repository at /work.
FROM ubuntu:24.04
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
       verilator g++ make python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /work
