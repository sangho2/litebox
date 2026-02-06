// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/// Benchmark for TCP throughput over TUN device.
/// Runs as server inside LiteBox, client sends bulk data from host.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <time.h>

#define PORT 12346
#define BUFFER_SIZE (64 * 1024)

static long long timespec_diff_us(struct timespec *start, struct timespec *end) {
    return (end->tv_sec - start->tv_sec) * 1000000LL +
           (end->tv_nsec - start->tv_nsec) / 1000;
}

int main(int argc, char *argv[]) {
    const char *ip_addr = argc > 1 ? argv[1] : "10.0.0.2";
    int port = argc > 2 ? atoi(argv[2]) : PORT;
    long total_bytes = argc > 3 ? atol(argv[3]) : (4 * 1024 * 1024); // 4MB default

    int server_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (server_fd < 0) { perror("socket"); return 1; }

    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    inet_pton(AF_INET, ip_addr, &addr.sin_addr);

    if (bind(server_fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("bind"); return 1;
    }
    if (listen(server_fd, 1) < 0) { perror("listen"); return 1; }

    printf("BENCH_SERVER: listening on %s:%d, expecting %ld bytes\n", ip_addr, port, total_bytes);
    fflush(stdout);

    int conn_fd = accept(server_fd, NULL, NULL);
    if (conn_fd < 0) { perror("accept"); return 1; }

    char *buf = malloc(BUFFER_SIZE);
    long received = 0;
    struct timespec start, end;
    clock_gettime(CLOCK_MONOTONIC, &start);

    while (received < total_bytes) {
        ssize_t n = recv(conn_fd, buf, BUFFER_SIZE, 0);
        if (n <= 0) break;
        received += n;
    }

    clock_gettime(CLOCK_MONOTONIC, &end);
    long long elapsed_us = timespec_diff_us(&start, &end);
    double elapsed_s = elapsed_us / 1000000.0;
    double mbps = (received * 8.0) / (elapsed_s * 1000000.0);

    printf("BENCH_RESULT: received=%ld bytes, time=%.3f s, throughput=%.2f Mbps\n",
           received, elapsed_s, mbps);
    fflush(stdout);

    // Send result back to client
    char result[256];
    int len = snprintf(result, sizeof(result), "%.2f", mbps);
    send(conn_fd, result, len, 0);

    free(buf);
    close(conn_fd);
    close(server_fd);
    return 0;
}
