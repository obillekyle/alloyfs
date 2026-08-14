// SPDX-License-Identifier: GPL-2.0-only
/*
 * The kernel <-> daemon transport: /dev/ds-fs.
 *
 * A filesystem operation builds a request, queues it, and sleeps. The daemon
 * read()s the request, does the remote work, and write()s a response tagged
 * with the same `unique`; that wakes the sleeper. Responses may come back in
 * any order, so a slow read can never block a fast getattr behind it.
 *
 * Two rules govern everything here:
 *   - Never trust the daemon. Every length, unique and payload size from
 *     userspace is bounds-checked before it is used, because a buggy or
 *     hostile daemon must not be able to panic the kernel.
 *   - Never leave a sleeper stranded. Daemon death, unmount, and a fatal
 *     signal all resolve every outstanding request, or the mount deadlocks.
 */
#define pr_fmt(fmt) "dsfs: " fmt

#include <linux/file.h>		/* fget/fput */
#include <linux/fs.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/poll.h>
#include <linux/sched/signal.h>
#include <linux/slab.h>
#include <linux/uaccess.h>

#include "dsfs.h"

static void dsfs_req_free(struct dsfs_req *req)
{
	kvfree(req->in_buf);
	kvfree(req->out_buf);
	kfree(req);
}

struct dsfs_conn *dsfs_conn_get(struct dsfs_conn *conn)
{
	refcount_inc(&conn->refs);
	return conn;
}

void dsfs_conn_put(struct dsfs_conn *conn)
{
	if (refcount_dec_and_test(&conn->refs))
		kfree(conn);
}

/* Fail every queued and in-flight request. Called when the daemon closes
 * its fd, and on unmount.
 */
static void dsfs_conn_abort(struct dsfs_conn *conn)
{
	struct dsfs_req *req, *tmp;
	LIST_HEAD(dead);

	spin_lock(&conn->lock);
	conn->connected = false;
	list_splice_init(&conn->pending, &dead);
	list_splice_init(&conn->processing, &dead);
	spin_unlock(&conn->lock);

	list_for_each_entry_safe(req, tmp, &dead, list) {
		list_del_init(&req->list);
		req->error = -ECONNABORTED;
		req->finished = true;
		complete(&req->done);
	}
	wake_up_all(&conn->waitq);
}

/*
 * Send one request and wait for its answer.
 *
 * `out_buf`/`out_max` receive the response payload (may be NULL/0 for
 * operations that answer with a header only). Returns the payload length on
 * success, or a negative errno — including the daemon's own error.
 */
int dsfs_request(struct dsfs_conn *conn, u32 opcode, u64 nodeid, u64 offset,
		 u32 size, const void *in_payload, u32 in_len,
		 void *out_buf, u32 out_max)
{
	struct dsfs_req *req;
	int ret;

	if (in_len > DSFS_MAX_PAYLOAD || out_max > DSFS_MAX_PAYLOAD)
		return -EINVAL;

	req = kzalloc(sizeof(*req), GFP_KERNEL);
	if (!req)
		return -ENOMEM;
	INIT_LIST_HEAD(&req->list);
	init_completion(&req->done);
	req->hdr.len = sizeof(req->hdr) + in_len;
	req->hdr.opcode = opcode;
	req->hdr.nodeid = nodeid;
	req->hdr.offset = offset;
	req->hdr.size = size;
	req->in_len = in_len;
	if (in_len) {
		/* kvmalloc: a 128 KiB write payload is an order-5 allocation,
		 * which kmalloc can fail under fragmentation.
		 */
		req->in_buf = kvmalloc(in_len, GFP_KERNEL);
		if (!req->in_buf) {
			kfree(req);
			return -ENOMEM;
		}
		memcpy(req->in_buf, in_payload, in_len);
	}
	req->out_max = out_max;
	if (out_max) {
		req->out_buf = kvzalloc(out_max, GFP_KERNEL);
		if (!req->out_buf) {
			dsfs_req_free(req);
			return -ENOMEM;
		}
	}

	spin_lock(&conn->lock);
	if (!conn->connected) {
		spin_unlock(&conn->lock);
		dsfs_req_free(req);
		return -ECONNABORTED;
	}
	req->hdr.unique = ++conn->next_unique;
	list_add_tail(&req->list, &conn->pending);
	spin_unlock(&conn->lock);
	wake_up_interruptible(&conn->waitq);

	/* Killable, not interruptible: a stray SIGWINCH must not tear down a
	 * filesystem operation, but ^C on a hung mount has to work.
	 */
	ret = wait_for_completion_killable(&req->done);
	if (ret) {
		spin_lock(&conn->lock);
		if (!req->finished) {
			list_del_init(&req->list);
			req->error = -EINTR;
		}
		spin_unlock(&conn->lock);
		/* If the daemon completed it concurrently we still own the
		 * request here; either way nobody else will touch it.
		 */
		wait_for_completion(&req->done);
	}

	ret = req->error ? req->error : (int)req->out_len;
	if (ret > 0 && out_buf)
		memcpy(out_buf, req->out_buf, req->out_len);
	dsfs_req_free(req);
	return ret;
}

/* ------------------------------------------------------------- char device */

static int dsfs_dev_open(struct inode *inode, struct file *file)
{
	struct dsfs_conn *conn = kzalloc(sizeof(*conn), GFP_KERNEL);

	if (!conn)
		return -ENOMEM;
	spin_lock_init(&conn->lock);
	INIT_LIST_HEAD(&conn->pending);
	INIT_LIST_HEAD(&conn->processing);
	init_waitqueue_head(&conn->waitq);
	refcount_set(&conn->refs, 1);
	conn->connected = true;
	file->private_data = conn;
	return 0;
}

static int dsfs_dev_release(struct inode *inode, struct file *file)
{
	struct dsfs_conn *conn = file->private_data;

	dsfs_conn_abort(conn);
	dsfs_conn_put(conn);
	return 0;
}

/* One request per read(). Short buffers are an error rather than a partial
 * transfer: the daemon must present a buffer big enough for any request.
 */
static ssize_t dsfs_dev_read(struct file *file, char __user *ubuf, size_t count,
			     loff_t *ppos)
{
	struct dsfs_conn *conn = file->private_data;
	struct dsfs_req *req;
	size_t total;
	int err;

	/* The daemon must present a buffer able to hold ANY request, so a big
	 * write never has to be split across reads.
	 */
	if (count < sizeof(struct dsfs_in_header) + DSFS_MAX_PAYLOAD)
		return -EINVAL;

	for (;;) {
		spin_lock(&conn->lock);
		if (!conn->connected) {
			spin_unlock(&conn->lock);
			return -ENODEV;
		}
		req = list_first_entry_or_null(&conn->pending, struct dsfs_req, list);
		if (req) {
			list_move_tail(&req->list, &conn->processing);
			spin_unlock(&conn->lock);
			break;
		}
		spin_unlock(&conn->lock);

		if (file->f_flags & O_NONBLOCK)
			return -EAGAIN;
		err = wait_event_interruptible(conn->waitq,
					       !list_empty(&conn->pending) ||
					       !conn->connected);
		if (err)
			return err;
	}

	total = sizeof(req->hdr) + req->in_len;
	if (copy_to_user(ubuf, &req->hdr, sizeof(req->hdr)) ||
	    (req->in_len &&
	     copy_to_user(ubuf + sizeof(req->hdr), req->in_buf, req->in_len))) {
		/* The daemon never saw it: requeue so it is not lost. */
		spin_lock(&conn->lock);
		if (!req->finished)
			list_move(&req->list, &conn->pending);
		spin_unlock(&conn->lock);
		return -EFAULT;
	}
	return total;
}

/* One response per write(). */
static ssize_t dsfs_dev_write(struct file *file, const char __user *ubuf,
			      size_t count, loff_t *ppos)
{
	struct dsfs_conn *conn = file->private_data;
	struct dsfs_out_header hdr;
	struct dsfs_req *req, *found = NULL;
	u32 payload;

	if (count < sizeof(hdr))
		return -EINVAL;
	if (copy_from_user(&hdr, ubuf, sizeof(hdr)))
		return -EFAULT;

	/* Everything below is daemon-supplied: validate before believing. */
	if (hdr.len < sizeof(hdr) || hdr.len > count ||
	    hdr.len > sizeof(hdr) + DSFS_MAX_PAYLOAD)
		return -EINVAL;
	payload = hdr.len - sizeof(hdr);

	/* unique == 0 is an unsolicited notification, not a reply: `error`
	 * carries the notify code. This is the path that makes remote changes
	 * visible to inotify — the reason the module exists.
	 */
	if (hdr.unique == 0) {
		void *buf;
		int ret;

		if (payload > DSFS_NOTIFY_MAX)
			return -EINVAL;
		buf = memdup_user(ubuf + sizeof(hdr), payload);
		if (IS_ERR(buf))
			return PTR_ERR(buf);
		ret = dsfs_notify_from_daemon(hdr.error, buf, payload);
		kfree(buf);
		return ret ? ret : (ssize_t)hdr.len;
	}

	spin_lock(&conn->lock);
	list_for_each_entry(req, &conn->processing, list) {
		if (req->hdr.unique == hdr.unique) {
			found = req;
			list_del_init(&found->list);
			break;
		}
	}
	spin_unlock(&conn->lock);
	if (!found)
		return -ENOENT;	/* unknown or already-abandoned request */

	if (payload > found->out_max)
		payload = found->out_max;	/* truncate, never overflow */
	if (payload && copy_from_user(found->out_buf, ubuf + sizeof(hdr), payload)) {
		found->error = -EIO;
	} else {
		found->error = hdr.error;
		found->out_len = payload;
	}
	found->finished = true;
	complete(&found->done);
	return hdr.len;
}

static __poll_t dsfs_dev_poll(struct file *file, poll_table *wait)
{
	struct dsfs_conn *conn = file->private_data;
	__poll_t mask = EPOLLOUT | EPOLLWRNORM;

	poll_wait(file, &conn->waitq, wait);
	spin_lock(&conn->lock);
	if (!list_empty(&conn->pending))
		mask |= EPOLLIN | EPOLLRDNORM;
	if (!conn->connected)
		mask |= EPOLLERR;
	spin_unlock(&conn->lock);
	return mask;
}

static const struct file_operations dsfs_dev_fops = {
	.owner = THIS_MODULE,
	.open = dsfs_dev_open,
	.release = dsfs_dev_release,
	.read = dsfs_dev_read,
	.write = dsfs_dev_write,
	.poll = dsfs_dev_poll,
	.llseek = noop_llseek,
};

static struct miscdevice dsfs_miscdev = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "ds-fs",
	.fops = &dsfs_dev_fops,
	.mode = 0600,
};

/* Claim the connection behind `fd`, verifying it is really our device. */
struct dsfs_conn *dsfs_conn_from_fd(unsigned int fd)
{
	struct file *file = fget(fd);
	struct dsfs_conn *conn;

	if (!file)
		return NULL;
	if (file->f_op != &dsfs_dev_fops) {
		fput(file);
		return NULL;
	}
	conn = dsfs_conn_get(file->private_data);
	fput(file);
	return conn;
}

void dsfs_conn_shutdown(struct dsfs_conn *conn)
{
	dsfs_conn_abort(conn);
}

int dsfs_conn_init(void)
{
	return misc_register(&dsfs_miscdev);
}

void dsfs_conn_exit(void)
{
	misc_deregister(&dsfs_miscdev);
}
