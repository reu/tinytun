FROM rust:1-slim-buster as build

ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
RUN cargo new --bin app
WORKDIR /app
COPY tinytun-server/Cargo.toml /app/
COPY Cargo.lock /app/
RUN cargo build --release

COPY tinytun-server/src /app/src
RUN touch src/main.rs
RUN cargo build --release

FROM debian:buster-slim
COPY --from=build /app/target/release/tinytun-server /app/tinytun-server
CMD /app/tinytun-server
