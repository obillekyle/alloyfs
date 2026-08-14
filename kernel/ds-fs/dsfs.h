/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * ds-fs — a filesystem that can report changes it did not make.
 *
 * Stage 1 (the spike): the tree is held in memory and mutated through
 * /proc/dsfs-inject instead of arriving from a daemon over a char device.
 * Everything below the injection trigger — path resolution, tree mutation,
 * cache invalidation, and the fsnotify calls — is the code the real driver
 * will use unchanged.
 */
#ifndef _DSFS_H
#define _DSFS_H

#include <linux/fs.h>
#include <linux/list.h>
#include <linux/mutex.h>

#define DSFS_MAGIC 0x64736673	/* "dsfs" */

/*
 * One entry in the in-memory tree. Guarded by dsfs_lock (a single global
 * mutex is right for a spike; the real driver will move to per-directory
 * locking with the VFS's i_rwsem).
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

/* The mounted superblock. Stage 1 supports exactly one mount. */
extern struct super_block *dsfs_sb;

struct dsfs_node *dsfs_node_new(struct dsfs_node *parent, const char *name, umode_t mode);
void dsfs_node_free(struct dsfs_node *node);
struct dsfs_node *dsfs_child(struct dsfs_node *dir, const char *name);
struct inode *dsfs_iget(struct super_block *sb, struct dsfs_node *node);

/* dsfs_notify.c — the point of the whole exercise. */
int dsfs_inject(const char *line);
int dsfs_notify_init(void);
void dsfs_notify_exit(void);

#endif /* _DSFS_H */
