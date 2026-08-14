/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * ds-fs — a filesystem that can report changes it did not make.
 *
 * Two mount modes share one implementation:
 *   - `mount -t dsfs none /mnt`         a hardcoded in-memory tree. This is
 *                                       the stage-1 spike, kept because its
 *                                       32 inotify assertions are the
 *                                       project's regression test.
 *   - `mount -t dsfs -o fd=N none /mnt` backed by a daemon on /dev/ds-fs.
 *
 * The fsnotify injection path (dsfs_notify.c) is identical in both.
 */
#ifndef _DSFS_H
#define _DSFS_H

#include <linux/completion.h>
#include <linux/fs.h>
#include <linux/list.h>
#include <linux/mutex.h>
#include <linux/refcount.h>
#include <linux/spinlock.h>
#include <linux/wait.h>

#include "uapi/ds_fs.h"

#define DSFS_MAGIC 0x64736673	/* "dsfs" */

/* ------------------------------------------------------- in-memory tree */

/*
 * One entry of the stage-1 tree. Guarded by dsfs_lock (a single global mutex
 * is right for a demo tree; daemon-backed mounts never touch these).
 */
struct dsfs_node {
	struct list_head sibling;
	struct list_head children;
	struct dsfs_node *parent;
	char name[NAME_MAX + 1];
	umode_t mode;
	unsigned long ino;
	loff_t size;
	char *data;		/* file contents; NULL means zero-filled */
};

extern struct mutex dsfs_lock;

/* The mounted superblock. One mount at a time, which is all the harness
 * needs and all stage 2 promises.
 */
extern struct super_block *dsfs_sb;

struct dsfs_node *dsfs_node_new(struct dsfs_node *parent, const char *name, umode_t mode);
void dsfs_node_free(struct dsfs_node *node);
struct dsfs_node *dsfs_child(struct dsfs_node *dir, const char *name);

/* --------------------------------------------------------- daemon transport */

struct dsfs_req {
	struct list_head list;
	struct dsfs_in_header hdr;
	/* Allocated per request: a name is 255 bytes, but a write carries up
	 * to DSFS_MAX_PAYLOAD, so this cannot be a fixed inline array.
	 */
	void *in_buf;
	u32 in_len;
	void *out_buf;
	u32 out_max;
	u32 out_len;
	int error;
	bool finished;
	struct completion done;
};

struct dsfs_conn {
	spinlock_t lock;
	struct list_head pending;	/* queued, not yet read by the daemon */
	struct list_head processing;	/* read, awaiting a response */
	wait_queue_head_t waitq;
	u64 next_unique;
	bool connected;
	refcount_t refs;
};

/* Per-superblock state; NULL conn means the in-memory mode. */
struct dsfs_sb_info {
	struct dsfs_conn *conn;
	struct dsfs_node *root_node;
};

static inline struct dsfs_sb_info *DSFS_SB(struct super_block *sb)
{
	return sb->s_fs_info;
}

int dsfs_conn_init(void);
void dsfs_conn_exit(void);
struct dsfs_conn *dsfs_conn_from_fd(unsigned int fd);
struct dsfs_conn *dsfs_conn_get(struct dsfs_conn *conn);
void dsfs_conn_put(struct dsfs_conn *conn);
void dsfs_conn_shutdown(struct dsfs_conn *conn);
int dsfs_request(struct dsfs_conn *conn, u32 opcode, u64 nodeid, u64 offset,
		 u32 size, const void *in_payload, u32 in_len,
		 void *out_buf, u32 out_max);

/* ------------------------------------------------------------------ inodes */

struct inode *dsfs_iget_node(struct super_block *sb, struct dsfs_node *node);
struct inode *dsfs_iget_attr(struct super_block *sb, const struct dsfs_attr *attr);

/* Upper bound on a notification payload: the fixed entry plus two names. */
#define DSFS_NOTIFY_MAX (sizeof(struct dsfs_notify_entry) + 2 * DSFS_MAX_NAME)

/* dsfs_notify.c — the point of the whole exercise. */
int dsfs_inject(const char *line);
int dsfs_notify_from_daemon(int code, const void *payload, u32 len);
int dsfs_notify_init(void);
void dsfs_notify_exit(void);

#endif /* _DSFS_H */
