# Shopping Sessions: one statically linked Rust binary in an empty image.
# The scorer builds for linux/amd64 (Intel Xeon, Cascade Lake or newer).
FROM rust:1.97-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
RUN cargo build --release --locked

FROM scratch
COPY --from=build /src/target/release/pipeline /pipeline
CMD ["/pipeline"]
