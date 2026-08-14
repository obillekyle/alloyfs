// SPDX-License-Identifier: GPL-2.0-only
/*
 * alloyfs-lock — take an advisory lock and say what happened, in one word.
 *
 * The shell needs to distinguish "refused" from "failed", which `flock(1)`
 * blurs into exit 1, and it needs a holder that keeps the lock for a known
 * time without keeping a shell alive around it. Both lock flavours are here
 * because the module implements them through different file_operations
 * hooks (->lock and ->flock) and only one of them is exercised by fcntl.
 *
 *   alloyfs-lock [-p|-f] [-s|-x] [-n] [-w MS] FILE
 *     -p  POSIX record lock via fcntl   (default)
 *     -f  BSD lock via flock
 *     -s  shared          -x  exclusive (default)
 *     -n  non-blocking: report BUSY rather than waiting
 *     -w  hold for MS milliseconds, then release and exit
 *
 * Prints exactly one of:
 *   LOCKED   acquired (and held for -w, if given)
 *   BUSY     someone else holds a conflicting lock
 *   ERR n    anything else, with the errno
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	int posix = 1, excl = 1, block = 1, hold_ms = 0;
	const char *path = NULL;
	int fd, rc, i;

	for (i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "-p"))
			posix = 1;
		else if (!strcmp(argv[i], "-f"))
			posix = 0;
		else if (!strcmp(argv[i], "-s"))
			excl = 0;
		else if (!strcmp(argv[i], "-x"))
			excl = 1;
		else if (!strcmp(argv[i], "-n"))
			block = 0;
		else if (!strcmp(argv[i], "-w") && i + 1 < argc)
			hold_ms = atoi(argv[++i]);
		else
			path = argv[i];
	}
	if (!path) {
		fprintf(stderr, "usage: alloyfs-lock [-p|-f] [-s|-x] [-n] [-w MS] FILE\n");
		return 2;
	}

	/* O_RDWR even for a shared lock: the daemon opens a write handle where
	 * it can, and opening read-only here would only obscure which side is
	 * responsible if that ever regresses. */
	fd = open(path, O_RDWR);
	if (fd < 0) {
		printf("ERR %d\n", errno);
		fflush(stdout);
		return 3;
	}

	if (posix) {
		struct flock fl;

		memset(&fl, 0, sizeof(fl));
		fl.l_type = excl ? F_WRLCK : F_RDLCK;
		fl.l_whence = SEEK_SET;
		fl.l_start = 0;
		fl.l_len = 0;	/* whole file, which is all the wire carries */
		rc = fcntl(fd, block ? F_SETLKW : F_SETLK, &fl);
	} else {
		rc = flock(fd, (excl ? LOCK_EX : LOCK_SH) | (block ? 0 : LOCK_NB));
	}

	if (rc < 0) {
		/* EACCES is the other spelling of "someone has it" that POSIX
		 * permits for F_SETLK; treating it as an error would make this
		 * test flaky rather than wrong. */
		if (errno == EAGAIN || errno == EACCES || errno == EWOULDBLOCK)
			printf("BUSY\n");
		else
			printf("ERR %d\n", errno);
		fflush(stdout);
		return 1;
	}

	printf("LOCKED\n");
	fflush(stdout);

	if (hold_ms > 0) {
		struct timespec ts = {
			.tv_sec = hold_ms / 1000,
			.tv_nsec = (long)(hold_ms % 1000) * 1000000L,
		};
		nanosleep(&ts, NULL);
	}
	/* Exiting closes the fd, which is how a real program usually releases
	 * a lock — so the close path gets exercised rather than an explicit
	 * unlock the kernel treats differently. */
	close(fd);
	return 0;
}
