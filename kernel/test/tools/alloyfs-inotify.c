// SPDX-License-Identifier: GPL-2.0-only
/*
 * alloyfs-inotify — a minimal, exact inotify probe for the AlloyFS test harness.
 *
 * Why not inotify-tools: its output format hides the rename COOKIE, which is
 * the single field this whole project hinges on (a MOVED_FROM/MOVED_TO pair
 * is only correct if both carry the same nonzero cookie). It is also
 * dynamically linked, which we cannot use in a static initramfs.
 *
 * Output: one line per event, fields space-separated and stable:
 *     EV <wd> <mask-hex> <cookie> <name>
 * `name` is "-" when the event carries none (watch is on the file itself).
 * Exits 0 on clean timeout, 1 on usage/setup error.
 *
 * Build: gcc -static -Os -o alloyfs-inotify alloyfs-inotify.c
 */
#include <errno.h>
#include <limits.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <unistd.h>

#define BUF_LEN (64 * (sizeof(struct inotify_event) + NAME_MAX + 1))

int main(int argc, char **argv)
{
	int timeout_ms = 5000;
	int i, fd, nwatch = 0;
	char buf[BUF_LEN];

	/* usage: alloyfs-inotify [-t SECONDS] PATH... */
	i = 1;
	if (argc > 2 && strcmp(argv[1], "-t") == 0) {
		timeout_ms = atoi(argv[2]) * 1000;
		i = 3;
	}
	if (i >= argc) {
		fprintf(stderr, "usage: alloyfs-inotify [-t SECONDS] PATH...\n");
		return 1;
	}

	fd = inotify_init1(0);
	if (fd < 0) {
		perror("inotify_init1");
		return 1;
	}
	for (; i < argc; i++) {
		int wd = inotify_add_watch(fd, argv[i], IN_ALL_EVENTS);

		if (wd < 0) {
			fprintf(stderr, "add_watch %s: %s\n", argv[i], strerror(errno));
			return 1;
		}
		/* So tests can map wd -> path deterministically. */
		printf("WD %d %s\n", wd, argv[i]);
		nwatch++;
	}
	printf("READY %d\n", nwatch);
	fflush(stdout);

	for (;;) {
		struct pollfd pfd = { .fd = fd, .events = POLLIN };
		ssize_t len;
		char *p;

		int r = poll(&pfd, 1, timeout_ms);

		if (r < 0) {
			if (errno == EINTR)
				continue;
			perror("poll");
			return 1;
		}
		if (r == 0) {		/* clean timeout: the normal end of a test */
			printf("TIMEOUT\n");
			fflush(stdout);
			return 0;
		}
		len = read(fd, buf, sizeof(buf));
		if (len <= 0) {
			if (len < 0 && errno == EINTR)
				continue;
			perror("read");
			return 1;
		}
		for (p = buf; p < buf + len; ) {
			struct inotify_event *e = (struct inotify_event *)p;

			printf("EV %d %08x %u %s\n", e->wd, e->mask, e->cookie,
			       (e->len && e->name[0]) ? e->name : "-");
			p += sizeof(struct inotify_event) + e->len;
		}
		fflush(stdout);
	}
}
