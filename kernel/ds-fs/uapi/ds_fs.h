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

struct dsfs_out_header {
	__u32 len;		/* header + payload */
	__s32 error;		/* 0 or a negative errno */
	__u64 unique;
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
