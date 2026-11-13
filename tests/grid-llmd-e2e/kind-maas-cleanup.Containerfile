FROM registry.access.redhat.com/ubi9/ubi-minimal@sha256:907b68736aa798b2d38255b7aa070b2a70acb90803864a40f05d0ec47556ddd0

LABEL org.opencontainers.image.title="Kind MaaS API key cleanup job helper"
LABEL org.opencontainers.image.description="Non-root curl helper for MaaS AITenant cleanup in local Kind"

USER 1001
