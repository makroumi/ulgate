# Build locally first: cargo build --release
# Then: docker build -t ulgate .
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY target/release/ulgate /usr/local/bin/ulgate
RUN mkdir -p /data
EXPOSE 8080
ENV PORT=8080
ENV ULGATE_DB=/data
ENTRYPOINT ["ulgate"]
CMD ["--port", "8080"]
