# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

CC ?= gcc
CFLAGS ?= -Wall -Wextra -Werror -O2 -static
TARGET = litebox-sh

.PHONY: all clean

all: $(TARGET)

$(TARGET): litebox-sh.c
	$(CC) $(CFLAGS) -o $@ $<

clean:
	rm -f $(TARGET)
