# Runtime image for praxis-extproc.
#
# Expects a prebuilt binary at .container/praxis-extproc
# (produced by `make container` or `make container-release`).
#
# Build:
#   make container-release
#
# Run:
#   docker run -p 50051:50051 -p 50052:50052 -p 9090:9090 \
#     -v $(pwd)/examples/praxis-extproc.yaml:/etc/praxis/extproc.yaml \
#     praxis-extproc:dev -c /etc/praxis/extproc.yaml

FROM registry.access.redhat.com/ubi10/ubi-minimal

RUN microdnf install -y shadow-utils \
    && microdnf clean all \
    && groupadd -r praxis \
    && useradd -r -g praxis praxis

COPY .container/praxis-extproc /usr/local/bin/praxis-extproc

USER praxis

EXPOSE 50051 50052 9090

ENTRYPOINT ["praxis-extproc"]
CMD ["-c", "/etc/praxis/extproc.yaml"]
