// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/// Benchmark for TCP round-trip latency over TUN device.
/// Server echoes back each message, measuring per-message RTT.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>

#define PORT 12347
#define BUFFER_SIZE 1024

int main(int argc, char *argv[]) {
    const char *ip_addr = argc > 1 ? argv[1] : "10.0.0.2";
    int port = argc > 2 ? atoi(argv[2]) : PORT;
    int num_pings = argc > 3 ? atoi(argv[3]) : 100;

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

    printf("LATENCY_SERVER: listening on %s:%d, expecting %d pings\n", ip_addr, port, num_pings);
    fflush(stdout);

    int conn_fd = accept(server_fd, NULL, NULL);
    if (conn_fd < 0) { perror("accept"); return 1; }

    char buf[BUFFER_SIZE];
    for (int i = 0; i < num_pings; i++) {
        ssize_t n = recv(conn_fd, buf, sizeof(buf), 0);
        if (n <= 0) break;
        send(conn_fd, buf, n, 0);
    }

    close(conn_fd);
    close(server_fd);
    return 0;
}
