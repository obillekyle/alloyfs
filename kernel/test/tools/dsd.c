// SPDX-License-Identifier: GPL-2.0-only
/*
 * dsd — a minimal ds-fs daemon for the test harness.
 *
 * Serves a hardcoded tree over /dev/ds-fs so the kernel module can be tested
 * without the real Rust client (that arrives in stage 6). It can also be
 * told to misbehave, which is the point: stages 3 and 5 need a daemon that
 * dies, hangs, or lies on demand.
 *
 *   dsd [--fd N] [--die-after N] [--hang-on OP] [--corrupt-len]
 *
 * With --fd it serves a descriptor it inherited, which is how the harness
 * gets the daemon and `mount -o fd=N` onto the SAME connection: the kernel
 * resolves fd= in the mounting process's table, so the two must share one
 * open file description.
 *
 * Prints "DSD-READY <fd>" once the device is open, then serves forever.
 * Build: gcc -static -Os -o dsd dsd.c
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "../../ds-fs/uapi/ds_fs.h"

#define DT_DIR 4
#define DT_REG 8

struct node {
	unsigned long long nodeid;
	unsigned long long parent;
	const char *name;
	unsigned int mode;
	const char *data;
};

/* nodeid 1 is the root, by ABI. */
static struct node tree[] = {
	{ 1, 0, "/",       S_IFDIR | 0755, NULL },
	{ 2, 1, "one.txt", S_IFREG | 0644, "first file" },
	{ 3, 1, "dir",     S_IFDIR | 0755, NULL },
	{ 4, 3, "two.txt", S_IFREG | 0644, "second file, in a subdirectory" },
};
static const int NNODES = sizeof(tree) / sizeof(tree[0]);

static long die_after = -1;
static const char *hang_on;
static int corrupt_len;

static struct node *find_node(unsigned long long nodeid)
{
	for (int i = 0; i < NNODES; i++)
		if (tree[i].nodeid == nodeid)
			return &tree[i];
	return NULL;
}

static struct node *find_child(unsigned long long parent, const char *name, int len)
{
	for (int i = 0; i < NNODES; i++) {
		if (tree[i].parent == parent && (int)strlen(tree[i].name) == len &&
		    !memcmp(tree[i].name, name, len)) {
			return &tree[i];
		}
	}
	return NULL;
}

static void fill_attr(struct dsfs_attr *a, const struct node *n)
{
	memset(a, 0, sizeof(*a));
	a->nodeid = n->nodeid;
	a->mode = n->mode;
	a->nlink = S_ISDIR(n->mode) ? 2 : 1;
	a->size = n->data ? strlen(n->data) : 0;
	a->mtime_ns = 1000000000ULL;
}

static int reply(int fd, unsigned long long unique, int error,
		 const void *payload, unsigned int len)
{
	char buf[sizeof(struct dsfs_out_header) + DSFS_MAX_PAYLOAD];
	struct dsfs_out_header *h = (void *)buf;

	if (len > DSFS_MAX_PAYLOAD)
		len = DSFS_MAX_PAYLOAD;
	h->len = sizeof(*h) + len;
	h->error = error;
	h->unique = unique;
	if (len)
		memcpy(buf + sizeof(*h), payload, len);
	if (corrupt_len)
		h->len = 0xffffffff;	/* the kernel must reject this */
	return write(fd, buf, sizeof(*h) + len) < 0 ? -1 : 0;
}

int main(int argc, char **argv)
{
	int fd, served = 0;
	char req[sizeof(struct dsfs_in_header) + DSFS_MAX_NAME + 16];

	fd = -1;
	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--fd") && i + 1 < argc)
			fd = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--die-after") && i + 1 < argc)
			die_after = atol(argv[++i]);
		else if (!strcmp(argv[i], "--hang-on") && i + 1 < argc)
			hang_on = argv[++i];
		else if (!strcmp(argv[i], "--corrupt-len"))
			corrupt_len = 1;
	}

	if (fd < 0) {
		fd = open("/dev/ds-fs", O_RDWR);
		if (fd < 0) {
			fprintf(stderr, "open /dev/ds-fs: %s\n", strerror(errno));
			return 1;
		}
	}
	printf("DSD-READY %d\n", fd);
	fflush(stdout);

	for (;;) {
		struct dsfs_in_header *h = (void *)req;
		const char *name = req + sizeof(*h);
		ssize_t n = read(fd, req, sizeof(req));

		if (n < 0) {
			if (errno == EINTR)
				continue;
			fprintf(stderr, "read: %s\n", strerror(errno));
			return errno == ENODEV ? 0 : 1;
		}
		if (n < (ssize_t)sizeof(*h))
			continue;

		if (die_after >= 0 && served >= die_after) {
			fprintf(stderr, "dsd: dying after %d requests\n", served);
			_exit(0);	/* no close(): exercise abrupt death */
		}
		served++;

		switch (h->opcode) {
		case DSFS_OP_LOOKUP: {
			struct node *n2;
			struct dsfs_attr a;
			int namelen = (int)(h->len - sizeof(*h));

			if (hang_on && !strcmp(hang_on, "lookup"))
				continue;	/* never answer: kernel must not wedge */
			n2 = find_child(h->nodeid, name, namelen);
			if (!n2) {
				reply(fd, h->unique, -ENOENT, NULL, 0);
				break;
			}
			fill_attr(&a, n2);
			reply(fd, h->unique, 0, &a, sizeof(a));
			break;
		}
		case DSFS_OP_GETATTR: {
			struct node *n2 = find_node(h->nodeid);
			struct dsfs_attr a;

			if (!n2) {
				reply(fd, h->unique, -ENOENT, NULL, 0);
				break;
			}
			fill_attr(&a, n2);
			reply(fd, h->unique, 0, &a, sizeof(a));
			break;
		}
		case DSFS_OP_READDIR: {
			char payload[DSFS_MAX_PAYLOAD];
			unsigned int used = 0;
			long long first;
			int idx = 0;

			/*
			 * `off` is opaque to the kernel: the daemon defines the
			 * numbering and must leave room for the two dots the VFS
			 * emits itself. Position p means "next entry is child
			 * p-2", so a child at index i resumes at i+3.
			 */
			first = (long long)h->offset - 2;
			if (first < 0)
				first = 0;

			for (int i = 0; i < NNODES; i++) {
				struct dsfs_dirent *de;
				unsigned int namelen, entry;

				if (tree[i].parent != h->nodeid)
					continue;
				if (idx++ < first)
					continue;
				namelen = strlen(tree[i].name);
				entry = DSFS_DIRENT_SIZE(namelen);
				if (used + entry > sizeof(payload) || used + entry > h->size)
					break;
				de = (void *)(payload + used);
				memset(de, 0, entry);
				de->nodeid = tree[i].nodeid;
				de->off = idx + 2;	/* resume after this entry */
				de->namelen = namelen;
				de->type = S_ISDIR(tree[i].mode) ? DT_DIR : DT_REG;
				memcpy(de + 1, tree[i].name, namelen);
				used += entry;
			}
			reply(fd, h->unique, 0, payload, used);
			break;
		}
		case DSFS_OP_READ: {
			struct node *n2 = find_node(h->nodeid);
			unsigned long long len, size;

			if (!n2 || !n2->data) {
				reply(fd, h->unique, 0, NULL, 0);
				break;
			}
			size = strlen(n2->data);
			if (h->offset >= size) {
				reply(fd, h->unique, 0, NULL, 0);
				break;
			}
			len = size - h->offset;
			if (len > h->size)
				len = h->size;
			reply(fd, h->unique, 0, n2->data + h->offset, (unsigned int)len);
			break;
		}
		default:
			reply(fd, h->unique, -ENOSYS, NULL, 0);
			break;
		}
	}
}
