// SPDX-License-Identifier: GPL-2.0-only
/*
 * Filesystem registration, inodes, and the VFS operations.
 *
 * Every operation has two implementations behind one entry point: the
 * in-memory stage-1 tree, and a round-trip to the daemon. Which one runs is
 * decided by whether the superblock has a connection.
 */
#define pr_fmt(fmt) "dsfs: " fmt

#include <linux/fs.h>
#include <linux/fs_context.h>
#include <linux/fs_parser.h>
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
static const struct inode_operations dsfs_file_inode_operations;
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

/* ----------------------------------------------------------------- inodes */

static void dsfs_init_inode(struct inode *inode, umode_t mode, loff_t size)
{
	inode->i_mode = mode;
	inode->i_uid = GLOBAL_ROOT_UID;
	inode->i_gid = GLOBAL_ROOT_GID;
	simple_inode_init_ts(inode);
	if (S_ISDIR(mode)) {
		inode->i_op = &dsfs_dir_inode_operations;
		inode->i_fop = &dsfs_dir_operations;
		set_nlink(inode, 2);
		inode->i_size = 0;
	} else {
		inode->i_op = &dsfs_file_inode_operations;
		inode->i_fop = &dsfs_file_operations;
		set_nlink(inode, 1);
		inode->i_size = size;
	}
}

/*
 * One inode per node/nodeid. Reusing the same inode across lookups is not an
 * optimisation but a correctness requirement: an inotify mark pins an inode,
 * and handing out a fresh one per lookup would silently drop watches.
 */
struct inode *dsfs_iget_node(struct super_block *sb, struct dsfs_node *node)
{
	struct inode *inode = iget_locked(sb, node->ino);

	if (!inode)
		return ERR_PTR(-ENOMEM);
	if (!(inode->i_state & I_NEW))
		return inode;
	inode->i_private = node;
	dsfs_init_inode(inode, node->mode, node->size);
	unlock_new_inode(inode);
	return inode;
}

struct inode *dsfs_iget_attr(struct super_block *sb, const struct dsfs_attr *attr)
{
	struct inode *inode = iget_locked(sb, (unsigned long)attr->nodeid);

	if (!inode)
		return ERR_PTR(-ENOMEM);
	if (!(inode->i_state & I_NEW)) {
		/* Refresh: the daemon is authoritative about size/mtime. */
		i_size_write(inode, attr->size);
		return inode;
	}
	inode->i_private = NULL;
	dsfs_init_inode(inode, attr->mode, attr->size);
	if (attr->nlink)
		set_nlink(inode, attr->nlink);
	unlock_new_inode(inode);
	return inode;
}

static struct dsfs_conn *dsfs_conn_of(struct inode *inode)
{
	struct dsfs_sb_info *sbi = DSFS_SB(inode->i_sb);

	return sbi ? sbi->conn : NULL;
}

/* ------------------------------------------------------------ operations */

static struct dentry *dsfs_lookup(struct inode *dir, struct dentry *dentry,
				  unsigned int flags)
{
	struct dsfs_conn *conn = dsfs_conn_of(dir);
	struct inode *inode = NULL;

	if (dentry->d_name.len > DSFS_MAX_NAME)
		return ERR_PTR(-ENAMETOOLONG);

	if (conn) {
		struct dsfs_attr attr;
		int ret = dsfs_request(conn, DSFS_OP_LOOKUP, dir->i_ino, 0, 0,
				       dentry->d_name.name, dentry->d_name.len,
				       &attr, sizeof(attr));

		if (ret == -ENOENT)
			return d_splice_alias(NULL, dentry);	/* negative */
		if (ret < 0)
			return ERR_PTR(ret);
		if (ret < (int)sizeof(attr))
			return ERR_PTR(-EIO);
		inode = dsfs_iget_attr(dir->i_sb, &attr);
		if (IS_ERR(inode))
			return ERR_CAST(inode);
		return d_splice_alias(inode, dentry);
	}

	mutex_lock(&dsfs_lock);
	{
		struct dsfs_node *child = dsfs_child(dir->i_private, dentry->d_name.name);

		if (child) {
			inode = dsfs_iget_node(dir->i_sb, child);
			if (IS_ERR(inode)) {
				mutex_unlock(&dsfs_lock);
				return ERR_CAST(inode);
			}
		}
	}
	mutex_unlock(&dsfs_lock);

	/* A negative dentry is cached too: it is what lets a later injected
	 * create find something to instantiate.
	 */
	return d_splice_alias(inode, dentry);
}

static int dsfs_readdir_remote(struct file *file, struct dir_context *ctx,
			       struct dsfs_conn *conn)
{
	struct inode *inode = file_inode(file);
	void *buf = kmalloc(DSFS_MAX_PAYLOAD, GFP_KERNEL);
	int ret = 0;

	if (!buf)
		return -ENOMEM;

	for (;;) {
		size_t pos = 0;
		int len = dsfs_request(conn, DSFS_OP_READDIR, inode->i_ino,
				       ctx->pos, DSFS_MAX_PAYLOAD, NULL, 0,
				       buf, DSFS_MAX_PAYLOAD);

		if (len <= 0) {
			ret = len;	/* 0 = end of directory */
			break;
		}
		while (pos + sizeof(struct dsfs_dirent) <= (size_t)len) {
			struct dsfs_dirent *de = buf + pos;
			size_t entry = DSFS_DIRENT_SIZE(de->namelen);
			const char *name = (const char *)(de + 1);

			/* Daemon-supplied: refuse anything that would read
			 * past the buffer or produce an illegal name.
			 */
			if (de->namelen == 0 || de->namelen > DSFS_MAX_NAME ||
			    pos + entry > (size_t)len) {
				ret = -EIO;
				goto out;
			}
			if (!dir_emit(ctx, name, de->namelen, de->nodeid, de->type))
				goto out;
			ctx->pos = de->off;
			pos += entry;
		}
	}
out:
	kfree(buf);
	return ret;
}

static int dsfs_readdir(struct file *file, struct dir_context *ctx)
{
	struct inode *inode = file_inode(file);
	struct dsfs_conn *conn = dsfs_conn_of(inode);
	struct dsfs_node *child;
	loff_t i = 2;

	if (!dir_emit_dots(file, ctx))
		return 0;
	if (conn)
		return dsfs_readdir_remote(file, ctx, conn);

	mutex_lock(&dsfs_lock);
	list_for_each_entry(child, &((struct dsfs_node *)inode->i_private)->children, sibling) {
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
	struct dsfs_conn *conn = dsfs_conn_of(inode);
	struct dsfs_node *node;
	ssize_t ret = 0;
	loff_t pos = iocb->ki_pos;
	size_t want = iov_iter_count(to);
	size_t avail;

	if (conn) {
		void *buf;
		int len;

		want = min_t(size_t, want, DSFS_MAX_PAYLOAD);
		if (!want)
			return 0;
		buf = kmalloc(want, GFP_KERNEL);
		if (!buf)
			return -ENOMEM;
		len = dsfs_request(conn, DSFS_OP_READ, inode->i_ino, pos, want,
				   NULL, 0, buf, want);
		if (len < 0) {
			kfree(buf);
			return len;
		}
		ret = copy_to_iter(buf, len, to);
		iocb->ki_pos = pos + ret;
		kfree(buf);
		return ret;
	}

	mutex_lock(&dsfs_lock);
	node = inode->i_private;
	if (node && pos < node->size) {
		avail = node->size - pos;
		if (node->data)
			ret = copy_to_iter(node->data + pos, min(avail, want), to);
		else
			ret = iov_iter_zero(min(avail, want), to);
		iocb->ki_pos = pos + ret;
	}
	mutex_unlock(&dsfs_lock);
	return ret;
}

static int dsfs_getattr(struct mnt_idmap *idmap, const struct path *path,
			struct kstat *stat, u32 request_mask, unsigned int flags)
{
	struct inode *inode = d_inode(path->dentry);
	struct dsfs_conn *conn = dsfs_conn_of(inode);

	/* The daemon is authoritative; refresh size/mtime before answering so
	 * a stat() right after an injected event sees the new values.
	 */
	if (conn) {
		struct dsfs_attr attr;
		int ret = dsfs_request(conn, DSFS_OP_GETATTR, inode->i_ino, 0, 0,
				       NULL, 0, &attr, sizeof(attr));

		if (ret >= (int)sizeof(attr)) {
			i_size_write(inode, attr.size);
			if (attr.nlink)
				set_nlink(inode, attr.nlink);
		}
	}
	generic_fillattr(idmap, request_mask, inode, stat);
	return 0;
}

static const struct inode_operations dsfs_dir_inode_operations = {
	.lookup = dsfs_lookup,
	.getattr = dsfs_getattr,
};

static const struct inode_operations dsfs_file_inode_operations = {
	.getattr = dsfs_getattr,
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
	struct dsfs_sb_info *sbi = DSFS_SB(sb);

	mutex_lock(&dsfs_lock);
	if (sbi) {
		if (sbi->root_node)
			dsfs_node_free(sbi->root_node);
		if (sbi->conn) {
			/* Release sleepers before the mount disappears. */
			dsfs_conn_shutdown(sbi->conn);
			dsfs_conn_put(sbi->conn);
		}
		kfree(sbi);
	}
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

/* ------------------------------------------------------------ mount options */

enum dsfs_param { Opt_fd };

static const struct fs_parameter_spec dsfs_fs_parameters[] = {
	fsparam_u32("fd", Opt_fd),
	{}
};

struct dsfs_fc {
	unsigned int fd;
	bool have_fd;
};

static int dsfs_parse_param(struct fs_context *fc, struct fs_parameter *param)
{
	struct dsfs_fc *ctx = fc->fs_private;
	struct fs_parse_result result;
	int opt = fs_parse(fc, dsfs_fs_parameters, param, &result);

	if (opt < 0)
		return opt;
	switch (opt) {
	case Opt_fd:
		ctx->fd = result.uint_32;
		ctx->have_fd = true;
		return 0;
	}
	return -EINVAL;
}

static int dsfs_fill_super(struct super_block *sb, struct fs_context *fc)
{
	struct dsfs_fc *ctx = fc->fs_private;
	struct dsfs_sb_info *sbi;
	struct inode *root_inode;
	int err;

	sb->s_magic = DSFS_MAGIC;
	sb->s_op = &dsfs_super_operations;
	sb->s_blocksize = PAGE_SIZE;
	sb->s_blocksize_bits = PAGE_SHIFT;
	sb->s_maxbytes = MAX_LFS_FILESIZE;
	sb->s_time_gran = 1;

	sbi = kzalloc(sizeof(*sbi), GFP_KERNEL);
	if (!sbi)
		return -ENOMEM;
	sb->s_fs_info = sbi;

	if (ctx->have_fd) {
		struct dsfs_attr attr;
		int ret;

		sbi->conn = dsfs_conn_from_fd(ctx->fd);
		if (!sbi->conn) {
			pr_err("fd=%u is not an open /dev/ds-fs\n", ctx->fd);
			return -EINVAL;
		}
		ret = dsfs_request(sbi->conn, DSFS_OP_GETATTR, DSFS_ROOT_NODEID,
				   0, 0, NULL, 0, &attr, sizeof(attr));
		if (ret < (int)sizeof(attr)) {
			pr_err("daemon did not describe the root (%d)\n", ret);
			return ret < 0 ? ret : -EIO;
		}
		if (!S_ISDIR(attr.mode)) {
			pr_err("daemon root is not a directory\n");
			return -ENOTDIR;
		}
		attr.nodeid = DSFS_ROOT_NODEID;
		root_inode = dsfs_iget_attr(sb, &attr);
	} else {
		struct dsfs_node *root_node = kzalloc(sizeof(*root_node), GFP_KERNEL);

		if (!root_node)
			return -ENOMEM;
		INIT_LIST_HEAD(&root_node->children);
		INIT_LIST_HEAD(&root_node->sibling);
		strscpy(root_node->name, "/", sizeof(root_node->name));
		root_node->mode = S_IFDIR | 0755;
		root_node->ino = 1;
		sbi->root_node = root_node;

		err = dsfs_build_tree(root_node);
		if (err)
			return err;
		root_inode = dsfs_iget_node(sb, root_node);
	}

	if (IS_ERR(root_inode))
		return PTR_ERR(root_inode);
	sb->s_root = d_make_root(root_inode);
	if (!sb->s_root)
		return -ENOMEM;

	dsfs_sb = sb;
	pr_info("mounted (%s)\n", sbi->conn ? "daemon-backed" : "in-memory stage-1 tree");
	return 0;
}

static int dsfs_get_tree(struct fs_context *fc)
{
	return get_tree_nodev(fc, dsfs_fill_super);
}

static void dsfs_free_fc(struct fs_context *fc)
{
	kfree(fc->fs_private);
}

static const struct fs_context_operations dsfs_context_ops = {
	.parse_param = dsfs_parse_param,
	.get_tree = dsfs_get_tree,
	.free = dsfs_free_fc,
};

static int dsfs_init_fs_context(struct fs_context *fc)
{
	struct dsfs_fc *ctx = kzalloc(sizeof(*ctx), GFP_KERNEL);

	if (!ctx)
		return -ENOMEM;
	fc->fs_private = ctx;
	fc->ops = &dsfs_context_ops;
	return 0;
}

static struct file_system_type dsfs_type = {
	.owner = THIS_MODULE,
	.name = "dsfs",
	.init_fs_context = dsfs_init_fs_context,
	.parameters = dsfs_fs_parameters,
	.kill_sb = kill_anon_super,
	.fs_flags = 0,
};

static int __init dsfs_module_init(void)
{
	int err = dsfs_conn_init();

	if (err)
		return err;
	err = register_filesystem(&dsfs_type);
	if (err)
		goto err_conn;
	err = dsfs_notify_init();
	if (err)
		goto err_fs;
	pr_info("loaded (abi %d)\n", DSFS_ABI_VERSION);
	return 0;

err_fs:
	unregister_filesystem(&dsfs_type);
err_conn:
	dsfs_conn_exit();
	return err;
}

static void __exit dsfs_module_exit(void)
{
	dsfs_notify_exit();
	unregister_filesystem(&dsfs_type);
	dsfs_conn_exit();
	pr_info("unloaded\n");
}

module_init(dsfs_module_init);
module_exit(dsfs_module_exit);

MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("drive-sync filesystem with real fsnotify for remote changes");
MODULE_AUTHOR("drive-sync");
