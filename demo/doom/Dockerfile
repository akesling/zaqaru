# The Doom bench: a container whose board zaqaru's kernel runs (see
# /etc/zaqaru/board), Hubris's Doom on it, with Humility beside it and the
# Hubris bench's front door on port 80. build.sh stages what this copies.
#
# Debian-based for glibc, which is the tested libc.
FROM --platform=linux/amd64 debian:bookworm-slim

COPY humility zaqaru-m0 /usr/local/bin/
COPY final.bin hubris.zip doom1.wad /hubris/
COPY archives /hubris/archives/
COPY board /etc/zaqaru/board

# One malloc arena: see demo/hubris/Dockerfile.
ENV MALLOC_ARENA_MAX=1

EXPOSE 80
CMD ["/usr/local/bin/zaqaru-m0", "door", "--archive", "/hubris/hubris.zip", "--probe", "usb"]
