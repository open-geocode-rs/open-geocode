# The og server and nothing else: the binary links only glibc, libm and
# libgcc_s (it needs glibc 2.39), which this base provides.
FROM gcr.io/distroless/cc-debian13@sha256:58e12be899275afaa05f1b03051ab19037ed7f252528e57a3a9ad7e220a62578
COPY --from=bin og /usr/local/bin/og
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/og"]
