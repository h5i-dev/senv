# Small runtime image for `senv serve`.
# Build from a checkout of this repository:
#   docker build -t senv .
#   docker run --rm -p 8000:8000 senv
#
# The process listens on 0.0.0.0:8000. It does not open PostgreSQL.
# The image was not built in the environment that validated the Echo client:
# that host's Docker data root sits on a nearly full root disk.

FROM rust:1.88-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin senv

FROM debian:bookworm-slim
RUN useradd --system --create-home --shell /usr/sbin/nologin senv
COPY --from=build /src/target/release/senv /usr/local/bin/senv
USER senv
EXPOSE 8000
ENV SENV_BIND=0.0.0.0:8000
ENTRYPOINT ["senv", "serve"]
