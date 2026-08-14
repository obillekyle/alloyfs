// SPDX-License-Identifier: GPL-2.0-only
/*
 * ds-devtest — negative tests and a fuzzer for /dev/ds-fs.
 *
 * The device is the module's only attack surface: everything a daemon sends
 * is untrusted input arriving in kernel context. These cases assert the
 * SPECIFIC errno for each malformed shape (a kernel that rejects garbage
 * with the wrong error is still wrong, and "it didn't crash" is far too weak
 * a claim to rest on), then hose the device with randomness to catch what
 * enumeration missed.
 *
 *   ds-devtest --neg          one line per case: RESULT <name> <want> <got>
 *   ds-devtest --fuzz N [seed]
 *   ds-devtest --race N
 *
 * Build: gcc -static -Os -pthread -o ds-devtest ds-devtest.c
 */
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#include "../../ds-fs/uapi/ds_fs.h"

#define DEV "/dev/ds-fs"

static int fd;

static void result(const char *name, int want, int got)
{
	printf("RESULT %s %d %d\n", name, want, got);
	fflush(stdout);
}

/* Perform a write and report the errno it produced (0 = it succeeded). */
static int wr(const void *buf, size_t len)
{
	errno = 0;
	return write(fd, buf, len) < 0 ? errno : 0;
}

/* A response header with sane defaults, for mutating one field at a time. */
static void mk(struct dsfs_out_header *h, unsigned long long unique)
{
	memset(h, 0, sizeof(*h));
	h->len = sizeof(*h);
	h->error = 0;
	h->unique = unique;
}

/* Build a notification: header + entry + name, with a settable code. */
static size_t mk_notify(char *buf, int code, const char *name, unsigned int namelen,
			unsigned int claimed_namelen)
{
	struct dsfs_out_header *h = (void *)buf;
	struct dsfs_notify_entry *e = (void *)(buf + sizeof(*h));

	memset(buf, 0, sizeof(*h) + sizeof(*e) + namelen);
	e->parent = DSFS_ROOT_NODEID;
	e->namelen = claimed_namelen;
	if (namelen)
		memcpy(e + 1, name, namelen);
	h->len = sizeof(*h) + sizeof(*e) + namelen;
	h->error = code;
	h->unique = 0;		/* notification */
	return h->len;
}

static void negatives(void)
{
	struct dsfs_out_header h;
	char buf[512];
	char rbuf[sizeof(struct dsfs_in_header) + DSFS_MAX_PAYLOAD];
	void *page, *unmapped;
	size_t n;

	/* --- framing ---------------------------------------------------- */
	result("write_shorter_than_header", EINVAL, wr(&h, 4));

	mk(&h, 1);
	h.len = 4;					/* below the header itself */
	result("len_below_header", EINVAL, wr(&h, sizeof(h)));

	mk(&h, 1);
	h.len = sizeof(h) + 1000;			/* claims more than sent */
	result("len_exceeds_count", EINVAL, wr(&h, sizeof(h)));

	mk(&h, 1);
	h.len = 0xffffffffu;				/* absurd */
	result("len_absurd", EINVAL, wr(&h, sizeof(h)));

	/* --- correlation ------------------------------------------------- */
	mk(&h, 999999);					/* no such request */
	result("unknown_unique", ENOENT, wr(&h, sizeof(h)));

	/* --- notifications (unique == 0) --------------------------------- */
	n = mk_notify(buf, 4242, "x", 1, 1);		/* nonsense opcode */
	result("notify_bad_code", EINVAL, wr(buf, n));

	mk(&h, 0);
	h.error = DSFS_NOTIFY_CREATE;			/* no entry at all */
	result("notify_no_entry", EINVAL, wr(&h, sizeof(h)));

	n = mk_notify(buf, DSFS_NOTIFY_CREATE, "", 0, 0);
	result("notify_zero_namelen", EINVAL, wr(buf, n));

	n = mk_notify(buf, DSFS_NOTIFY_CREATE, "a/b", 3, 3);
	result("notify_slash_in_name", EINVAL, wr(buf, n));

	n = mk_notify(buf, DSFS_NOTIFY_CREATE, "..", 2, 2);
	result("notify_dotdot", EINVAL, wr(buf, n));

	n = mk_notify(buf, DSFS_NOTIFY_CREATE, ".", 1, 1);
	result("notify_dot", EINVAL, wr(buf, n));

	/* Claims a longer name than the payload holds: the classic overread. */
	n = mk_notify(buf, DSFS_NOTIFY_CREATE, "ab", 2, 200);
	result("notify_namelen_past_payload", EINVAL, wr(buf, n));

	/* --- reads -------------------------------------------------------- */
	errno = 0;
	result("read_buffer_too_small", EINVAL,
	       read(fd, rbuf, 16) < 0 ? errno : 0);

	errno = 0;					/* nothing queued, O_NONBLOCK */
	result("read_nonblock_empty", EAGAIN,
	       read(fd, rbuf, sizeof(rbuf)) < 0 ? errno : 0);

	/* --- bad addresses ------------------------------------------------ */
	result("write_null_pointer", EFAULT, wr(NULL, 64));
	result("write_kernel_pointer", EFAULT, wr((void *)0xffffffff81000000ULL, 64));

	/*
	 * NOT EFAULT: with an empty queue the emptiness check comes first, so
	 * the buffer is never touched. Asserting EAGAIN here pins that
	 * ordering — validate cheaply and refuse early, before believing any
	 * user pointer.
	 */
	errno = 0;
	result("read_null_ptr_empty_queue", EAGAIN,
	       read(fd, NULL, sizeof(rbuf)) < 0 ? errno : 0);

	/* Unmapped user address. */
	page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (page != MAP_FAILED) {
		munmap(page, 4096);
		result("write_unmapped_page", EFAULT, wr(page, 64));
	}

	/*
	 * Page-straddling: a buffer whose first page is mapped and whose
	 * second is not. A copy that checks only the start address passes the
	 * check and then faults halfway through — which must be an error, not
	 * a partial trusted read.
	 */
	page = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (page != MAP_FAILED) {
		unmapped = (char *)page + 4096;
		munmap(unmapped, 4096);
		/*
		 * Start 8 bytes before the hole so the 16-byte header copy
		 * CROSSES it. (Ending exactly at the boundary is not a
		 * straddle: the copy succeeds and you end up testing length
		 * validation instead — which is what this case did at first.)
		 */
		result("write_straddles_unmapped", EFAULT,
		       wr((char *)page + 4096 - 8,
			  sizeof(struct dsfs_out_header) + 16));
		munmap(page, 4096);
	}
}

/* ------------------------------------------------------------------ fuzz */

static unsigned long long rng_state;

static unsigned int rnd(void)
{
	rng_state = rng_state * 6364136223846793005ULL + 1442695040888963407ULL;
	return (unsigned int)(rng_state >> 33);
}

static void fuzz(long iterations)
{
	char buf[sizeof(struct dsfs_out_header) + DSFS_MAX_PAYLOAD];
	long i;

	for (i = 0; i < iterations; i++) {
		size_t len = rnd() % (sizeof(struct dsfs_out_header) + 4096);
		unsigned int shape = rnd() % 4;
		size_t j;

		for (j = 0; j < len && j < sizeof(buf); j++)
			buf[j] = (char)rnd();

		if (shape) {
			/* Mostly half-plausible frames: a real header with random
			 * fields is far more likely to reach deep code paths
			 * than uniform noise.
			 */
			struct dsfs_out_header *h = (void *)buf;

			h->len = (shape == 1) ? (unsigned int)len : rnd();
			h->unique = (rnd() % 3) ? 0 : rnd();
			h->error = (int)(rnd() % 8);
		}
		if (len < sizeof(struct dsfs_out_header))
			len = sizeof(struct dsfs_out_header);
		(void)write(fd, buf, len);

		/* Interleave reads so the two paths race in the kernel. */
		if (rnd() % 8 == 0) {
			char rbuf[sizeof(struct dsfs_in_header) + DSFS_MAX_PAYLOAD];

			(void)read(fd, rbuf, sizeof(rbuf));
		}
	}
	printf("FUZZ-DONE %ld\n", iterations);
	fflush(stdout);
}

/* ------------------------------------------------------------------ race */

static volatile int race_stop;

static void *spam(void *arg)
{
	char buf[256];

	memset(buf, 0x41, sizeof(buf));
	while (!race_stop) {
		struct dsfs_out_header *h = (void *)buf;

		h->len = sizeof(*h);
		h->unique = rnd();
		h->error = 0;
		(void)write(fd, buf, sizeof(*h));
		(void)read(fd, buf, sizeof(buf));
	}
	return NULL;
}

/*
 * close() while another thread is inside read()/write() on the same
 * descriptor: the connection teardown must not free anything the in-flight
 * syscalls still hold.
 */
static void race(long rounds)
{
	long i;

	for (i = 0; i < rounds; i++) {
		pthread_t t;

		fd = open(DEV, O_RDWR | O_NONBLOCK);
		if (fd < 0) {
			printf("RACE-OPEN-FAIL %d\n", errno);
			return;
		}
		race_stop = 0;
		if (pthread_create(&t, NULL, spam, NULL) != 0) {
			close(fd);
			printf("RACE-THREAD-FAIL\n");
			return;
		}
		usleep(2000);
		close(fd);		/* teardown under concurrent traffic */
		usleep(1000);
		race_stop = 1;
		pthread_join(t, NULL);
	}
	printf("RACE-DONE %ld\n", rounds);
	fflush(stdout);
}

int main(int argc, char **argv)
{
	const char *mode = argc > 1 ? argv[1] : "--neg";
	long n = argc > 2 ? atol(argv[2]) : 2000;

	rng_state = argc > 3 ? (unsigned long long)atol(argv[3]) : 20260814;

	if (!strcmp(mode, "--race")) {
		race(n);
		return 0;
	}

	fd = open(DEV, O_RDWR | O_NONBLOCK);
	if (fd < 0) {
		fprintf(stderr, "open %s: %s\n", DEV, strerror(errno));
		return 1;
	}
	if (!strcmp(mode, "--fuzz")) {
		printf("FUZZ-SEED %llu\n", rng_state);
		fuzz(n);
	} else {
		negatives();
	}
	close(fd);
	return 0;
}
