#!/usr/bin/env bash
# Builds DataReco's test disk images inside a Linux container (Docker).
# On Linux you can also skip Docker:  sudo python3 testdata/make_images.py
# Extra arguments go to make_images.py, e.g.  ./testdata/build-images.sh --only fat32
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
docker build --tag datareco-testimages "$here/docker"
# --privileged is needed to use loop devices and mount the images inside the container.
docker run --rm --privileged --volume "$(dirname "$here"):/work" datareco-testimages "$@"
