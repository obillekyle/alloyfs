// SPDX-License-Identifier: GPL-2.0-only
/*
 * dsd — a minimal ds-fs daemon for the test harness.
 *
 * Serves a small mutable tree over /dev/ds-fs so the kernel module can be
 * tested without the real Rust client (that arrives in stage 6). A control
 * FIFO stands in for the network: writing a line to it mutates the tree and
 * sends the matching notification, which is exactly the shape of the real
 * thing (remote event arrives → tell the kernel → watchers see it).
 *
 *   dsd [--fd N] [--ctl PATH] [--die-after N] [--hang-on OP] [--corrupt-len]
 *
 * With --fd it serves a descriptor it inherited, which is how the harness
 * gets the daemon and `mount -o fd=N` onto the SAME connection: the kernel
 * resolves fd= in the mounting process's table, so the two must share one
 * open file description.
 *
 * Control lines (parent/dir are names, resolved to nodeids):
 *   create <dir> <name>          mkdir  <dir> <name>
 *   modify <dir> <name> <text>   attrib <dir> <name>
 *   delete <dir> <name>          rename <dir> <name> <dir2> <name2>
 *
 * Build: gcc -static -Os -o dsd dsd.c
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "../../ds-fs/uapi/ds_fs.h"

#define DT_DIR 4
#define DT_REG 8
#define MAX_NODES 64
#define MAX_DATA 256

struct node {
	int alive;
	unsigned long long nodeid;
	unsigned long long parent;
	char name[DSFS_MAX_NAME + 1];
	unsigned int mode;
	char data[MAX_DATA];
};

static struct node tree[MAX_NODES];
static unsigned long long next_nodeid = 5;

static long die_after = -1;
static const char *hang_on;
static int corrupt_len;

static void add(unsigned long long nodeid, unsigned long long parent,
		const char *name, unsigned int mode, const char *data)
{
	for (int i = 0; i < MAX_NODES; i++) {
		if (tree[i].alive)
			continue;
		tree[i].alive = 1;
		tree[i].nodeid = nodeid;
		tree[i].parent = parent;
		snprintf(tree[i].name, sizeof(tree[i].name), "%s", name);
		tree[i].mode = mode;
		snprintf(tree[i].data, sizeof(tree[i].data), "%s", data ? data : "");
		return;
	}
}

static void seed_tree(void)
{
	add(1, 0, "/",       S_IFDIR | 0755, NULL);
	add(2, 1, "one.txt", S_IFREG | 0644, "first file");
	add(3, 1, "dir",     S_IFDIR | 0755, NULL);
	add(4, 3, "two.txt", S_IFREG | 0644, "second file, in a subdirectory");
}

static struct node *find_node(unsigned long long nodeid)
{
	for (int i = 0; i < MAX_NODES; i++)
		if (tree[i].alive && tree[i].nodeid == nodeid)
			return &tree[i];
	return NULL;
}

static struct node *find_child(unsigned long long parent, const char *name, int len)
{
	for (int i = 0; i < MAX_NODES; i++) {
		if (tree[i].alive && tree[i].parent == parent &&
		    (int)strlen(tree[i].name) == len && !memcmp(tree[i].name, name, len)) {
			return &tree[i];
		}
	}
	return NULL;
}

/* Control lines name directories, not nodeids: "/" or a child of the root. */
static unsigned long long dir_nodeid(const char *name)
{
	struct node *n;

	if (!name || !*name || !strcmp(name, "/"))
		return 1;
	n = find_child(1, name, strlen(name));
	return n ? n->nodeid : 0;
}

static void fill_attr(struct dsfs_attr *a, const struct node *n)
{
	memset(a, 0, sizeof(*a));
	a->nodeid = n->nodeid;
	a->mode = n->mode;
	a->nlink = S_ISDIR(n->mode) ? 2 : 1;
	a->size = strlen(n->data);
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

/* Unsolicited notification: unique == 0, `error` carries the code. */
static int notify(int fd, int code, unsigned long long parent, const char *name,
		  unsigned long long parent2, const char *name2,
		  unsigned long long size, int isdir)
{
	char buf[sizeof(struct dsfs_out_header) + sizeof(struct dsfs_notify_entry) +
		 2 * (DSFS_MAX_NAME + 1)];
	struct dsfs_out_header *h = (void *)buf;
	struct dsfs_notify_entry *e = (void *)(buf + sizeof(*h));
	char *p = (char *)(e + 1);
	unsigned int nl = name ? strlen(name) : 0;
	unsigned int nl2 = name2 ? strlen(name2) : 0;

	memset(e, 0, sizeof(*e));
	e->parent = parent;
	e->parent2 = parent2;
	e->size = size;
	e->flags = isdir ? DSFS_NOTIFY_F_ISDIR : 0;
	e->namelen = nl;
	e->name2len = nl2;
	if (nl)
		memcpy(p, name, nl);
	if (nl2)
		memcpy(p + nl, name2, nl2);

	h->len = sizeof(*h) + sizeof(*e) + nl + nl2;
	h->error = code;
	h->unique = 0;
	if (write(fd, buf, h->len) < 0) {
		fprintf(stderr, "dsd: notify: %s\n", strerror(errno));
		return -1;
	}
	return 0;
}

/* Apply one control line: mutate the tree, then tell the kernel. */
static void handle_ctl(int fd, char *line)
{
	char *cmd = strtok(line, " \t\n");
	char *a1 = strtok(NULL, " \t\n");
	char *a2 = strtok(NULL, " \t\n");
	char *a3 = strtok(NULL, " \t\n");
	char *a4 = strtok(NULL, " \t\n");
	unsigned long long parent, parent2;
	struct node *n;

	if (!cmd || !a1 || !a2)
		return;
	parent = dir_nodeid(a1);
	if (!parent)
		return;

	if (!strcmp(cmd, "create") || !strcmp(cmd, "mkdir")) {
		int isdir = cmd[0] == 'm';

		add(next_nodeid++, parent, a2,
		    isdir ? (S_IFDIR | 0755) : (S_IFREG | 0644), a3 ? a3 : "");
		notify(fd, DSFS_NOTIFY_CREATE, parent, a2, 0, NULL, 0, isdir);
	} else if (!strcmp(cmd, "modify")) {
		n = find_child(parent, a2, strlen(a2));
		if (!n)
			return;
		snprintf(n->data, sizeof(n->data), "%s", a3 ? a3 : "");
		notify(fd, DSFS_NOTIFY_MODIFY, parent, a2, 0, NULL, strlen(n->data), 0);
	} else if (!strcmp(cmd, "attrib")) {
		notify(fd, DSFS_NOTIFY_ATTRIB, parent, a2, 0, NULL, 0, 0);
	} else if (!strcmp(cmd, "delete")) {
		n = find_child(parent, a2, strlen(a2));
		if (!n)
			return;
		n->alive = 0;
		notify(fd, DSFS_NOTIFY_DELETE, parent, a2, 0, NULL, 0,
		       S_ISDIR(n->mode));
	} else if (!strcmp(cmd, "rename") && a3 && a4) {
		int isdir;

		parent2 = dir_nodeid(a3);
		n = find_child(parent, a2, strlen(a2));
		if (!n || !parent2)
			return;
		isdir = S_ISDIR(n->mode);
		n->parent = parent2;
		snprintf(n->name, sizeof(n->name), "%s", a4);
		notify(fd, DSFS_NOTIFY_RENAME, parent, a2, parent2, a4, 0, isdir);
	}
}

static void serve_request(int fd, char *req, ssize_t n, int *served)
{
	struct dsfs_in_header *h = (void *)req;
	const char *name = req + sizeof(*h);

	if (n < (ssize_t)sizeof(*h))
		return;

	if (die_after >= 0 && *served >= die_after) {
		fprintf(stderr, "dsd: dying after %d requests\n", *served);
		_exit(0);	/* no close(): exercise abrupt death */
	}
	(*served)++;

	switch (h->opcode) {
	case DSFS_OP_LOOKUP: {
		struct node *n2;
		struct dsfs_attr a;
		int namelen = (int)(h->len - sizeof(*h));

		if (hang_on && !strcmp(hang_on, "lookup"))
			return;		/* never answer: the kernel must not wedge */
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
		 * numbering and must leave room for the two dots the VFS emits
		 * itself. Position p means "next entry is child p-2", so a
		 * child at index i resumes at i+2 (one past itself, plus dots).
		 */
		first = (long long)h->offset - 2;
		if (first < 0)
			first = 0;

		for (int i = 0; i < MAX_NODES; i++) {
			struct dsfs_dirent *de;
			unsigned int namelen, entry;

			if (!tree[i].alive || tree[i].parent != h->nodeid)
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

		if (!n2) {
			reply(fd, h->unique, -ENOENT, NULL, 0);
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
	/* ------------------------------------------------ stage 4: mutations */
	case DSFS_OP_CREATE:
	case DSFS_OP_MKDIR: {
		const struct dsfs_create_in *ci = (const void *)name;
		const char *nm = (const char *)(ci + 1);
		int namelen = (int)(h->len - sizeof(*h) - sizeof(*ci));
		char tmp[DSFS_MAX_NAME + 1];
		struct dsfs_attr a;
		struct node *n2;

		if (namelen <= 0 || namelen > DSFS_MAX_NAME) {
			reply(fd, h->unique, -EINVAL, NULL, 0);
			break;
		}
		memcpy(tmp, nm, namelen);
		tmp[namelen] = '\0';
		if (find_child(h->nodeid, tmp, namelen)) {
			reply(fd, h->unique, -EEXIST, NULL, 0);
			break;
		}
		add(next_nodeid, h->nodeid, tmp, ci->mode, "");
		n2 = find_node(next_nodeid++);
		if (!n2) {
			reply(fd, h->unique, -ENOSPC, NULL, 0);
			break;
		}
		fill_attr(&a, n2);
		reply(fd, h->unique, 0, &a, sizeof(a));
		break;
	}
	case DSFS_OP_UNLINK:
	case DSFS_OP_RMDIR: {
		int namelen = (int)(h->len - sizeof(*h));
		struct node *n2 = find_child(h->nodeid, name, namelen);

		if (!n2) {
			reply(fd, h->unique, -ENOENT, NULL, 0);
			break;
		}
		if (h->opcode == DSFS_OP_RMDIR) {
			for (int i = 0; i < MAX_NODES; i++) {
				if (tree[i].alive && tree[i].parent == n2->nodeid) {
					reply(fd, h->unique, -ENOTEMPTY, NULL, 0);
					goto done;
				}
			}
		}
		n2->alive = 0;
		reply(fd, h->unique, 0, NULL, 0);
done:
		break;
	}
	case DSFS_OP_RENAME: {
		const struct dsfs_rename_in *ri = (const void *)name;
		const char *from = (const char *)(ri + 1);
		const char *to = from + ri->namelen;
		struct node *n2, *victim;
		char tmp[DSFS_MAX_NAME + 1];

		if (ri->namelen > DSFS_MAX_NAME || ri->newnamelen > DSFS_MAX_NAME) {
			reply(fd, h->unique, -EINVAL, NULL, 0);
			break;
		}
		n2 = find_child(h->nodeid, from, ri->namelen);
		if (!n2) {
			reply(fd, h->unique, -ENOENT, NULL, 0);
			break;
		}
		victim = find_child(ri->newparent, to, ri->newnamelen);
		if (victim)
			victim->alive = 0;	/* replace, POSIX-style */
		memcpy(tmp, to, ri->newnamelen);
		tmp[ri->newnamelen] = '\0';
		n2->parent = ri->newparent;
		snprintf(n2->name, sizeof(n2->name), "%s", tmp);
		reply(fd, h->unique, 0, NULL, 0);
		break;
	}
	case DSFS_OP_WRITE: {
		struct node *n2 = find_node(h->nodeid);
		const char *data = name;
		unsigned int len = h->len - sizeof(*h);
		struct dsfs_write_out wo;
		unsigned long long end;

		if (!n2) {
			reply(fd, h->unique, -ENOENT, NULL, 0);
			break;
		}
		end = h->offset + len;
		if (end >= MAX_DATA) {
			reply(fd, h->unique, -EFBIG, NULL, 0);
			break;
		}
		/* Sparse writes past the end zero-fill, like a real file. */
		if (h->offset > strlen(n2->data))
			memset(n2->data + strlen(n2->data), '\0',
			       h->offset - strlen(n2->data));
		memcpy(n2->data + h->offset, data, len);
		n2->data[end] = '\0';
		memset(&wo, 0, sizeof(wo));
		wo.written = len;
		reply(fd, h->unique, 0, &wo, sizeof(wo));
		break;
	}
	case DSFS_OP_SETATTR: {
		const struct dsfs_setattr_in *si = (const void *)name;
		struct node *n2 = find_node(h->nodeid);
		struct dsfs_attr a;

		if (!n2) {
			reply(fd, h->unique, -ENOENT, NULL, 0);
			break;
		}
		if (si->valid & DSFS_SETATTR_MODE)
			n2->mode = (n2->mode & S_IFMT) | (si->mode & 07777);
		if (si->valid & DSFS_SETATTR_SIZE) {
			unsigned long long want = si->size;

			if (want >= MAX_DATA)
				want = MAX_DATA - 1;
			if (want < strlen(n2->data))
				n2->data[want] = '\0';
			else
				memset(n2->data + strlen(n2->data), '\0',
				       want - strlen(n2->data));
			n2->data[want] = '\0';
		}
		fill_attr(&a, n2);
		reply(fd, h->unique, 0, &a, sizeof(a));
		break;
	}
	default:
		reply(fd, h->unique, -ENOSYS, NULL, 0);
		break;
	}
}

int main(int argc, char **argv)
{
	int fd = -1, ctl = -1, served = 0;
	const char *ctl_path = NULL;
	/* Must hold ANY request: a write carries a full payload, not a name. */
	static char req[sizeof(struct dsfs_in_header) + DSFS_MAX_PAYLOAD];

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--fd") && i + 1 < argc)
			fd = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--ctl") && i + 1 < argc)
			ctl_path = argv[++i];
		else if (!strcmp(argv[i], "--die-after") && i + 1 < argc)
			die_after = atol(argv[++i]);
		else if (!strcmp(argv[i], "--hang-on") && i + 1 < argc)
			hang_on = argv[++i];
		else if (!strcmp(argv[i], "--corrupt-len"))
			corrupt_len = 1;
	}

	seed_tree();

	if (fd < 0) {
		fd = open("/dev/ds-fs", O_RDWR);
		if (fd < 0) {
			fprintf(stderr, "open /dev/ds-fs: %s\n", strerror(errno));
			return 1;
		}
	}
	if (ctl_path) {
		/* O_RDWR so the FIFO never reports EOF when a writer closes. */
		ctl = open(ctl_path, O_RDWR | O_NONBLOCK);
		if (ctl < 0) {
			fprintf(stderr, "open %s: %s\n", ctl_path, strerror(errno));
			return 1;
		}
	}
	printf("DSD-READY %d\n", fd);
	fflush(stdout);

	for (;;) {
		struct pollfd p[2] = {
			{ .fd = fd,  .events = POLLIN },
			{ .fd = ctl, .events = POLLIN },
		};
		int nfds = ctl >= 0 ? 2 : 1;

		if (poll(p, nfds, -1) < 0) {
			if (errno == EINTR)
				continue;
			return 1;
		}
		if (p[0].revents & POLLIN) {
			ssize_t n = read(fd, req, sizeof(req));

			if (n < 0) {
				if (errno == EINTR)
					continue;
				fprintf(stderr, "read: %s\n", strerror(errno));
				return errno == ENODEV ? 0 : 1;
			}
			serve_request(fd, req, n, &served);
		}
		if (ctl >= 0 && (p[1].revents & POLLIN)) {
			char line[512];
			ssize_t n = read(ctl, line, sizeof(line) - 1);
			char *s, *nl;

			if (n <= 0)
				continue;
			line[n] = '\0';
			for (s = line; (nl = strchr(s, '\n')); s = nl + 1) {
				*nl = '\0';
				if (*s)
					handle_ctl(fd, s);
			}
			if (*s)
				handle_ctl(fd, s);
		}
	}
}
