# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

SRC = rtld_audit_arm64.c
OUT_DIR ?= .
OUTPUT = $(OUT_DIR)/litebox_rtld_audit_arm64.so
CC ?= cc
CFLAGS ?= -Wall -Werror -fPIC -shared -nostdlib -fno-stack-protector
ARCH ?= $(shell uname -m)
ifeq ($(ARCH),aarch64)
    # No special flags needed for aarch64
else
    $(error Unsupported target architecture: $(ARCH). Expected aarch64.)
endif
ifdef DEBUG
    CFLAGS += -DDEBUG
endif
all: $(OUTPUT)

$(OUTPUT): $(SRC)
	$(CC) $(CFLAGS) -o $@ $<

clean:
	rm -f $(OUTPUT)

.PHONY: all clean
