/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * AlloyFS — a filesystem that can report changes it did not make.
 *
 * One mount mode: `mount -t alloyfs -o fd=N none /mnt`, where N is a
 * descriptor for an open /dev/alloyfs shared with the daemon serving the
 * export. The tree, the file contents and the change notifications all arrive
 * over that connection; a mount without one is refused rather than served
 * from anything local.
 *
 * The fsnotify injection path — the reason the module exists — is in
 * alloyfs_notify.c.
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

/* ------------------------------------------------------- VFS API shims
 *
 * Three things this module uses moved between 6.14, which it was written
 * against, and 7.0, which Ubuntu 26.04 ships. Kbuild probes the header the
 * build is actually running against and defines the macros below; see the
 * reasoning there for why that beats a LINUX_VERSION_CODE comparison.
 *
 * Each shim spells the OLD form by default, so a build whose probe did not
 * run still compiles exactly as it always did.
 */

/* `i_state` became `struct inode_state_flags`, read through accessors. */
#ifdef ALLOYFS_HAVE_INODE_STATE_ACCESSORS
#define alloyfs_inode_state(inode) inode_state_read(inode)
#else
#define alloyfs_inode_state(inode) ((inode)->i_state)
#endif

/* `generic_delete_inode` was renamed `inode_just_drop`. Same behaviour: drop
 * the inode on last put rather than keeping it cached, which is what a
 * filesystem with no local backing store wants. */
#ifdef ALLOYFS_HAVE_INODE_JUST_DROP
#define ALLOYFS_DROP_INODE inode_just_drop
#else
#define ALLOYFS_DROP_INODE generic_delete_inode
#endif

#define ALLOYFS_MAGIC 0x64736673	/* "dsfs" — the pre-rename value, frozen: it is
					 * baked into every mounted superblock and
					 * changing it would strand existing mounts. */

/* --------------------------------------------------------------- the mount */

/* The mounted superblock. One mount at a time, which is all the harness needs
 * and all stage 2 promises.
 *
 * alloyfs_lock guards that pointer: notifications arrive on the daemon's own
 * thread and must not reach a superblock that put_super is halfway through
 * tearing down.
 */
extern struct mutex alloyfs_lock;
extern struct super_block *alloyfs_sb;

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

/* Per-superblock state. A live mount always has its connection; NULL means
 * the superblock is on its way out.
 */
struct alloyfs_sb_info {
	struct alloyfs_conn *conn;
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

struct inode *alloyfs_iget_attr(struct super_block *sb, const struct alloyfs_attr *attr);

/* Upper bound on a notification payload: the fixed entry plus two names. */
#define ALLOYFS_NOTIFY_MAX (sizeof(struct alloyfs_notify_entry) + 2 * ALLOYFS_MAX_NAME)

/* alloyfs_notify.c — the point of the whole exercise. */
int alloyfs_notify_from_daemon(int code, const void *payload, u32 len);

#endif /* _ALLOYFS_H */
