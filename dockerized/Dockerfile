FROM --platform=linux/amd64 rust:1.73.0 AS chef
ARG RELEASE_TAG
WORKDIR /ebu
RUN apt-get update && apt-get install -y git cmake clang pkg-config libssl-dev build-essential
RUN echo "a"
RUN git clone https://github.com/TennisBowling/executionbackup.git .
RUN if [[ "$RELEASE_TAG" ]] ; then git checkout $RELEASE_TAG ; fi
RUN cargo install --path .

FROM ubuntu:22.04
RUN apt-get update && apt-get install -y libssl-dev && rm -rf /var/lib/apt/lists/*
RUN ls /usr/lib
COPY --from=chef /usr/local/cargo/bin/executionbackup /usr/local/bin/executionbackup
RUN chmod +x /usr/local/bin/executionbackup
ENTRYPOINT ["/scripts/init.sh"]
