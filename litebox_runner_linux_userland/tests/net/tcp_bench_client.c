// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/// Client for TCP throughput benchmark. Runs on host, sends bulk data to server.
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

int main(int argc, char *argv[]) {
    const char *ip_addr = argc > 1 ? argv[1] : "10.0.0.2";
    int port = argc > 2 ? atoi(argv[2]) : PORT;
    long total_bytes = argc > 3 ? atol(argv[3]) : (4 * 1024 * 1024); // 4MB default

    int sock = socket(AF_INET, SOCK_STREAM, 0);
    if (sock < 0) { perror("socket"); return 1; }

    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    inet_pton(AF_INET, ip_addr, &addr.sin_addr);

    printf("BENCH_CLIENT: connecting to %s:%d, sending %ld bytes\n", ip_addr, port, total_bytes);

    if (connect(sock, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("connect"); return 1;
    }

    char *buf = malloc(BUFFER_SIZE);
    memset(buf, 'A', BUFFER_SIZE);
    long sent = 0;

    while (sent < total_bytes) {
        long remaining = total_bytes - sent;
        int chunk = remaining < BUFFER_SIZE ? remaining : BUFFER_SIZE;
        ssize_t n = send(sock, buf, chunk, 0);
        if (n <= 0) { perror("send"); break; }
        sent += n;
    }

    // Shutdown write side and read result
    shutdown(sock, SHUT_WR);
    char result[256] = {0};
    recv(sock, result, sizeof(result) - 1, 0);
    printf("BENCH_CLIENT: sent=%ld bytes, server reported throughput=%s Mbps\n", sent, result);

    free(buf);
    close(sock);
    return 0;
}
