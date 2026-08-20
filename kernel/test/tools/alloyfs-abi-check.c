// SPDX-License-Identifier: GPL-2.0-only
/*
 * alloyfs-abi-check — compile-time proof that the Rust daemon and the C header
 * describe the same wire format.
 *
 * `crates/alloyfs-mount-kernel/src/abi.rs` encodes every struct by hand from
 * byte offsets, so it cannot notice a change to alloyfs.h the way a C caller
 * would. These asserts are the missing link: the numbers below are copied
 * from abi.rs, so editing the header without editing abi.rs stops the test
 * build with a message naming the struct that moved.
 *
 * Nothing runs it — building it IS the test. mkinitramfs.sh compiles it on
 * every run.
 */
#include <stddef.h>

#include "../../alloyfs/uapi/alloyfs.h"

#define SAME_SIZE(type, rust_len)                                              \
	_Static_assert(sizeof(type) == (rust_len),                             \
		       #type " changed size — update abi.rs (" #rust_len ")")

#define SAME_OFFSET(type, field, rust_off)                                     \
	_Static_assert(offsetof(type, field) == (rust_off),                    \
		       #type "." #field " moved — update abi.rs")

/* --- sizes, against the *_LEN constants in abi.rs ------------------------- */
SAME_SIZE(struct alloyfs_in_header, 40);	/* IN_HEADER_LEN */
SAME_SIZE(struct alloyfs_out_header, 16);	/* OUT_HEADER_LEN */
SAME_SIZE(struct alloyfs_attr, 32);		/* ATTR_LEN */
SAME_SIZE(struct alloyfs_create_in, 8);		/* CREATE_IN_LEN */
SAME_SIZE(struct alloyfs_rename_in, 16);	/* RENAME_IN_LEN */
SAME_SIZE(struct alloyfs_setattr_in, 24);	/* SETATTR_IN_LEN */
SAME_SIZE(struct alloyfs_write_out, 8);		/* WRITE_OUT_LEN */
SAME_SIZE(struct alloyfs_notify_entry, 32);	/* NOTIFY_ENTRY_LEN */
SAME_SIZE(struct alloyfs_dirent, 24);		/* DIRENT_LEN */
SAME_SIZE(struct alloyfs_lock_in, 32);		/* LOCK_IN_LEN */
SAME_SIZE(struct alloyfs_unlock_in, 24);	/* UNLOCK_IN_LEN */
SAME_SIZE(struct alloyfs_getlk_out, 24);	/* GETLK_OUT_LEN */
SAME_SIZE(struct alloyfs_symlink_in, 8);	/* SYMLINK_IN_LEN */
SAME_SIZE(struct alloyfs_link_in, 8);		/* LINK_IN_LEN */
SAME_SIZE(struct alloyfs_statfs_out, 24);	/* STATFS_OUT_LEN */

/* --- field offsets, against the u32_at()/u64_at() calls in abi.rs --------- */
SAME_OFFSET(struct alloyfs_in_header, len, 0);
SAME_OFFSET(struct alloyfs_in_header, opcode, 4);
SAME_OFFSET(struct alloyfs_in_header, unique, 8);
SAME_OFFSET(struct alloyfs_in_header, nodeid, 16);
SAME_OFFSET(struct alloyfs_in_header, offset, 24);
SAME_OFFSET(struct alloyfs_in_header, size, 32);

SAME_OFFSET(struct alloyfs_out_header, len, 0);
SAME_OFFSET(struct alloyfs_out_header, error, 4);
SAME_OFFSET(struct alloyfs_out_header, unique, 8);

SAME_OFFSET(struct alloyfs_attr, nodeid, 0);
SAME_OFFSET(struct alloyfs_attr, size, 8);
SAME_OFFSET(struct alloyfs_attr, mtime_ns, 16);
SAME_OFFSET(struct alloyfs_attr, mode, 24);
SAME_OFFSET(struct alloyfs_attr, nlink, 28);

SAME_OFFSET(struct alloyfs_rename_in, newparent, 0);
SAME_OFFSET(struct alloyfs_rename_in, namelen, 8);
SAME_OFFSET(struct alloyfs_rename_in, newnamelen, 10);

SAME_OFFSET(struct alloyfs_setattr_in, valid, 0);
SAME_OFFSET(struct alloyfs_setattr_in, mode, 4);
SAME_OFFSET(struct alloyfs_setattr_in, size, 8);
SAME_OFFSET(struct alloyfs_setattr_in, mtime_ns, 16);

SAME_OFFSET(struct alloyfs_notify_entry, parent, 0);
SAME_OFFSET(struct alloyfs_notify_entry, parent2, 8);
SAME_OFFSET(struct alloyfs_notify_entry, size, 16);
SAME_OFFSET(struct alloyfs_notify_entry, flags, 24);
SAME_OFFSET(struct alloyfs_notify_entry, namelen, 28);
SAME_OFFSET(struct alloyfs_notify_entry, name2len, 30);

SAME_OFFSET(struct alloyfs_dirent, nodeid, 0);
SAME_OFFSET(struct alloyfs_dirent, off, 8);
SAME_OFFSET(struct alloyfs_dirent, namelen, 16);
SAME_OFFSET(struct alloyfs_dirent, type, 20);

SAME_OFFSET(struct alloyfs_lock_in, kind, 0);
SAME_OFFSET(struct alloyfs_lock_in, wait, 4);
SAME_OFFSET(struct alloyfs_lock_in, owner, 8);
SAME_OFFSET(struct alloyfs_lock_in, start, 16);
SAME_OFFSET(struct alloyfs_lock_in, len, 24);

SAME_OFFSET(struct alloyfs_unlock_in, owner, 0);
SAME_OFFSET(struct alloyfs_unlock_in, start, 8);
SAME_OFFSET(struct alloyfs_unlock_in, len, 16);

SAME_OFFSET(struct alloyfs_getlk_out, start, 0);
SAME_OFFSET(struct alloyfs_getlk_out, len, 8);
SAME_OFFSET(struct alloyfs_getlk_out, kind, 16);
SAME_OFFSET(struct alloyfs_getlk_out, pid, 20);

SAME_OFFSET(struct alloyfs_symlink_in, namelen, 0);
SAME_OFFSET(struct alloyfs_symlink_in, targetlen, 2);

SAME_OFFSET(struct alloyfs_link_in, target, 0);

SAME_OFFSET(struct alloyfs_statfs_out, blocks, 0);
SAME_OFFSET(struct alloyfs_statfs_out, blocks_free, 8);
SAME_OFFSET(struct alloyfs_statfs_out, block_size, 16);

/* --- constants ----------------------------------------------------------- */
_Static_assert(ALLOYFS_MAX_PAYLOAD == 128 * 1024, "MAX_PAYLOAD changed");
_Static_assert(ALLOYFS_MAX_NAME == 255, "MAX_NAME changed");
_Static_assert(ALLOYFS_ROOT_NODEID == 1, "ROOT_NODEID changed");
_Static_assert(ALLOYFS_OP_SETATTR == 11, "opcode numbering changed");
_Static_assert(ALLOYFS_OP_UNLOCK == 13, "opcode numbering changed");
_Static_assert(ALLOYFS_LOCK_EXCLUSIVE == 2, "lock kind numbering changed");
_Static_assert(ALLOYFS_OP_STATFS == 17, "opcode numbering changed");
_Static_assert(ALLOYFS_OP_GETLK == 18, "opcode numbering changed");
_Static_assert(ALLOYFS_ABI_VERSION == 4, "ABI version changed — check both ends");
_Static_assert(ALLOYFS_NOTIFY_RENAME == 5, "notify code numbering changed");
_Static_assert(ALLOYFS_NOTIFY_F_ISDIR == 1, "notify flags changed");

int main(void)
{
	return 0;
}
