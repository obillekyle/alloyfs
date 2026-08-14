// SPDX-License-Identifier: GPL-2.0-only
/*
 * ds-fs stage 1: filesystem registration and an in-memory tree.
 *
 * Deliberately minimal — this exists so dsfs_notify.c has real inodes and
 * real dentries to notify on. The daemon transport arrives in stage 2.
 */
#define pr_fmt(fmt) "dsfs: " fmt

#include <linux/fs.h>
#include <linux/fs_context.h>
#include <linux/module.h>
#include <linux/pagemap.h>
#include <linux/slab.h>
#include <linux/statfs.h>
#include <linux/string.h>
#include <linux/time.h>

#include "dsfs.h"

DEFINE_MUTEX(dsfs_lock);
struct super_block *dsfs_sb;

static atomic_long_t dsfs_next_ino = ATOMIC_LONG_INIT(2);	/* 1 is the root */

static const struct inode_operations dsfs_dir_inode_operations;
static const struct file_operations dsfs_dir_operations;
static const struct file_operations dsfs_file_operations;

/* ------------------------------------------------------------------ tree */

struct dsfs_node *dsfs_node_new(struct dsfs_node *parent, const char *name, umode_t mode)
{
	struct dsfs_node *node = kzalloc(sizeof(*node), GFP_KERNEL);

	if (!node)
		return NULL;
	INIT_LIST_HEAD(&node->children);
	INIT_LIST_HEAD(&node->sibling);
	strscpy(node->name, name, sizeof(node->name));
	node->mode = mode;
	node->ino = atomic_long_inc_return(&dsfs_next_ino);
	node->parent = parent;
	if (parent)
		list_add_tail(&node->sibling, &parent->children);
	return node;
}

void dsfs_node_free(struct dsfs_node *node)
{
	struct dsfs_node *child, *tmp;

	list_for_each_entry_safe(child, tmp, &node->children, sibling)
		dsfs_node_free(child);
	list_del(&node->sibling);
	kfree(node->data);
	kfree(node);
}

struct dsfs_node *dsfs_child(struct dsfs_node *dir, const char *name)
{
	struct dsfs_node *child;

	list_for_each_entry(child, &dir->children, sibling) {
		if (!strcmp(child->name, name))
			return child;
	}
	return NULL;
}

/* ----------------------------------------------------------------- inode */

struct inode *dsfs_iget(struct super_block *sb, struct dsfs_node *node)
{
	struct inode *inode;

	/* One inode per node, keyed by our own ino so repeated lookups of the
	 * same name return the same inode — inotify marks pin inodes, and a
	 * fresh inode per lookup would silently drop watches.
	 */
	inode = iget_locked(sb, node->ino);
	if (!inode)
		return ERR_PTR(-ENOMEM);
	if (!(inode->i_state & I_NEW))
		return inode;

	inode->i_mode = node->mode;
	inode->i_uid = GLOBAL_ROOT_UID;
	inode->i_gid = GLOBAL_ROOT_GID;
	inode->i_private = node;
	simple_inode_init_ts(inode);

	if (S_ISDIR(node->mode)) {
		inode->i_op = &dsfs_dir_inode_operations;
		inode->i_fop = &dsfs_dir_operations;
		set_nlink(inode, 2);
		inode->i_size = 0;
	} else {
		inode->i_op = &simple_dir_inode_operations;	/* getattr/setattr defaults */
		inode->i_fop = &dsfs_file_operations;
		set_nlink(inode, 1);
		inode->i_size = node->size;
	}
	unlock_new_inode(inode);
	return inode;
}

/* ------------------------------------------------------------ operations */

static struct dentry *dsfs_lookup(struct inode *dir, struct dentry *dentry,
				  unsigned int flags)
{
	struct dsfs_node *dnode = dir->i_private;
	struct dsfs_node *child;
	struct inode *inode = NULL;

	if (dentry->d_name.len > NAME_MAX)
		return ERR_PTR(-ENAMETOOLONG);

	mutex_lock(&dsfs_lock);
	child = dsfs_child(dnode, dentry->d_name.name);
	if (child) {
		inode = dsfs_iget(dir->i_sb, child);
		if (IS_ERR(inode)) {
			mutex_unlock(&dsfs_lock);
			return ERR_CAST(inode);
		}
	}
	mutex_unlock(&dsfs_lock);

	/* A negative dentry is cached too: it is what lets a later injected
	 * create find something to instantiate.
	 */
	return d_splice_alias(inode, dentry);
}

static int dsfs_readdir(struct file *file, struct dir_context *ctx)
{
	struct inode *inode = file_inode(file);
	struct dsfs_node *dnode = inode->i_private;
	struct dsfs_node *child;
	loff_t i = 2;

	if (!dir_emit_dots(file, ctx))
		return 0;

	mutex_lock(&dsfs_lock);
	list_for_each_entry(child, &dnode->children, sibling) {
		if (i++ < ctx->pos)
			continue;
		if (!dir_emit(ctx, child->name, strlen(child->name), child->ino,
			      S_ISDIR(child->mode) ? DT_DIR : DT_REG)) {
			break;
		}
		ctx->pos++;
	}
	mutex_unlock(&dsfs_lock);
	return 0;
}

static ssize_t dsfs_read_iter(struct kiocb *iocb, struct iov_iter *to)
{
	struct inode *inode = file_inode(iocb->ki_filp);
	struct dsfs_node *node = inode->i_private;
	ssize_t ret = 0;
	loff_t pos = iocb->ki_pos;
	size_t avail;

	mutex_lock(&dsfs_lock);
	if (pos < node->size) {
		avail = node->size - pos;
		if (node->data) {
			ret = copy_to_iter(node->data + pos, min(avail, iov_iter_count(to)), to);
		} else {
			/* No backing buffer: report zeroes, which is enough for
			 * the size/stat assertions this stage makes.
			 */
			ret = iov_iter_zero(min(avail, iov_iter_count(to)), to);
		}
		iocb->ki_pos = pos + ret;
	}
	mutex_unlock(&dsfs_lock);
	return ret;
}

static const struct inode_operations dsfs_dir_inode_operations = {
	.lookup = dsfs_lookup,
};

static const struct file_operations dsfs_dir_operations = {
	.owner = THIS_MODULE,
	.llseek = generic_file_llseek,
	.read = generic_read_dir,
	.iterate_shared = dsfs_readdir,
};

static const struct file_operations dsfs_file_operations = {
	.owner = THIS_MODULE,
	.llseek = generic_file_llseek,
	.read_iter = dsfs_read_iter,
};

/* ------------------------------------------------------------ superblock */

static void dsfs_put_super(struct super_block *sb)
{
	struct dsfs_node *root = sb->s_fs_info;

	mutex_lock(&dsfs_lock);
	if (root)
		dsfs_node_free(root);
	sb->s_fs_info = NULL;
	if (dsfs_sb == sb)
		dsfs_sb = NULL;
	mutex_unlock(&dsfs_lock);
}

static const struct super_operations dsfs_super_operations = {
	.statfs = simple_statfs,
	.drop_inode = generic_delete_inode,
	.put_super = dsfs_put_super,
};

/* The hardcoded tree the stage-1 assertions are written against. */
static int dsfs_build_tree(struct dsfs_node *root)
{
	struct dsfs_node *a, *sub, *b;

	a = dsfs_node_new(root, "a.txt", S_IFREG | 0644);
	if (!a)
		return -ENOMEM;
	a->data = kstrdup("hello world", GFP_KERNEL);
	if (!a->data)
		return -ENOMEM;
	a->size = strlen(a->data);

	sub = dsfs_node_new(root, "sub", S_IFDIR | 0755);
	if (!sub)
		return -ENOMEM;

	b = dsfs_node_new(sub, "b.txt", S_IFREG | 0644);
	if (!b)
		return -ENOMEM;
	b->data = kstrdup("bee", GFP_KERNEL);
	if (!b->data)
		return -ENOMEM;
	b->size = strlen(b->data);
	return 0;
}

static int dsfs_fill_super(struct super_block *sb, struct fs_context *fc)
{
	struct dsfs_node *root_node;
	struct inode *root_inode;
	int err;

	sb->s_magic = DSFS_MAGIC;
	sb->s_op = &dsfs_super_operations;
	sb->s_blocksize = PAGE_SIZE;
	sb->s_blocksize_bits = PAGE_SHIFT;
	sb->s_maxbytes = MAX_LFS_FILESIZE;
	sb->s_time_gran = 1;

	root_node = kzalloc(sizeof(*root_node), GFP_KERNEL);
	if (!root_node)
		return -ENOMEM;
	INIT_LIST_HEAD(&root_node->children);
	INIT_LIST_HEAD(&root_node->sibling);
	strscpy(root_node->name, "/", sizeof(root_node->name));
	root_node->mode = S_IFDIR | 0755;
	root_node->ino = 1;
	sb->s_fs_info = root_node;

	err = dsfs_build_tree(root_node);
	if (err)
		return err;

	root_inode = dsfs_iget(sb, root_node);
	if (IS_ERR(root_inode))
		return PTR_ERR(root_inode);

	sb->s_root = d_make_root(root_inode);
	if (!sb->s_root)
		return -ENOMEM;

	dsfs_sb = sb;
	pr_info("mounted (in-memory stage-1 tree)\n");
	return 0;
}

static int dsfs_get_tree(struct fs_context *fc)
{
	return get_tree_nodev(fc, dsfs_fill_super);
}

static const struct fs_context_operations dsfs_context_ops = {
	.get_tree = dsfs_get_tree,
};

static int dsfs_init_fs_context(struct fs_context *fc)
{
	fc->ops = &dsfs_context_ops;
	return 0;
}

static struct file_system_type dsfs_type = {
	.owner = THIS_MODULE,
	.name = "dsfs",
	.init_fs_context = dsfs_init_fs_context,
	.kill_sb = kill_anon_super,
	.fs_flags = 0,
};

static int __init dsfs_module_init(void)
{
	int err = register_filesystem(&dsfs_type);

	if (err)
		return err;
	err = dsfs_notify_init();
	if (err) {
		unregister_filesystem(&dsfs_type);
		return err;
	}
	pr_info("loaded\n");
	return 0;
}

static void __exit dsfs_module_exit(void)
{
	dsfs_notify_exit();
	unregister_filesystem(&dsfs_type);
	pr_info("unloaded\n");
}

module_init(dsfs_module_init);
module_exit(dsfs_module_exit);

MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("drive-sync filesystem (stage 1: fsnotify injection spike)");
MODULE_AUTHOR("drive-sync");
