// SPDX-License-Identifier: GPL-2.0-only
/*
 * The whole point of ds-fs: turning "something changed on the server" into
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
 * Path resolution walks OUR OWN dentry cache with d_hash_and_lookup(), which
 * never calls ->d_revalidate and never blocks. An uncached path is skipped:
 * nothing has walked that subtree, so nothing can be watching it.
 */
#define pr_fmt(fmt) "dsfs: " fmt

#include <linux/dcache.h>
#include <linux/fs.h>
#include <linux/fsnotify.h>
#include <linux/namei.h>
#include <linux/proc_fs.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/uaccess.h>

#include "dsfs.h"

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
static void dsfs_notify_dirent(struct inode *dir, struct dentry *child,
			       const struct qstr *name, __u32 mask, u32 cookie)
{
	if (child && d_really_is_positive(child)) {
		fsnotify(mask, child, FSNOTIFY_EVENT_DENTRY, dir, name, NULL, cookie);
		return;
	}
	fsnotify(mask, dir, FSNOTIFY_EVENT_INODE, dir, name, NULL, cookie);
}

/* Resolve a mount-relative directory path in our own dcache. "" or "/" is
 * the root. Returns a dentry reference, or NULL when any component is not
 * cached (nobody has walked it, so nobody is watching it).
 */
static struct dentry *dsfs_resolve_dir(const char *path)
{
	struct dentry *cur, *next;
	char *buf, *p, *comp;

	if (!dsfs_sb || !dsfs_sb->s_root)
		return NULL;

	cur = dget(dsfs_sb->s_root);
	if (!path || !*path || !strcmp(path, "/"))
		return cur;

	buf = kstrdup(path, GFP_KERNEL);
	if (!buf) {
		dput(cur);
		return NULL;
	}
	p = buf;
	while ((comp = strsep(&p, "/"))) {
		struct qstr q;

		if (!*comp)
			continue;	/* tolerate "a//b" and trailing slashes */
		q.name = comp;
		q.len = strlen(comp);
		next = d_hash_and_lookup(cur, &q);
		dput(cur);
		if (IS_ERR_OR_NULL(next) || d_really_is_negative(next)) {
			if (!IS_ERR_OR_NULL(next))
				dput(next);
			kfree(buf);
			return NULL;
		}
		cur = next;
	}
	kfree(buf);
	return cur;
}

/* The cached child dentry, positive or negative, or NULL. Caller dputs.
 * (d_hash_and_lookup takes a non-const qstr; it does not modify it.)
 */
static struct dentry *dsfs_cached_child(struct dentry *dir, const struct qstr *name)
{
	struct qstr q = *name;
	struct dentry *child = d_hash_and_lookup(dir, &q);

	return IS_ERR(child) ? NULL : child;
}

/* ------------------------------------------------------------ operations */

static int dsfs_do_create(const char *dirpath, const char *name, bool isdir)
{
	struct dentry *dir_dentry, *child = NULL;
	struct dsfs_node *dnode, *node;
	struct inode *dir_inode, *inode;
	struct qstr q = QSTR_INIT(name, strlen(name));
	__u32 mask = FS_CREATE | (isdir ? FS_ISDIR : 0);

	dir_dentry = dsfs_resolve_dir(dirpath);
	if (!dir_dentry)
		return -ENOENT;
	dir_inode = d_inode(dir_dentry);
	dnode = dir_inode->i_private;

	mutex_lock(&dsfs_lock);
	if (dsfs_child(dnode, name)) {
		mutex_unlock(&dsfs_lock);
		dput(dir_dentry);
		return -EEXIST;
	}
	node = dsfs_node_new(dnode, name,
			     isdir ? (S_IFDIR | 0755) : (S_IFREG | 0644));
	mutex_unlock(&dsfs_lock);
	if (!node) {
		dput(dir_dentry);
		return -ENOMEM;
	}

	/* A negative dentry may be cached from an earlier failed lookup; the
	 * VFS would otherwise keep answering ENOENT for a file that now
	 * exists. Instantiate it so the name is immediately live, exactly as
	 * a local create would.
	 */
	child = dsfs_cached_child(dir_dentry, &q);
	if (child && d_really_is_negative(child)) {
		inode = dsfs_iget(dsfs_sb, node);
		if (!IS_ERR(inode))
			d_instantiate(child, inode);
	}

	dsfs_notify_dirent(dir_inode, child, &q, mask, 0);
	if (child)
		dput(child);
	dput(dir_dentry);
	return 0;
}

static int dsfs_do_modify(const char *dirpath, const char *name, loff_t newsize,
			  __u32 mask)
{
	struct dentry *dir_dentry, *child;
	struct dsfs_node *dnode, *node;
	struct inode *dir_inode;
	struct qstr q = QSTR_INIT(name, strlen(name));

	dir_dentry = dsfs_resolve_dir(dirpath);
	if (!dir_dentry)
		return -ENOENT;
	dir_inode = d_inode(dir_dentry);
	dnode = dir_inode->i_private;

	mutex_lock(&dsfs_lock);
	node = dsfs_child(dnode, name);
	if (!node) {
		mutex_unlock(&dsfs_lock);
		dput(dir_dentry);
		return -ENOENT;
	}
	if (mask & FS_MODIFY) {
		kfree(node->data);
		node->data = NULL;	/* size-only change; reads return zeroes */
		node->size = newsize;
	}
	mutex_unlock(&dsfs_lock);

	child = dsfs_cached_child(dir_dentry, &q);

	/* Caches honest BEFORE observers are woken: a watcher that stats the
	 * file on receipt must see the new size, not the old one.
	 */
	if (child && d_really_is_positive(child)) {
		struct inode *inode = d_inode(child);

		if (mask & FS_MODIFY)
			i_size_write(inode, newsize);
		inode_set_mtime_to_ts(inode, current_time(inode));

		/* fsnotify_dentry() reaches both the parent's watch (with the
		 * name) and a watch on the file itself.
		 */
		fsnotify_dentry(child, mask);
		dput(child);
	} else {
		dsfs_notify_dirent(dir_inode, NULL, &q, mask, 0);
	}
	dput(dir_dentry);
	return 0;
}

static int dsfs_do_delete(const char *dirpath, const char *name)
{
	struct dentry *dir_dentry, *child;
	struct dsfs_node *dnode, *node;
	struct inode *dir_inode;
	struct qstr q = QSTR_INIT(name, strlen(name));
	bool isdir;

	dir_dentry = dsfs_resolve_dir(dirpath);
	if (!dir_dentry)
		return -ENOENT;
	dir_inode = d_inode(dir_dentry);
	dnode = dir_inode->i_private;

	mutex_lock(&dsfs_lock);
	node = dsfs_child(dnode, name);
	if (!node) {
		mutex_unlock(&dsfs_lock);
		dput(dir_dentry);
		return -ENOENT;
	}
	isdir = S_ISDIR(node->mode);
	dsfs_node_free(node);
	mutex_unlock(&dsfs_lock);

	child = dsfs_cached_child(dir_dentry, &q);

	/* Order matters: report the removal while we still hold the dentry,
	 * then tear the cache entry down.
	 */
	dsfs_notify_dirent(dir_inode, child, &q,
			   FS_DELETE | (isdir ? FS_ISDIR : 0), 0);

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
	dput(dir_dentry);
	return 0;
}

static int dsfs_do_rename(const char *fromdir, const char *fromname,
			  const char *todir, const char *toname)
{
	struct dentry *from_dentry, *to_dentry, *from_child, *to_child;
	struct dsfs_node *fnode, *tnode, *node;
	struct inode *from_inode, *to_inode;
	struct qstr fq = QSTR_INIT(fromname, strlen(fromname));
	struct qstr tq = QSTR_INIT(toname, strlen(toname));
	u32 cookie;
	bool isdir;

	from_dentry = dsfs_resolve_dir(fromdir);
	if (!from_dentry)
		return -ENOENT;
	to_dentry = dsfs_resolve_dir(todir);
	if (!to_dentry) {
		dput(from_dentry);
		return -ENOENT;
	}
	from_inode = d_inode(from_dentry);
	to_inode = d_inode(to_dentry);
	fnode = from_inode->i_private;
	tnode = to_inode->i_private;

	mutex_lock(&dsfs_lock);
	node = dsfs_child(fnode, fromname);
	if (!node) {
		mutex_unlock(&dsfs_lock);
		dput(to_dentry);
		dput(from_dentry);
		return -ENOENT;
	}
	isdir = S_ISDIR(node->mode);
	list_del(&node->sibling);
	strscpy(node->name, toname, sizeof(node->name));
	node->parent = tnode;
	list_add_tail(&node->sibling, &tnode->children);
	mutex_unlock(&dsfs_lock);

	from_child = dsfs_cached_child(from_dentry, &fq);
	to_child = dsfs_cached_child(to_dentry, &tq);

	/* ONE cookie for the pair — this is what makes a rename a rename to
	 * every watcher instead of an unrelated delete plus create. Never
	 * FS_RENAME (invariant 2).
	 */
	cookie = fsnotify_get_cookie();
	dsfs_notify_dirent(from_inode, from_child, &fq,
			   FS_MOVED_FROM | (isdir ? FS_ISDIR : 0), cookie);
	dsfs_notify_dirent(to_inode, to_child, &tq,
			   FS_MOVED_TO | (isdir ? FS_ISDIR : 0), cookie);

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
	dput(to_dentry);
	dput(from_dentry);
	return 0;
}

/* ------------------------------------------------------- /proc/dsfs-inject
 *
 * Stage-1 trigger only. Stage 3 replaces it with the daemon's char device;
 * every function above is reused verbatim.
 *
 *   create <dir> <name>          mkdir  <dir> <name>
 *   modify <dir> <name> <size>   attrib <dir> <name>
 *   delete <dir> <name>          rename <dir> <name> <dir2> <name2>
 *
 * <dir> is mount-relative; "/" is the root.
 */
int dsfs_inject(const char *line)
{
	char *buf, *p, *cmd, *a1, *a2, *a3, *a4;
	int ret;

	buf = kstrdup(line, GFP_KERNEL);
	if (!buf)
		return -ENOMEM;
	p = strim(buf);

	cmd = strsep(&p, " ");
	a1 = strsep(&p, " ");
	a2 = strsep(&p, " ");
	a3 = strsep(&p, " ");
	a4 = strsep(&p, " ");

	if (!cmd || !a1 || !a2) {
		ret = -EINVAL;
	} else if (!strcmp(cmd, "create")) {
		ret = dsfs_do_create(a1, a2, false);
	} else if (!strcmp(cmd, "mkdir")) {
		ret = dsfs_do_create(a1, a2, true);
	} else if (!strcmp(cmd, "attrib")) {
		ret = dsfs_do_modify(a1, a2, 0, FS_ATTRIB);
	} else if (!strcmp(cmd, "delete")) {
		ret = dsfs_do_delete(a1, a2);
	} else if (!strcmp(cmd, "modify")) {
		long long size = 0;

		if (!a3 || kstrtoll(a3, 10, &size))
			ret = -EINVAL;
		else
			ret = dsfs_do_modify(a1, a2, size, FS_MODIFY);
	} else if (!strcmp(cmd, "rename")) {
		ret = (a3 && a4) ? dsfs_do_rename(a1, a2, a3, a4) : -EINVAL;
	} else {
		ret = -EINVAL;
	}

	kfree(buf);
	return ret;
}

static ssize_t dsfs_inject_write(struct file *file, const char __user *ubuf,
				 size_t count, loff_t *ppos)
{
	char buf[512];
	int ret;

	if (count == 0 || count >= sizeof(buf))
		return -EINVAL;
	if (copy_from_user(buf, ubuf, count))
		return -EFAULT;
	buf[count] = '\0';

	ret = dsfs_inject(buf);
	if (ret)
		return ret;
	return count;
}

static const struct proc_ops dsfs_inject_proc_ops = {
	.proc_write = dsfs_inject_write,
	.proc_lseek = noop_llseek,
};

static struct proc_dir_entry *dsfs_inject_entry;

int dsfs_notify_init(void)
{
	dsfs_inject_entry = proc_create("dsfs-inject", 0200, NULL,
					&dsfs_inject_proc_ops);
	return dsfs_inject_entry ? 0 : -ENOMEM;
}

void dsfs_notify_exit(void)
{
	if (dsfs_inject_entry)
		proc_remove(dsfs_inject_entry);
}
