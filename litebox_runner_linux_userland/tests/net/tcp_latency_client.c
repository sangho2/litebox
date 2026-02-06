// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/// Client for TCP latency benchmark. Sends ping messages and measures RTT.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <time.h>

#define PORT 12347
#define MSG_SIZE 64

static long long timespec_diff_us(struct timespec *start, struct timespec *end) {
    return (end->tv_sec - start->tv_sec) * 1000000LL +
           (end->tv_nsec - start->tv_nsec) / 1000;
}

int main(int argc, char *argv[]) {
    const char *ip_addr = argc > 1 ? argv[1] : "10.0.0.2";
    int port = argc > 2 ? atoi(argv[2]) : PORT;
    int num_pings = argc > 3 ? atoi(argv[3]) : 100;

    int sock = socket(AF_INET, SOCK_STREAM, 0);
    if (sock < 0) { perror("socket"); return 1; }

    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    inet_pton(AF_INET, ip_addr, &addr.sin_addr);

    if (connect(sock, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("connect"); return 1;
    }

    char send_buf[MSG_SIZE];
    char recv_buf[MSG_SIZE];
    memset(send_buf, 'P', MSG_SIZE);

    long long *latencies = malloc(num_pings * sizeof(long long));
    long long total_us = 0;
    long long min_us = 999999999LL;
    long long max_us = 0;
    int successful = 0;

    // Warmup
    for (int i = 0; i < 5; i++) {
        send(sock, send_buf, MSG_SIZE, 0);
        recv(sock, recv_buf, MSG_SIZE, 0);
    }

    struct timespec total_start, total_end;
    clock_gettime(CLOCK_MONOTONIC, &total_start);

    for (int i = 0; i < num_pings; i++) {
        struct timespec start, end;
        clock_gettime(CLOCK_MONOTONIC, &start);
        ssize_t s = send(sock, send_buf, MSG_SIZE, 0);
        if (s <= 0) break;
        ssize_t r = recv(sock, recv_buf, MSG_SIZE, 0);
        if (r <= 0) break;
        clock_gettime(CLOCK_MONOTONIC, &end);

        long long rtt = timespec_diff_us(&start, &end);
        latencies[successful] = rtt;
        total_us += rtt;
        if (rtt < min_us) min_us = rtt;
        if (rtt > max_us) max_us = rtt;
        successful++;
    }

    clock_gettime(CLOCK_MONOTONIC, &total_end);

    // Sort for percentiles
    for (int i = 0; i < successful - 1; i++) {
        for (int j = i + 1; j < successful; j++) {
            if (latencies[j] < latencies[i]) {
                long long tmp = latencies[i];
                latencies[i] = latencies[j];
                latencies[j] = tmp;
            }
        }
    }

    double avg_us = successful > 0 ? (double)total_us / successful : 0;
    long long p50 = successful > 0 ? latencies[successful / 2] : 0;
    long long p99 = successful > 0 ? latencies[(int)(successful * 0.99)] : 0;

    printf("LATENCY_RESULT: pings=%d, avg=%.1f us, min=%lld us, p50=%lld us, p99=%lld us, max=%lld us\n",
           successful, avg_us, min_us, p50, p99, max_us);

    free(latencies);
    close(sock);
    return 0;
}
