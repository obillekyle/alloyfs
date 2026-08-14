// SPDX-License-Identifier: GPL-2.0-only
/*
 * Filesystem registration, inodes, and the VFS operations.
 *
 * Every operation has two implementations behind one entry point: the
 * in-memory stage-1 tree, and a round-trip to the daemon. Which one runs is
 * decided by whether the superblock has a connection.
 */
#define pr_fmt(fmt) "alloyfs: " fmt

#include <linux/fs.h>
#include <linux/fs_context.h>
#include <linux/fs_parser.h>
#include <linux/module.h>
#include <linux/pagemap.h>
#include <linux/slab.h>
#include <linux/statfs.h>
#include <linux/string.h>
#include <linux/time.h>

#include "alloyfs.h"

DEFINE_MUTEX(alloyfs_lock);
struct super_block *alloyfs_sb;

static atomic_long_t alloyfs_next_ino = ATOMIC_LONG_INIT(2);	/* 1 is the root */

static const struct inode_operations alloyfs_dir_inode_operations;
static const struct inode_operations alloyfs_file_inode_operations;
static const struct file_operations alloyfs_dir_operations;
static const struct file_operations alloyfs_file_operations;

/* ------------------------------------------------------------------ tree */

struct alloyfs_node *alloyfs_node_new(struct alloyfs_node *parent, const char *name, umode_t mode)
{
	struct alloyfs_node *node = kzalloc(sizeof(*node), GFP_KERNEL);

	if (!node)
		return NULL;
	INIT_LIST_HEAD(&node->children);
	INIT_LIST_HEAD(&node->sibling);
	strscpy(node->name, name, sizeof(node->name));
	node->mode = mode;
	node->ino = atomic_long_inc_return(&alloyfs_next_ino);
	node->parent = parent;
	if (parent)
		list_add_tail(&node->sibling, &parent->children);
	return node;
}

void alloyfs_node_free(struct alloyfs_node *node)
{
	struct alloyfs_node *child, *tmp;

	list_for_each_entry_safe(child, tmp, &node->children, sibling)
		alloyfs_node_free(child);
	list_del(&node->sibling);
	kfree(node->data);
	kfree(node);
}

struct alloyfs_node *alloyfs_child(struct alloyfs_node *dir, const char *name)
{
	struct alloyfs_node *child;

	list_for_each_entry(child, &dir->children, sibling) {
		if (!strcmp(child->name, name))
			return child;
	}
	return NULL;
}

/* ----------------------------------------------------------------- inodes */

static void alloyfs_init_inode(struct inode *inode, umode_t mode, loff_t size)
{
	inode->i_mode = mode;
	inode->i_uid = GLOBAL_ROOT_UID;
	inode->i_gid = GLOBAL_ROOT_GID;
	simple_inode_init_ts(inode);
	if (S_ISDIR(mode)) {
		inode->i_op = &alloyfs_dir_inode_operations;
		inode->i_fop = &alloyfs_dir_operations;
		set_nlink(inode, 2);
		inode->i_size = 0;
	} else {
		inode->i_op = &alloyfs_file_inode_operations;
		inode->i_fop = &alloyfs_file_operations;
		set_nlink(inode, 1);
		inode->i_size = size;
	}
}

/*
 * One inode per node/nodeid. Reusing the same inode across lookups is not an
 * optimisation but a correctness requirement: an inotify mark pins an inode,
 * and handing out a fresh one per lookup would silently drop watches.
 */
struct inode *alloyfs_iget_node(struct super_block *sb, struct alloyfs_node *node)
{
	struct inode *inode = iget_locked(sb, node->ino);

	if (!inode)
		return ERR_PTR(-ENOMEM);
	if (!(inode->i_state & I_NEW))
		return inode;
	inode->i_private = node;
	alloyfs_init_inode(inode, node->mode, node->size);
	unlock_new_inode(inode);
	return inode;
}

struct inode *alloyfs_iget_attr(struct super_block *sb, const struct alloyfs_attr *attr)
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
	alloyfs_init_inode(inode, attr->mode, attr->size);
	if (attr->nlink)
		set_nlink(inode, attr->nlink);
	unlock_new_inode(inode);
	return inode;
}

static struct alloyfs_conn *alloyfs_conn_of(struct inode *inode)
{
	struct alloyfs_sb_info *sbi = ALLOYFS_SB(inode->i_sb);

	return sbi ? sbi->conn : NULL;
}

/* ------------------------------------------------------------ operations */

static struct dentry *alloyfs_lookup(struct inode *dir, struct dentry *dentry,
				  unsigned int flags)
{
	struct alloyfs_conn *conn = alloyfs_conn_of(dir);
	struct inode *inode = NULL;

	if (dentry->d_name.len > ALLOYFS_MAX_NAME)
		return ERR_PTR(-ENAMETOOLONG);

	if (conn) {
		struct alloyfs_attr attr;
		int ret = alloyfs_request(conn, ALLOYFS_OP_LOOKUP, dir->i_ino, 0, 0,
				       dentry->d_name.name, dentry->d_name.len,
				       &attr, sizeof(attr));

		if (ret == -ENOENT)
			return d_splice_alias(NULL, dentry);	/* negative */
		if (ret < 0)
			return ERR_PTR(ret);
		if (ret < (int)sizeof(attr))
			return ERR_PTR(-EIO);
		inode = alloyfs_iget_attr(dir->i_sb, &attr);
		if (IS_ERR(inode))
			return ERR_CAST(inode);
		return d_splice_alias(inode, dentry);
	}

	mutex_lock(&alloyfs_lock);
	{
		struct alloyfs_node *child = alloyfs_child(dir->i_private, dentry->d_name.name);

		if (child) {
			inode = alloyfs_iget_node(dir->i_sb, child);
			if (IS_ERR(inode)) {
				mutex_unlock(&alloyfs_lock);
				return ERR_CAST(inode);
			}
		}
	}
	mutex_unlock(&alloyfs_lock);

	/* A negative dentry is cached too: it is what lets a later injected
	 * create find something to instantiate.
	 */
	return d_splice_alias(inode, dentry);
}

static int alloyfs_readdir_remote(struct file *file, struct dir_context *ctx,
			       struct alloyfs_conn *conn)
{
	struct inode *inode = file_inode(file);
	void *buf = kmalloc(ALLOYFS_MAX_PAYLOAD, GFP_KERNEL);
	int ret = 0;

	if (!buf)
		return -ENOMEM;

	for (;;) {
		size_t pos = 0;
		int len = alloyfs_request(conn, ALLOYFS_OP_READDIR, inode->i_ino,
				       ctx->pos, ALLOYFS_MAX_PAYLOAD, NULL, 0,
				       buf, ALLOYFS_MAX_PAYLOAD);

		if (len <= 0) {
			ret = len;	/* 0 = end of directory */
			break;
		}
		while (pos + sizeof(struct alloyfs_dirent) <= (size_t)len) {
			struct alloyfs_dirent *de = buf + pos;
			size_t entry = ALLOYFS_DIRENT_SIZE(de->namelen);
			const char *name = (const char *)(de + 1);

			/* Daemon-supplied: refuse anything that would read
			 * past the buffer or produce an illegal name.
			 */
			if (de->namelen == 0 || de->namelen > ALLOYFS_MAX_NAME ||
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

static int alloyfs_readdir(struct file *file, struct dir_context *ctx)
{
	struct inode *inode = file_inode(file);
	struct alloyfs_conn *conn = alloyfs_conn_of(inode);
	struct alloyfs_node *child;
	loff_t i = 2;

	if (!dir_emit_dots(file, ctx))
		return 0;
	if (conn)
		return alloyfs_readdir_remote(file, ctx, conn);

	mutex_lock(&alloyfs_lock);
	list_for_each_entry(child, &((struct alloyfs_node *)inode->i_private)->children, sibling) {
		if (i++ < ctx->pos)
			continue;
		if (!dir_emit(ctx, child->name, strlen(child->name), child->ino,
			      S_ISDIR(child->mode) ? DT_DIR : DT_REG)) {
			break;
		}
		ctx->pos++;
	}
	mutex_unlock(&alloyfs_lock);
	return 0;
}

static ssize_t alloyfs_read_iter(struct kiocb *iocb, struct iov_iter *to)
{
	struct inode *inode = file_inode(iocb->ki_filp);
	struct alloyfs_conn *conn = alloyfs_conn_of(inode);
	struct alloyfs_node *node;
	ssize_t ret = 0;
	loff_t pos = iocb->ki_pos;
	size_t want = iov_iter_count(to);
	size_t avail;

	if (conn) {
		void *buf;
		int len;

		want = min_t(size_t, want, ALLOYFS_MAX_PAYLOAD);
		if (!want)
			return 0;
		buf = kmalloc(want, GFP_KERNEL);
		if (!buf)
			return -ENOMEM;
		len = alloyfs_request(conn, ALLOYFS_OP_READ, inode->i_ino, pos, want,
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

	mutex_lock(&alloyfs_lock);
	node = inode->i_private;
	if (node && pos < node->size) {
		avail = node->size - pos;
		if (node->data)
			ret = copy_to_iter(node->data + pos, min(avail, want), to);
		else
			ret = iov_iter_zero(min(avail, want), to);
		iocb->ki_pos = pos + ret;
	}
	mutex_unlock(&alloyfs_lock);
	return ret;
}

/* ------------------------------------------------------------- mutations
 *
 * Every mutation is WRITE-THROUGH: the syscall does not return until the
 * daemon has acknowledged, so an acknowledged write is durable as far as the
 * application is concerned and there is no writeback cache to lose. Slower
 * than buffering, but a filesystem that loses acknowledged writes is a bug
 * generator, and the client above already batches at a higher level.
 *
 * Local mutations do NOT emit fsnotify events here: the VFS already does
 * that for us (vfs_create -> fsnotify_create, vfs_unlink -> fsnotify_unlink,
 * and so on). Only remote changes need the injection path — that asymmetry
 * is the whole design.
 */

/* Shared by create() and mkdir(): both send mode + name and get an attr. */
static int alloyfs_do_mknod(struct inode *dir, struct dentry *dentry, umode_t mode,
			 u32 opcode)
{
	struct alloyfs_conn *conn = alloyfs_conn_of(dir);
	struct alloyfs_create_in *in;
	struct alloyfs_attr attr;
	struct inode *inode;
	size_t len = sizeof(*in) + dentry->d_name.len;
	int ret;

	if (!conn)
		return -EROFS;	/* the in-memory demo tree is read-only */
	if (dentry->d_name.len > ALLOYFS_MAX_NAME)
		return -ENAMETOOLONG;

	in = kzalloc(len, GFP_KERNEL);
	if (!in)
		return -ENOMEM;
	in->mode = mode;
	memcpy(in + 1, dentry->d_name.name, dentry->d_name.len);

	ret = alloyfs_request(conn, opcode, dir->i_ino, 0, 0, in, len,
			   &attr, sizeof(attr));
	kfree(in);
	if (ret < 0)
		return ret;
	if (ret < (int)sizeof(attr))
		return -EIO;

	inode = alloyfs_iget_attr(dir->i_sb, &attr);
	if (IS_ERR(inode))
		return PTR_ERR(inode);
	d_instantiate(dentry, inode);
	if (S_ISDIR(mode))
		inc_nlink(dir);
	return 0;
}

static int alloyfs_create(struct mnt_idmap *idmap, struct inode *dir,
		       struct dentry *dentry, umode_t mode, bool excl)
{
	return alloyfs_do_mknod(dir, dentry, mode | S_IFREG, ALLOYFS_OP_CREATE);
}

static int alloyfs_mkdir(struct mnt_idmap *idmap, struct inode *dir,
		      struct dentry *dentry, umode_t mode)
{
	return alloyfs_do_mknod(dir, dentry, mode | S_IFDIR, ALLOYFS_OP_MKDIR);
}

static int alloyfs_do_remove(struct inode *dir, struct dentry *dentry, u32 opcode)
{
	struct alloyfs_conn *conn = alloyfs_conn_of(dir);
	int ret;

	if (!conn)
		return -EROFS;
	ret = alloyfs_request(conn, opcode, dir->i_ino, 0, 0,
			   dentry->d_name.name, dentry->d_name.len, NULL, 0);
	if (ret < 0)
		return ret;

	/* The name is gone; make the inode agree so a watch on the file sees
	 * DELETE_SELF and a stale open fd reports the truth.
	 */
	if (d_really_is_positive(dentry)) {
		struct inode *inode = d_inode(dentry);

		if (opcode == ALLOYFS_OP_RMDIR) {
			clear_nlink(inode);
			drop_nlink(dir);
		} else if (inode->i_nlink) {
			drop_nlink(inode);
		}
	}
	return 0;
}

static int alloyfs_unlink(struct inode *dir, struct dentry *dentry)
{
	return alloyfs_do_remove(dir, dentry, ALLOYFS_OP_UNLINK);
}

static int alloyfs_rmdir(struct inode *dir, struct dentry *dentry)
{
	return alloyfs_do_remove(dir, dentry, ALLOYFS_OP_RMDIR);
}

static int alloyfs_rename(struct mnt_idmap *idmap, struct inode *old_dir,
		       struct dentry *old_dentry, struct inode *new_dir,
		       struct dentry *new_dentry, unsigned int flags)
{
	struct alloyfs_conn *conn = alloyfs_conn_of(old_dir);
	struct alloyfs_rename_in *in;
	size_t len;
	int ret;

	if (!conn)
		return -EROFS;
	/* EXCHANGE and WHITEOUT have no wire representation; refusing them is
	 * correct and the VFS falls back for callers that can.
	 */
	if (flags & ~RENAME_NOREPLACE)
		return -EINVAL;
	if (old_dentry->d_name.len > ALLOYFS_MAX_NAME ||
	    new_dentry->d_name.len > ALLOYFS_MAX_NAME)
		return -ENAMETOOLONG;
	if ((flags & RENAME_NOREPLACE) && d_really_is_positive(new_dentry))
		return -EEXIST;

	len = sizeof(*in) + old_dentry->d_name.len + new_dentry->d_name.len;
	in = kzalloc(len, GFP_KERNEL);
	if (!in)
		return -ENOMEM;
	in->newparent = new_dir->i_ino;
	in->namelen = old_dentry->d_name.len;
	in->newnamelen = new_dentry->d_name.len;
	memcpy((char *)(in + 1), old_dentry->d_name.name, old_dentry->d_name.len);
	memcpy((char *)(in + 1) + old_dentry->d_name.len,
	       new_dentry->d_name.name, new_dentry->d_name.len);

	ret = alloyfs_request(conn, ALLOYFS_OP_RENAME, old_dir->i_ino, 0, 0, in, len,
			   NULL, 0);
	kfree(in);
	if (ret < 0)
		return ret;

	/* A replaced target is gone from the namespace. */
	if (d_really_is_positive(new_dentry)) {
		struct inode *victim = d_inode(new_dentry);

		if (S_ISDIR(victim->i_mode)) {
			clear_nlink(victim);
			drop_nlink(new_dir);
		} else if (victim->i_nlink) {
			drop_nlink(victim);
		}
	}
	if (S_ISDIR(d_inode(old_dentry)->i_mode) && old_dir != new_dir) {
		drop_nlink(old_dir);
		inc_nlink(new_dir);
	}
	return 0;
}

static ssize_t alloyfs_write_iter(struct kiocb *iocb, struct iov_iter *from)
{
	struct inode *inode = file_inode(iocb->ki_filp);
	struct alloyfs_conn *conn = alloyfs_conn_of(inode);
	ssize_t total = 0;
	loff_t pos;
	int err;

	if (!conn)
		return -EROFS;

	inode_lock(inode);
	err = generic_write_checks(iocb, from) <= 0 ? -EINVAL : 0;
	if (err && iov_iter_count(from)) {
		inode_unlock(inode);
		return err;
	}
	pos = iocb->ki_pos;

	/* One request per DATA_CHUNK: bounded memory, and a partial failure
	 * still reports exactly how much reached the server.
	 */
	while (iov_iter_count(from)) {
		size_t want = min_t(size_t, iov_iter_count(from), ALLOYFS_MAX_PAYLOAD);
		struct alloyfs_write_out out;
		void *buf = kvmalloc(want, GFP_KERNEL);
		size_t got;
		int ret;

		if (!buf) {
			if (!total)
				total = -ENOMEM;
			break;
		}
		got = copy_from_iter(buf, want, from);
		if (!got) {
			kvfree(buf);
			break;
		}
		ret = alloyfs_request(conn, ALLOYFS_OP_WRITE, inode->i_ino, pos, got,
				   buf, got, &out, sizeof(out));
		kvfree(buf);
		if (ret < 0) {
			if (!total)
				total = ret;
			break;
		}
		if (ret < (int)sizeof(out) || out.written == 0) {
			if (!total)
				total = -EIO;
			break;
		}
		pos += out.written;
		total += out.written;
		if (out.written < got)
			break;	/* short write: the server took what it could */
	}

	if (total > 0) {
		iocb->ki_pos = pos;
		if (pos > i_size_read(inode))
			i_size_write(inode, pos);
		inode_set_mtime_to_ts(inode, current_time(inode));
	}
	inode_unlock(inode);
	return total;
}

static int alloyfs_setattr(struct mnt_idmap *idmap, struct dentry *dentry,
			struct iattr *attr)
{
	struct inode *inode = d_inode(dentry);
	struct alloyfs_conn *conn = alloyfs_conn_of(inode);
	struct alloyfs_setattr_in in = {};
	struct alloyfs_attr out;
	int ret;

	ret = setattr_prepare(idmap, dentry, attr);
	if (ret)
		return ret;
	if (!conn)
		return -EROFS;

	if (attr->ia_valid & ATTR_MODE) {
		in.valid |= ALLOYFS_SETATTR_MODE;
		in.mode = attr->ia_mode;
	}
	if (attr->ia_valid & ATTR_SIZE) {
		in.valid |= ALLOYFS_SETATTR_SIZE;
		in.size = attr->ia_size;
	}
	if (attr->ia_valid & ATTR_MTIME) {
		in.valid |= ALLOYFS_SETATTR_MTIME;
		in.mtime_ns = (u64)attr->ia_mtime.tv_sec * NSEC_PER_SEC +
			      attr->ia_mtime.tv_nsec;
	}
	if (!in.valid)
		return 0;

	ret = alloyfs_request(conn, ALLOYFS_OP_SETATTR, inode->i_ino, 0, 0,
			   &in, sizeof(in), &out, sizeof(out));
	if (ret < 0)
		return ret;

	setattr_copy(idmap, inode, attr);
	if (attr->ia_valid & ATTR_SIZE) {
		truncate_setsize(inode, attr->ia_size);
		i_size_write(inode, attr->ia_size);
	}
	mark_inode_dirty(inode);
	return 0;
}

static int alloyfs_getattr(struct mnt_idmap *idmap, const struct path *path,
			struct kstat *stat, u32 request_mask, unsigned int flags)
{
	struct inode *inode = d_inode(path->dentry);
	struct alloyfs_conn *conn = alloyfs_conn_of(inode);

	/* The daemon is authoritative; refresh size/mtime before answering so
	 * a stat() right after an injected event sees the new values.
	 */
	if (conn) {
		struct alloyfs_attr attr;
		int ret = alloyfs_request(conn, ALLOYFS_OP_GETATTR, inode->i_ino, 0, 0,
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

static const struct inode_operations alloyfs_dir_inode_operations = {
	.lookup = alloyfs_lookup,
	.getattr = alloyfs_getattr,
	.setattr = alloyfs_setattr,
	.create = alloyfs_create,
	.mkdir = alloyfs_mkdir,
	.unlink = alloyfs_unlink,
	.rmdir = alloyfs_rmdir,
	.rename = alloyfs_rename,
};

static const struct inode_operations alloyfs_file_inode_operations = {
	.getattr = alloyfs_getattr,
	.setattr = alloyfs_setattr,
};

static const struct file_operations alloyfs_dir_operations = {
	.owner = THIS_MODULE,
	.llseek = generic_file_llseek,
	.read = generic_read_dir,
	.iterate_shared = alloyfs_readdir,
};

/* Write-through means there is nothing to flush: fsync is a no-op that
 * honestly reports success rather than pretending to sync a cache we do
 * not keep.
 */
static int alloyfs_fsync(struct file *file, loff_t start, loff_t end, int datasync)
{
	return 0;
}

static const struct file_operations alloyfs_file_operations = {
	.owner = THIS_MODULE,
	.llseek = generic_file_llseek,
	.read_iter = alloyfs_read_iter,
	.write_iter = alloyfs_write_iter,
	.fsync = alloyfs_fsync,
};

/* ------------------------------------------------------------ superblock */

static void alloyfs_put_super(struct super_block *sb)
{
	struct alloyfs_sb_info *sbi = ALLOYFS_SB(sb);

	mutex_lock(&alloyfs_lock);
	if (sbi) {
		if (sbi->root_node)
			alloyfs_node_free(sbi->root_node);
		if (sbi->conn) {
			/* Release sleepers before the mount disappears. */
			alloyfs_conn_shutdown(sbi->conn);
			alloyfs_conn_put(sbi->conn);
		}
		kfree(sbi);
	}
	sb->s_fs_info = NULL;
	if (alloyfs_sb == sb)
		alloyfs_sb = NULL;
	mutex_unlock(&alloyfs_lock);
}

static const struct super_operations alloyfs_super_operations = {
	.statfs = simple_statfs,
	.drop_inode = generic_delete_inode,
	.put_super = alloyfs_put_super,
};

/* The hardcoded tree the stage-1 assertions are written against. */
static int alloyfs_build_tree(struct alloyfs_node *root)
{
	struct alloyfs_node *a, *sub, *b;

	a = alloyfs_node_new(root, "a.txt", S_IFREG | 0644);
	if (!a)
		return -ENOMEM;
	a->data = kstrdup("hello world", GFP_KERNEL);
	if (!a->data)
		return -ENOMEM;
	a->size = strlen(a->data);

	sub = alloyfs_node_new(root, "sub", S_IFDIR | 0755);
	if (!sub)
		return -ENOMEM;

	b = alloyfs_node_new(sub, "b.txt", S_IFREG | 0644);
	if (!b)
		return -ENOMEM;
	b->data = kstrdup("bee", GFP_KERNEL);
	if (!b->data)
		return -ENOMEM;
	b->size = strlen(b->data);
	return 0;
}

/* ------------------------------------------------------------ mount options */

enum alloyfs_param { Opt_fd };

static const struct fs_parameter_spec alloyfs_fs_parameters[] = {
	fsparam_u32("fd", Opt_fd),
	{}
};

struct alloyfs_fc {
	unsigned int fd;
	bool have_fd;
};

static int alloyfs_parse_param(struct fs_context *fc, struct fs_parameter *param)
{
	struct alloyfs_fc *ctx = fc->fs_private;
	struct fs_parse_result result;
	int opt = fs_parse(fc, alloyfs_fs_parameters, param, &result);

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

static int alloyfs_fill_super(struct super_block *sb, struct fs_context *fc)
{
	struct alloyfs_fc *ctx = fc->fs_private;
	struct alloyfs_sb_info *sbi;
	struct inode *root_inode;
	int err;

	sb->s_magic = ALLOYFS_MAGIC;
	sb->s_op = &alloyfs_super_operations;
	sb->s_blocksize = PAGE_SIZE;
	sb->s_blocksize_bits = PAGE_SHIFT;
	sb->s_maxbytes = MAX_LFS_FILESIZE;
	sb->s_time_gran = 1;

	sbi = kzalloc(sizeof(*sbi), GFP_KERNEL);
	if (!sbi)
		return -ENOMEM;
	sb->s_fs_info = sbi;

	if (ctx->have_fd) {
		struct alloyfs_attr attr;
		int ret;

		sbi->conn = alloyfs_conn_from_fd(ctx->fd);
		if (!sbi->conn) {
			pr_err("fd=%u is not an open /dev/alloyfs\n", ctx->fd);
			return -EINVAL;
		}
		ret = alloyfs_request(sbi->conn, ALLOYFS_OP_GETATTR, ALLOYFS_ROOT_NODEID,
				   0, 0, NULL, 0, &attr, sizeof(attr));
		if (ret < (int)sizeof(attr)) {
			pr_err("daemon did not describe the root (%d)\n", ret);
			return ret < 0 ? ret : -EIO;
		}
		if (!S_ISDIR(attr.mode)) {
			pr_err("daemon root is not a directory\n");
			return -ENOTDIR;
		}
		attr.nodeid = ALLOYFS_ROOT_NODEID;
		root_inode = alloyfs_iget_attr(sb, &attr);
	} else {
		struct alloyfs_node *root_node = kzalloc(sizeof(*root_node), GFP_KERNEL);

		if (!root_node)
			return -ENOMEM;
		INIT_LIST_HEAD(&root_node->children);
		INIT_LIST_HEAD(&root_node->sibling);
		strscpy(root_node->name, "/", sizeof(root_node->name));
		root_node->mode = S_IFDIR | 0755;
		root_node->ino = 1;
		sbi->root_node = root_node;

		err = alloyfs_build_tree(root_node);
		if (err)
			return err;
		root_inode = alloyfs_iget_node(sb, root_node);
	}

	if (IS_ERR(root_inode))
		return PTR_ERR(root_inode);
	sb->s_root = d_make_root(root_inode);
	if (!sb->s_root)
		return -ENOMEM;

	alloyfs_sb = sb;
	pr_info("mounted (%s)\n", sbi->conn ? "daemon-backed" : "in-memory stage-1 tree");
	return 0;
}

static int alloyfs_get_tree(struct fs_context *fc)
{
	return get_tree_nodev(fc, alloyfs_fill_super);
}

static void alloyfs_free_fc(struct fs_context *fc)
{
	kfree(fc->fs_private);
}

static const struct fs_context_operations alloyfs_context_ops = {
	.parse_param = alloyfs_parse_param,
	.get_tree = alloyfs_get_tree,
	.free = alloyfs_free_fc,
};

static int alloyfs_init_fs_context(struct fs_context *fc)
{
	struct alloyfs_fc *ctx = kzalloc(sizeof(*ctx), GFP_KERNEL);

	if (!ctx)
		return -ENOMEM;
	fc->fs_private = ctx;
	fc->ops = &alloyfs_context_ops;
	return 0;
}

static struct file_system_type alloyfs_type = {
	.owner = THIS_MODULE,
	.name = "alloyfs",
	.init_fs_context = alloyfs_init_fs_context,
	.parameters = alloyfs_fs_parameters,
	.kill_sb = kill_anon_super,
	.fs_flags = 0,
};

static int __init alloyfs_module_init(void)
{
	int err = alloyfs_conn_init();

	if (err)
		return err;
	err = register_filesystem(&alloyfs_type);
	if (err)
		goto err_conn;
	err = alloyfs_notify_init();
	if (err)
		goto err_fs;
	pr_info("loaded (abi %d)\n", ALLOYFS_ABI_VERSION);
	return 0;

err_fs:
	unregister_filesystem(&alloyfs_type);
err_conn:
	alloyfs_conn_exit();
	return err;
}

static void __exit alloyfs_module_exit(void)
{
	alloyfs_notify_exit();
	unregister_filesystem(&alloyfs_type);
	alloyfs_conn_exit();
	pr_info("unloaded\n");
}

module_init(alloyfs_module_init);
module_exit(alloyfs_module_exit);

MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("alloyfs filesystem with real fsnotify for remote changes");
MODULE_AUTHOR("alloyfs");
