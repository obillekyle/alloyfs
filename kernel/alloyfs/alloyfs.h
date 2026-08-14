/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * AlloyFS — a filesystem that can report changes it did not make.
 *
 * Two mount modes share one implementation:
 *   - `mount -t alloyfs none /mnt`         a hardcoded in-memory tree. This is
 *                                       the stage-1 spike, kept because its
 *                                       32 inotify assertions are the
 *                                       project's regression test.
 *   - `mount -t alloyfs -o fd=N none /mnt` backed by a daemon on /dev/alloyfs.
 *
 * The fsnotify injection path (alloyfs_notify.c) is identical in both.
 */
#ifndef _ALLOYFS_H
#define _ALLOYFS_H

#include <linux/completion.h>
#include <linux/fs.h>
#include <linux/list.h>
#include <linux/mutex.h>
#include <linux/refcount.h>
#include <linux/spinlock.h>
#include <linux/wait.h>

#include "uapi/alloyfs.h"

#define ALLOYFS_MAGIC 0x64736673	/* "alloyfs" */

/* ------------------------------------------------------- in-memory tree */

/*
 * One entry of the stage-1 tree. Guarded by alloyfs_lock (a single global mutex
 * is right for a demo tree; daemon-backed mounts never touch these).
 */
struct alloyfs_node {
	struct list_head sibling;
	struct list_head children;
	struct alloyfs_node *parent;
	char name[NAME_MAX + 1];
	umode_t mode;
	unsigned long ino;
	loff_t size;
	char *data;		/* file contents; NULL means zero-filled */
};

extern struct mutex alloyfs_lock;

/* The mounted superblock. One mount at a time, which is all the harness
 * needs and all stage 2 promises.
 */
extern struct super_block *alloyfs_sb;

struct alloyfs_node *alloyfs_node_new(struct alloyfs_node *parent, const char *name, umode_t mode);
void alloyfs_node_free(struct alloyfs_node *node);
struct alloyfs_node *alloyfs_child(struct alloyfs_node *dir, const char *name);

/* --------------------------------------------------------- daemon transport */

struct alloyfs_req {
	struct list_head list;
	struct alloyfs_in_header hdr;
	/* Allocated per request: a name is 255 bytes, but a write carries up
	 * to ALLOYFS_MAX_PAYLOAD, so this cannot be a fixed inline array.
	 */
	void *in_buf;
	u32 in_len;
	void *out_buf;
	u32 out_max;
	u32 out_len;
	int error;
	bool finished;
	/*
	 * Two owners at most: the sleeping caller, and whoever holds it on a
	 * queue (the daemon, once it has read the request). Refcounting is
	 * what lets a KILLED caller walk away immediately instead of waiting
	 * for a daemon that may never answer — see alloyfs_request().
	 */
	refcount_t refs;
	struct completion done;
};

struct alloyfs_conn {
	spinlock_t lock;
	struct list_head pending;	/* queued, not yet read by the daemon */
	struct list_head processing;	/* read, awaiting a response */
	wait_queue_head_t waitq;
	u64 next_unique;
	bool connected;
	refcount_t refs;
};

/* Per-superblock state; NULL conn means the in-memory mode. */
struct alloyfs_sb_info {
	struct alloyfs_conn *conn;
	struct alloyfs_node *root_node;
};

static inline struct alloyfs_sb_info *ALLOYFS_SB(struct super_block *sb)
{
	return sb->s_fs_info;
}

int alloyfs_conn_init(void);
void alloyfs_conn_exit(void);
struct alloyfs_conn *alloyfs_conn_from_fd(unsigned int fd);
struct alloyfs_conn *alloyfs_conn_get(struct alloyfs_conn *conn);
void alloyfs_conn_put(struct alloyfs_conn *conn);
void alloyfs_conn_shutdown(struct alloyfs_conn *conn);
int alloyfs_request(struct alloyfs_conn *conn, u32 opcode, u64 nodeid, u64 offset,
		 u32 size, const void *in_payload, u32 in_len,
		 void *out_buf, u32 out_max);

/* ------------------------------------------------------------------ inodes */

struct inode *alloyfs_iget_node(struct super_block *sb, struct alloyfs_node *node);
struct inode *alloyfs_iget_attr(struct super_block *sb, const struct alloyfs_attr *attr);

/* Upper bound on a notification payload: the fixed entry plus two names. */
#define ALLOYFS_NOTIFY_MAX (sizeof(struct alloyfs_notify_entry) + 2 * ALLOYFS_MAX_NAME)

/* alloyfs_notify.c — the point of the whole exercise. */
int alloyfs_inject(const char *line);
int alloyfs_notify_from_daemon(int code, const void *payload, u32 len);
int alloyfs_notify_init(void);
void alloyfs_notify_exit(void);

#endif /* _ALLOYFS_H */
