/* SPDX-License-Identifier: GPL-2.0-only WITH Linux-syscall-note */
/*
 * ds-fs kernel <-> daemon ABI.
 *
 * Deliberately smaller than FUSE's: we control both ends and ship them
 * together, so this covers exactly the operations the filesystem needs and
 * nothing speculative. Every struct is fixed-size, naturally aligned, and
 * has an explicit length so the receiver never trusts a pointer.
 *
 * Flow: the daemon opens /dev/ds-fs, passes the fd to mount as `-o fd=N`,
 * then loops { read() a request; write() a response }. Requests carry a
 * `unique` the response must echo back; the kernel matches on it, so the
 * daemon may answer out of order.
 */
#ifndef _UAPI_DS_FS_H
#define _UAPI_DS_FS_H

#include <linux/types.h>

#define DSFS_ABI_VERSION 1
#define DSFS_ROOT_NODEID 1ULL

/* Largest payload either direction; bounds every kernel-side allocation. */
#define DSFS_MAX_PAYLOAD (128 * 1024)
#define DSFS_MAX_NAME 255

enum dsfs_opcode {
	DSFS_OP_LOOKUP = 1,	/* nodeid = parent dir, payload = name */
	DSFS_OP_GETATTR = 2,	/* nodeid = target */
	DSFS_OP_READDIR = 3,	/* nodeid = dir, offset = cursor */
	DSFS_OP_READ = 4,	/* nodeid = file, offset/size = range */
};

struct dsfs_attr {
	__u64 nodeid;
	__u64 size;
	__u64 mtime_ns;
	__u32 mode;		/* S_IF* | permission bits */
	__u32 nlink;
};

struct dsfs_in_header {
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
 * daemon, in which case `error` carries a dsfs_notify_code and the payload
 * is a struct dsfs_notify_entry. (FUSE uses the same unique==0 convention;
 * it keeps the header fixed-size and the write() path single.)
 */
struct dsfs_out_header {
	__u32 len;		/* header + payload */
	__s32 error;		/* 0, a negative errno, or a notify code */
	__u64 unique;		/* 0 = notification */
};

enum dsfs_notify_code {
	DSFS_NOTIFY_CREATE = 1,	/* a name appeared in `parent` */
	DSFS_NOTIFY_DELETE = 2,	/* a name went away */
	DSFS_NOTIFY_MODIFY = 3,	/* contents changed; `size` is the new size */
	DSFS_NOTIFY_ATTRIB = 4,	/* metadata changed */
	DSFS_NOTIFY_RENAME = 5,	/* name moved: parent/name -> parent2/name2 */
};

#define DSFS_NOTIFY_F_ISDIR (1u << 0)

/*
 * Notifications are addressed by parent nodeid + name, matching the rest of
 * the ABI (the daemon thinks in nodeids, not paths). The kernel resolves
 * them in its own dentry cache and skips anything not cached — an unwalked
 * subtree cannot be under a watch.
 */
struct dsfs_notify_entry {
	__u64 parent;		/* nodeid of the containing directory */
	__u64 parent2;		/* RENAME: destination directory */
	__u64 size;		/* MODIFY: new size */
	__u32 flags;		/* DSFS_NOTIFY_F_* */
	__u16 namelen;
	__u16 name2len;		/* RENAME: destination name length */
	/* namelen bytes, then name2len bytes */
};

/*
 * READDIR payload: a packed run of these, each followed by `namelen` bytes
 * of name, the whole entry padded up to 8 bytes. `off` is the cursor to
 * resume from AFTER this entry.
 */
struct dsfs_dirent {
	__u64 nodeid;
	__u64 off;
	__u32 namelen;
	__u32 type;		/* DT_REG / DT_DIR */
};

#define DSFS_DIRENT_ALIGN(x) (((x) + 7) & ~7ULL)
#define DSFS_DIRENT_SIZE(namelen) \
	DSFS_DIRENT_ALIGN(sizeof(struct dsfs_dirent) + (namelen))

#endif /* _UAPI_DS_FS_H */
