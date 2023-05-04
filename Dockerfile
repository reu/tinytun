FROM rust:1-slim-buster as build

ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
RUN mkdir /app
WORKDIR /app
RUN cargo new --lib tinytun
RUN cargo new --lib tinytun-client
RUN cargo new --bin tinytun-server
COPY Cargo.toml /app/
COPY Cargo.lock /app/
COPY tinytun/Cargo.toml /app/tinytun/
COPY tinytun-server/Cargo.toml /app/tinytun-server/
RUN cargo build --release --package tinytun-server

COPY tinytun/src /app/tinytun/src
COPY tinytun-server/src /app/tinytun-server/src
RUN touch tinytun/src/lib.rs
RUN touch tinytun-server/src/main.rs
RUN cargo build --release --package tinytun-server

FROM debian:buster-slim
COPY --from=build /app/target/release/tinytun-server /app/tinytun-server
CMD /app/tinytun-server
