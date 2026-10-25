# The Hubris bench: a container whose board zaqaru's kernel runs (see
# /etc/zaqaru/board), with Humility beside it and a front door on port 80
# that runs a Humility command per connection. Humility reaches the board
# through its USB debug probe, which it finds as it would a probe plugged
# in. build.sh stages the four files this copies.
#
# Debian-based for glibc, which is the tested libc.
FROM --platform=linux/amd64 debian:bookworm-slim

COPY humility zaqaru-m0 /usr/local/bin/
COPY final.bin hubris.zip /hubris/
COPY board /etc/zaqaru/board

# One malloc arena. Every process here shares the module's one 4 GiB
# address space, and glibc reserves 64-128 MiB for each thread that
# allocates; a handful of those left Humility no room for its heap.
ENV MALLOC_ARENA_MAX=1

EXPOSE 80
CMD ["/usr/local/bin/zaqaru-m0", "door", "--archive", "/hubris/hubris.zip", "--probe", "usb"]
