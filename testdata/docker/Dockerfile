# Linux toolbox for building DataReco's test disk images.
# Used by testdata/build-images.ps1 and build-images.sh; see testdata/README.md.
FROM ubuntu:24.04

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      python3 dosfstools exfatprogs exfat-fuse ntfs-3g e2fsprogs hfsprogs fdisk util-linux mount kmod \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /work
ENTRYPOINT ["python3", "/work/testdata/make_images.py"]
