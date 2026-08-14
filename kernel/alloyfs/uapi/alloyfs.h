/* SPDX-License-Identifier: GPL-2.0-only WITH Linux-syscall-note */
/*
 * AlloyFS kernel <-> daemon ABI.
 *
 * Deliberately smaller than FUSE's: we control both ends and ship them
 * together, so this covers exactly the operations the filesystem needs and
 * nothing speculative. Every struct is fixed-size, naturally aligned, and
 * has an explicit length so the receiver never trusts a pointer.
 *
 * Flow: the daemon opens /dev/alloyfs, passes the fd to mount as `-o fd=N`,
 * then loops { read() a request; write() a response }. Requests carry a
 * `unique` the response must echo back; the kernel matches on it, so the
 * daemon may answer out of order.
 */
#ifndef _UAPI_DS_FS_H
#define _UAPI_DS_FS_H

#include <linux/types.h>

#define ALLOYFS_ABI_VERSION 3
#define ALLOYFS_ROOT_NODEID 1ULL

/* Largest payload either direction; bounds every kernel-side allocation. */
#define ALLOYFS_MAX_PAYLOAD (128 * 1024)
#define ALLOYFS_MAX_NAME 255

enum alloyfs_opcode {
	ALLOYFS_OP_LOOKUP = 1,	/* nodeid = parent dir, payload = name */
	ALLOYFS_OP_GETATTR = 2,	/* nodeid = target */
	ALLOYFS_OP_READDIR = 3,	/* nodeid = dir, offset = cursor */
	ALLOYFS_OP_READ = 4,	/* nodeid = file, offset/size = range */
	/* --- stage 4: mutations. All are write-through: the syscall does not
	 * return until the daemon has acknowledged, so an acknowledged write
	 * is durable as far as the application is concerned.
	 */
	ALLOYFS_OP_CREATE = 5,	/* nodeid = parent, payload = create_in + name */
	ALLOYFS_OP_MKDIR = 6,	/* same shape as CREATE */
	ALLOYFS_OP_UNLINK = 7,	/* nodeid = parent, payload = name */
	ALLOYFS_OP_RMDIR = 8,	/* nodeid = parent, payload = name */
	ALLOYFS_OP_RENAME = 9,	/* nodeid = parent, payload = rename_in + names */
	ALLOYFS_OP_WRITE = 10,	/* nodeid = file, offset, payload = data */
	ALLOYFS_OP_SETATTR = 11,	/* nodeid = target, payload = setattr_in */
	/* --- stage 8: advisory locks, forwarded so they mean something
	 * BETWEEN machines. Whole-file only, because that is all the wire
	 * protocol carries; a byte range is coarsened to the whole file,
	 * which is safe (over-locking) rather than unsafe (under-locking).
	 */
	ALLOYFS_OP_LOCK = 12,	/* nodeid = file, payload = lock_in */
	ALLOYFS_OP_UNLOCK = 13,	/* nodeid = file */
	/* --- stage 9: links, and the last of the metadata ops. */
	ALLOYFS_OP_SYMLINK = 14,	/* nodeid = parent, payload = symlink_in */
	ALLOYFS_OP_READLINK = 15,	/* nodeid = link; reply is raw target bytes */
	ALLOYFS_OP_LINK = 16,	/* nodeid = parent, payload = link_in + name */
	ALLOYFS_OP_STATFS = 17,	/* no payload; reply = statfs_out */
};

/*
 * SYMLINK request payload, followed by the link's name then the target.
 *
 * The target is NOT a path the kernel validates: it is whatever text the
 * caller passed to symlink(2), commonly relative and possibly dangling. The
 * daemon decides whether where it lands is inside the export.
 */
struct alloyfs_symlink_in {
	__u16 namelen;
	__u16 targetlen;
	__u32 _pad;
};

/* LINK request payload, followed by the new name. */
struct alloyfs_link_in {
	__u64 target;		/* nodeid of the file gaining a name */
};

/* STATFS reply payload. */
struct alloyfs_statfs_out {
	__u64 blocks;
	__u64 blocks_free;
	__u32 block_size;
	__u32 _pad;
};

#define ALLOYFS_LOCK_SHARED 1
#define ALLOYFS_LOCK_EXCLUSIVE 2

struct alloyfs_lock_in {
	__u32 kind;		/* ALLOYFS_LOCK_* */
	__u32 wait;		/* non-zero = block until granted (F_SETLKW) */
};

/* CREATE / MKDIR request payload, followed by the name. */
struct alloyfs_create_in {
	__u32 mode;
	__u32 _pad;
};

/* RENAME request payload, followed by name then newname. */
struct alloyfs_rename_in {
	__u64 newparent;
	__u16 namelen;
	__u16 newnamelen;
	__u32 _pad;
};

#define ALLOYFS_SETATTR_MODE (1u << 0)
#define ALLOYFS_SETATTR_SIZE (1u << 1)
#define ALLOYFS_SETATTR_MTIME (1u << 2)

struct alloyfs_setattr_in {
	__u32 valid;		/* ALLOYFS_SETATTR_* */
	__u32 mode;
	__u64 size;
	__u64 mtime_ns;
};

/* WRITE reply payload. */
struct alloyfs_write_out {
	__u32 written;
	__u32 _pad;
};

struct alloyfs_attr {
	__u64 nodeid;
	__u64 size;
	__u64 mtime_ns;
	__u32 mode;		/* S_IF* | permission bits */
	__u32 nlink;
};

struct alloyfs_in_header {
	__u32 len;		/* header + payload */
	__u32 opcode;
	__u64 unique;
	__u64 nodeid;
	__u64 offset;
	__u32 size;
	__u32 _pad;
};

/*
 * A response, OR — when `unique` is 0 — an unsolicited notification from the
 * daemon, in which case `error` carries a alloyfs_notify_code and the payload
 * is a struct alloyfs_notify_entry. (FUSE uses the same unique==0 convention;
 * it keeps the header fixed-size and the write() path single.)
 */
struct alloyfs_out_header {
	__u32 len;		/* header + payload */
	__s32 error;		/* 0, a negative errno, or a notify code */
	__u64 unique;		/* 0 = notification */
};

enum alloyfs_notify_code {
	ALLOYFS_NOTIFY_CREATE = 1,	/* a name appeared in `parent` */
	ALLOYFS_NOTIFY_DELETE = 2,	/* a name went away */
	ALLOYFS_NOTIFY_MODIFY = 3,	/* contents changed; `size` is the new size */
	ALLOYFS_NOTIFY_ATTRIB = 4,	/* metadata changed */
	ALLOYFS_NOTIFY_RENAME = 5,	/* name moved: parent/name -> parent2/name2 */
};

#define ALLOYFS_NOTIFY_F_ISDIR (1u << 0)

/*
 * Notifications are addressed by parent nodeid + name, matching the rest of
 * the ABI (the daemon thinks in nodeids, not paths). The kernel resolves
 * them in its own dentry cache and skips anything not cached — an unwalked
 * subtree cannot be under a watch.
 */
struct alloyfs_notify_entry {
	__u64 parent;		/* nodeid of the containing directory */
	__u64 parent2;		/* RENAME: destination directory */
	__u64 size;		/* MODIFY: new size */
	__u32 flags;		/* ALLOYFS_NOTIFY_F_* */
	__u16 namelen;
	__u16 name2len;		/* RENAME: destination name length */
	/* namelen bytes, then name2len bytes */
};

/*
 * READDIR payload: a packed run of these, each followed by `namelen` bytes
 * of name, the whole entry padded up to 8 bytes. `off` is the cursor to
 * resume from AFTER this entry.
 */
struct alloyfs_dirent {
	__u64 nodeid;
	__u64 off;
	__u32 namelen;
	__u32 type;		/* DT_REG / DT_DIR */
};

#define ALLOYFS_DIRENT_ALIGN(x) (((x) + 7) & ~7ULL)
#define ALLOYFS_DIRENT_SIZE(namelen) \
	ALLOYFS_DIRENT_ALIGN(sizeof(struct alloyfs_dirent) + (namelen))

#endif /* _UAPI_DS_FS_H */
