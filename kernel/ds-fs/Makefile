# SPDX-License-Identifier: GPL-2.0-only
# Out-of-tree build. KDIR defaults to the running kernel's build tree; the
# test harness passes it explicitly so the module always matches the kernel
# the guest will boot.
KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(shell pwd)

all:
	$(MAKE) -C $(KDIR) M=$(PWD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean

check:
	$(MAKE) -C $(KDIR) M=$(PWD) C=1 CHECK=sparse modules

.PHONY: all clean check
