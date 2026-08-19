// SPDX-License-Identifier: GPL-2.0-only
/*
 * The whole point of AlloyFS: turning "something changed on the server" into
 * genuine fsnotify events, which no FUSE/NFS/CIFS mount can do.
 *
 * TWO INVARIANTS, both verified against the v6.14 source. Break either and
 * the box panics the moment somebody actually watches the mount:
 *
 *  1. fsnotify() derives the superblock FROM ITS `data` ARGUMENT:
 *         sb = fsnotify_data_sb(data, data_type);
 *         ... marks_mask = READ_ONCE(sb->s_fsnotify_mask);
 *     With FSNOTIFY_EVENT_NONE that sb is NULL. The early return above it
 *     only fires when nothing has marks — i.e. exactly when nobody is
 *     listening. So a data-less event is harmless in an unwatched test and
 *     a NULL deref in the one case we care about. EVERY event below carries
 *     a live dentry or inode.
 *
 *  2. FS_RENAME dereferences moved->d_parent->d_inode and is only legal with
 *     a real dentry. We never emit it: inotify wants FS_MOVED_FROM and
 *     FS_MOVED_TO sharing one cookie, which is what we send.
 *
 * Resolution never blocks and never calls ->d_revalidate: we walk our own
 * dentry cache with d_hash_and_lookup(). An uncached path is skipped —
 * nothing has walked that subtree, so nothing can be watching it.
 *
 * Layout: the primitives in the middle of this file are the shared core, and
 * the daemon front-end below them addresses events by nodeid, the same way it
 * addresses everything else.
 */
#define pr_fmt(fmt) "alloyfs: " fmt

#include <linux/dcache.h>
#include <linux/fs.h>
#include <linux/fsnotify.h>
#include <linux/pagemap.h>	/* invalidate_inode_pages2 */
#include <linux/string.h>

#include "alloyfs.h"

/*
 * Emit a directory-entry event (create/delete/move) on `dir`.
 *
 * The `inode` argument stays NULL — this is a DIRECTORY event carrying a
 * name, and that is exactly what the kernel's own fsnotify_name() passes.
 * Handing it the child inode instead reroutes the event to the child's own
 * marks: the parent's watch then never sees the DELETE, and MOVED_FROM
 * disappears entirely (inotify only reports moves on directory watches).
 * Found by the differential test against tmpfs.
 *
 * `data` still carries the child dentry when we have one, which is what
 * supplies the superblock (invariant 1) and lets child-self watches fire;
 * with no dentry the directory inode plays that role.
 */
static void alloyfs_notify_dirent(struct inode *dir, struct dentry *child,
			       const struct qstr *name, __u32 mask, u32 cookie)
{
	if (child && d_really_is_positive(child)) {
		fsnotify(mask, child, FSNOTIFY_EVENT_DENTRY, dir, name, NULL, cookie);
		return;
	}
	fsnotify(mask, dir, FSNOTIFY_EVENT_INODE, dir, name, NULL, cookie);
}

/* The cached child dentry, positive or negative, or NULL. Caller dputs.
 * (the lookup takes a non-const qstr; it does not modify it.)
 *
 * `d_hash_and_lookup` stopped being declared for modules after 6.14. With no
 * custom `->d_hash` — and this filesystem installs no dentry_operations at
 * all — it was exactly "hash the name, then d_lookup", so the replacement
 * spells out what it had been doing rather than substituting something else.
 */
static struct dentry *alloyfs_cached_child(struct dentry *dir, const struct qstr *name)
{
	struct qstr q = *name;
	struct dentry *child;

#ifdef ALLOYFS_HAVE_D_HASH_AND_LOOKUP
	child = d_hash_and_lookup(dir, &q);
#else
	q.hash = full_name_hash(dir, q.name, q.len);
	child = d_lookup(dir, &q);
#endif
	return IS_ERR(child) ? NULL : child;
}

/* ------------------------------------------------------------- primitives
 *
 * Each takes a resolved parent DENTRY and a name. They touch caches first
 * and notify second, so a watcher that reacts by stat()ing sees the new
 * state rather than the old one.
 */

/*
 * A name appeared on the server.
 *
 * A negative dentry may be cached from a lookup that ran before the name
 * existed, and nothing here revalidates one: this filesystem has no
 * ->d_revalidate, so the VFS would go on answering ENOENT for a file that is
 * now really there. Unhashing it forces the next lookup to ask the daemon.
 *
 * The notification cannot instantiate the name itself — a CREATE carries the
 * parent's nodeid and a name, not the child's attributes — so dropping is the
 * whole of the fix, and it has to happen before the watchers are woken or a
 * watcher that reacts by opening the file races the stale answer.
 */
static void alloyfs_ev_created(struct dentry *dir_dentry, const struct qstr *q,
			    bool isdir)
{
	struct inode *dir = d_inode(dir_dentry);
	struct dentry *child = alloyfs_cached_child(dir_dentry, q);

	if (child && d_really_is_negative(child))
		d_drop(child);

	alloyfs_notify_dirent(dir, child, q, FS_CREATE | (isdir ? FS_ISDIR : 0), 0);
	if (child)
		dput(child);
}

/* Contents or metadata changed. `newsize` >= 0 updates i_size first. */
static void alloyfs_ev_changed(struct dentry *dir_dentry, const struct qstr *q,
			    __u32 mask, loff_t newsize)
{
	struct inode *dir = d_inode(dir_dentry);
	struct dentry *child = alloyfs_cached_child(dir_dentry, q);

	if (child && d_really_is_positive(child)) {
		struct inode *inode = d_inode(child);

		if (newsize >= 0)
			i_size_write(inode, newsize);
		inode_set_mtime_to_ts(inode, current_time(inode));
		if (mask & FS_MODIFY)
			invalidate_inode_pages2(inode->i_mapping);

		/* fsnotify_dentry() reaches both the parent's watch (with the
		 * name) and a watch on the file itself.
		 */
		fsnotify_dentry(child, mask);
		dput(child);
		return;
	}
	alloyfs_notify_dirent(dir, NULL, q, mask, 0);
}

/* A name went away. */
static void alloyfs_ev_removed(struct dentry *dir_dentry, const struct qstr *q, bool isdir)
{
	struct inode *dir = d_inode(dir_dentry);
	struct dentry *child = alloyfs_cached_child(dir_dentry, q);

	/* Report while we still hold the dentry, then tear the entry down. */
	alloyfs_notify_dirent(dir, child, q, FS_DELETE | (isdir ? FS_ISDIR : 0), 0);

	if (child) {
		if (d_really_is_positive(child)) {
			struct inode *inode = d_inode(child);

			/* DELETE_SELF + IGNORED for watches on the file. */
			clear_nlink(inode);
			fsnotify_inoderemove(inode);
		}
		d_delete(child);
		dput(child);
	}
}

/* A name moved. ONE cookie for the pair — that is what makes a rename a
 * rename to every watcher instead of an unrelated delete plus create.
 */
static void alloyfs_ev_moved(struct dentry *from_dir_dentry, const struct qstr *fq,
			  struct dentry *to_dir_dentry, const struct qstr *tq,
			  bool isdir)
{
	struct inode *from_dir = d_inode(from_dir_dentry);
	struct inode *to_dir = d_inode(to_dir_dentry);
	struct dentry *from_child = alloyfs_cached_child(from_dir_dentry, fq);
	struct dentry *to_child = alloyfs_cached_child(to_dir_dentry, tq);
	u32 cookie = fsnotify_get_cookie();
	__u32 dirbit = isdir ? FS_ISDIR : 0;

	alloyfs_notify_dirent(from_dir, from_child, fq, FS_MOVED_FROM | dirbit, cookie);
	alloyfs_notify_dirent(to_dir, to_child, tq, FS_MOVED_TO | dirbit, cookie);

	/* Move the dentry so the new name resolves without a fresh lookup. */
	if (from_child && d_really_is_positive(from_child)) {
		if (to_child && d_really_is_negative(to_child))
			d_move(from_child, to_child);
		else
			d_drop(from_child);
	}
	if (to_child)
		dput(to_child);
	if (from_child)
		dput(from_child);
}

/* ------------------------------------------------- front-end: daemon (nodeid)
 *
 * The daemon addresses events the way it addresses everything else: by the
 * parent's nodeid. ilookup() finds the inode only if it is cached, and
 * d_find_alias() gives its dentry — a directory has at most one, since
 * directories cannot be hard-linked. Neither can block or upcall.
 */
static struct dentry *alloyfs_dentry_of_nodeid(struct super_block *sb, u64 nodeid)
{
	struct inode *inode = ilookup(sb, (unsigned long)nodeid);
	struct dentry *dentry;

	if (!inode)
		return NULL;	/* never looked up ⇒ nothing is watching it */
	if (!S_ISDIR(inode->i_mode)) {
		iput(inode);
		return NULL;
	}
	dentry = d_find_alias(inode);
	iput(inode);
	return dentry;
}

static bool alloyfs_name_ok(const char *name, u16 len)
{
	if (len == 0 || len > ALLOYFS_MAX_NAME)
		return false;
	if (memchr(name, '/', len) || memchr(name, '\0', len))
		return false;
	if (len == 1 && name[0] == '.')
		return false;
	if (len == 2 && name[0] == '.' && name[1] == '.')
		return false;
	return true;
}

/*
 * Apply one daemon notification. Everything here is userspace-supplied and
 * is validated before use; an unresolvable target is skipped, never fatal.
 */
int alloyfs_notify_from_daemon(int code, const void *payload, u32 len)
{
	const struct alloyfs_notify_entry *e = payload;
	const char *name, *name2;
	struct super_block *sb;
	struct dentry *dir = NULL, *dir2 = NULL;
	struct qstr q, q2;
	bool isdir;
	int ret = 0;

	if (len < sizeof(*e))
		return -EINVAL;
	if ((u64)e->namelen + e->name2len + sizeof(*e) > len)
		return -EINVAL;
	name = (const char *)(e + 1);
	name2 = name + e->namelen;
	if (!alloyfs_name_ok(name, e->namelen))
		return -EINVAL;
	if (code == ALLOYFS_NOTIFY_RENAME && !alloyfs_name_ok(name2, e->name2len))
		return -EINVAL;

	/* Validate the opcode HERE, with the other structural checks, rather
	 * than in the switch below: a malformed message is malformed whether
	 * or not anything happens to be mounted, and reporting ENODEV for an
	 * unknown code would make the error depend on unrelated state.
	 */
	switch (code) {
	case ALLOYFS_NOTIFY_CREATE:
	case ALLOYFS_NOTIFY_DELETE:
	case ALLOYFS_NOTIFY_MODIFY:
	case ALLOYFS_NOTIFY_ATTRIB:
	case ALLOYFS_NOTIFY_RENAME:
		break;
	default:
		return -EINVAL;
	}

	isdir = e->flags & ALLOYFS_NOTIFY_F_ISDIR;
	q = (struct qstr)QSTR_INIT(name, e->namelen);
	q2 = (struct qstr)QSTR_INIT(name2, e->name2len);

	/* alloyfs_lock keeps the superblock alive for the duration: put_super
	 * takes the same mutex before clearing alloyfs_sb.
	 */
	mutex_lock(&alloyfs_lock);
	sb = alloyfs_sb;
	if (!sb) {
		mutex_unlock(&alloyfs_lock);
		return -ENODEV;
	}
	dir = alloyfs_dentry_of_nodeid(sb, e->parent);
	if (!dir) {
		mutex_unlock(&alloyfs_lock);
		return 0;	/* not cached ⇒ nothing to tell anyone */
	}

	switch (code) {
	case ALLOYFS_NOTIFY_CREATE:
		alloyfs_ev_created(dir, &q, isdir);
		break;
	case ALLOYFS_NOTIFY_DELETE:
		alloyfs_ev_removed(dir, &q, isdir);
		break;
	case ALLOYFS_NOTIFY_MODIFY:
		alloyfs_ev_changed(dir, &q, FS_MODIFY, (loff_t)e->size);
		break;
	case ALLOYFS_NOTIFY_ATTRIB:
		alloyfs_ev_changed(dir, &q, FS_ATTRIB, -1);
		break;
	case ALLOYFS_NOTIFY_RENAME:
		dir2 = (e->parent2 == e->parent) ? dget(dir)
						 : alloyfs_dentry_of_nodeid(sb, e->parent2);
		if (!dir2) {
			ret = 0;	/* destination not cached */
			break;
		}
		alloyfs_ev_moved(dir, &q, dir2, &q2, isdir);
		dput(dir2);
		break;
	}

	dput(dir);
	mutex_unlock(&alloyfs_lock);
	return ret;
}
