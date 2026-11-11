/* SPDX-License-Identifier: GPL-3.0-or-later */
#include "includes.h"
#include "lib/util/sys_rw.h"
#include "lib/util/tevent_unix.h"
#include "smbd/globals.h"
#include "smbd/smbd.h"
#include "tier_client.h"
#include <dirent.h>

struct tier_config {
	struct tier_client *client;
	uint64_t root, cwd;
	char *cwd_name;
};
static int next_fd = 1000;
struct tier_fh {
	uint64_t handle;
	int flags;
	off_t offset;
	bool metadata;
};
struct tier_dir {
	struct tier_config *config;
	struct files_struct *fsp;
	uint64_t handle, cursor;
	TALLOC_CTX *page_ctx;
	TierResponse *page;
	size_t index;
	bool eof;
	int error;
	struct dirent entry;
};
static struct tier_config *config(struct vfs_handle_struct *h)
{
	return talloc_get_type_abort(h->data, struct tier_config);
}
static uint64_t fh(struct vfs_handle_struct *h, const struct files_struct *fsp)
{
	struct tier_fh *f = VFS_FETCH_FSP_EXTENSION(h, fsp);
	if (!f) {
		errno = EBADF;
		return 0;
	}
	return f->handle;
}
static uint64_t parent(struct vfs_handle_struct *h, const struct files_struct *fsp)
{
	if (!fsp || fsp_get_pathref_fd(fsp) == AT_FDCWD)
		return config(h)->cwd;
	return fh(h, fsp);
}
static struct timespec timestamp(TierTimestamp *t)
{
	struct timespec ts = {0};
	if (t) {
		ts.tv_sec = t->seconds;
		ts.tv_nsec = t->nanos;
	}
	return ts;
}
static int attributes(TierAttr *a, SMB_STRUCT_STAT *s)
{
	if (!a || a->inode == 0) {
		errno = EPROTO;
		return -1;
	}
	ZERO_STRUCTP(s);
	s->st_ex_dev = 0x54494552;
	s->st_ex_ino = a->inode;
	s->st_ex_mode = (a->directory ? S_IFDIR : S_IFREG) | (a->mode & 07777);
	s->st_ex_nlink = a->nlink;
	s->st_ex_uid = a->uid;
	s->st_ex_gid = a->gid;
	s->st_ex_size = a->size;
	s->st_ex_blksize = 4096;
	s->st_ex_blocks = (a->allocated + 511) / 512;
	s->st_ex_atime = timestamp(a->atime);
	s->st_ex_mtime = timestamp(a->mtime);
	s->st_ex_ctime = timestamp(a->ctime);
	s->st_ex_btime = timestamp(a->btime);
	s->cached_dos_attributes =
	    a->directory ? FILE_ATTRIBUTE_DIRECTORY : FILE_ATTRIBUTE_NORMAL;
	return 0;
}
static int rpc_status(struct vfs_handle_struct *h, TierRequest *r)
{
	TALLOC_CTX *tmp = talloc_new(h);
	int ret = tier_rpc(tmp, config(h)->client, r) ? 0 : -1;
	int error = errno;
	TALLOC_FREE(tmp);
	errno = error;
	return ret;
}
static bool no_stream(const struct smb_filename *name)
{
	if (name->stream_name) {
		errno = ENOTSUP;
		return false;
	}
	return true;
}
static int tier_connect(struct vfs_handle_struct *h, const char *service,
			const char *user)
{
	struct tier_config *c;
	TierRequest r = {0};
	TierResponse *out;
	TALLOC_CTX *tmp;
	const char *path =
	    lp_parm_const_string(SNUM(h->conn), "tier", "socket", "/run/tier/tier.sock");
	/* Kernel locking cannot operate on Tier's virtual descriptors. Samba's
	 * own share-mode and byte-range lock databases remain authoritative. */
	if (lp_kernel_share_modes(SNUM(h->conn)) || lp_posix_locking(h->conn->params)) {
		DBG_ERR("tier requires kernel share modes = no and posix locking = no\n");
		errno = EINVAL;
		return -1;
	}
	c = talloc_zero(h, struct tier_config);
	if (!c) {
		errno = ENOMEM;
		return -1;
	}
	h->data = c;
	c->cwd_name = talloc_strdup(c, "/");
	become_root();
	c->client =
	    tier_client_connect(c, h->conn->sconn->ev_ctx, path, h->conn->sconn->pool);
	{
		int saved = errno;
		unbecome_root();
		errno = saved;
	}
	if (!c->client)
		return -1;
	tmp = talloc_new(c);
	r.method = TIER_METHOD_ATTACH;
	r.name = h->conn->connectpath;
	out = tier_rpc(tmp, c->client, &r);
	if (!out) {
		TALLOC_FREE(tmp);
		return -1;
	}
	c->root = c->cwd = out->handle;
	TALLOC_FREE(tmp);
	return 0;
}
static void tier_disconnect(struct vfs_handle_struct *h) { TALLOC_FREE(h->data); }
static int tier_fstat(struct vfs_handle_struct *h, struct files_struct *fsp,
		      SMB_STRUCT_STAT *s)
{
	TALLOC_CTX *tmp = talloc_new(h);
	TierRequest r = {0};
	TierResponse *out;
	int ret = -1;
	r.method = TIER_METHOD_GETATTR;
	r.handle = fh(h, fsp);
	if (r.handle && (out = tier_rpc(tmp, config(h)->client, &r)))
		ret = attributes(out->attr, s);
	{
		int saved = errno;
		TALLOC_FREE(tmp);
		errno = saved;
	}
	return ret;
}
static int tier_fstatat(struct vfs_handle_struct *h, const struct files_struct *dir,
			const struct smb_filename *name, SMB_STRUCT_STAT *s, int flags)
{
	TALLOC_CTX *tmp = talloc_new(h);
	TierRequest r = {0};
	TierResponse *out;
	int ret = -1;
	if (!no_stream(name))
		goto done;
	if (flags & ~AT_SYMLINK_NOFOLLOW) {
		errno = ENOTSUP;
		goto done;
	}
	r.method = TIER_METHOD_STAT_AT;
	r.parent = parent(h, dir);
	r.name = name->base_name;
	if (r.parent && (out = tier_rpc(tmp, config(h)->client, &r)))
		ret = attributes(out->attr, s);
done: {
	int saved = errno;
	TALLOC_FREE(tmp);
	errno = saved;
}
	return ret;
}
static int tier_stat(struct vfs_handle_struct *h, struct smb_filename *name)
{
	return tier_fstatat(h, NULL, name, &name->st, 0);
}
static int tier_openat(struct vfs_handle_struct *h, const struct files_struct *dir,
		       const struct smb_filename *name, struct files_struct *fsp,
		       const struct vfs_open_how *how)
{
	struct tier_config *c = config(h);
	TALLOC_CTX *tmp = talloc_new(h);
	TierRequest r = {0};
	TierResponse *out;
	struct tier_fh *f;
	int ret = -1;
	int descriptor;
	if (next_fd == INT_MAX) {
		errno = EMFILE;
		goto done;
	}
	descriptor = next_fd++;
	if (!no_stream(name))
		goto done;
	if (how->resolve &
	    ~(VFS_OPEN_HOW_WITH_BACKUP_INTENT | VFS_OPEN_HOW_RESOLVE_NO_SYMLINKS)) {
		errno = ENOTSUP;
		goto done;
	}
	r.method = TIER_METHOD_OPEN;
	r.parent = parent(h, dir);
	r.name = name->base_name;
	if (!r.parent)
		goto done;
	if (fsp->fsp_flags.is_pathref || (how->flags & O_PATH) ||
	    (fsp->access_mask != 0 &&
	     !(fsp->access_mask &
	       (SEC_FILE_READ_DATA | SEC_FILE_WRITE_DATA | SEC_FILE_APPEND_DATA)) &&
	     !(how->flags & O_TRUNC)))
		r.access = TIER_ACCESS_METADATA;
	else
		switch (how->flags & O_ACCMODE) {
		case O_RDONLY:
			r.access = TIER_ACCESS_READ_ONLY;
			break;
		case O_WRONLY:
			r.access = TIER_ACCESS_WRITE_ONLY;
			break;
		case O_RDWR:
			r.access = TIER_ACCESS_READ_WRITE;
			break;
		default:
			errno = EINVAL;
			goto done;
		}
	/* A data-read-only handle may still request mutable metadata rights.
	 * Those handles must stay registered with Tier for later SETATTR calls. */
	r.local_access = r.access == TIER_ACCESS_READ_ONLY &&
			 !(fsp->access_mask &
			   (SEC_FILE_WRITE_ATTRIBUTE | SEC_FILE_WRITE_EA |
			    SEC_STD_WRITE_DAC | SEC_STD_WRITE_OWNER | SEC_STD_DELETE));
	r.directory = (how->flags & O_DIRECTORY) != 0;
	r.truncate = (how->flags & O_TRUNC) != 0;
	if (how->flags & O_CREAT)
		r.disposition = (how->flags & O_EXCL) ? TIER_DISPOSITION_CREATE_NEW
						      : TIER_DISPOSITION_OPEN_OR_CREATE;
	out = tier_rpc(tmp, c->client, &r);
	if (!out)
		goto done;
	f = VFS_ADD_FSP_EXTENSION(h, fsp, struct tier_fh, NULL);
	if (!f) {
		r.method = TIER_METHOD_CLOSE;
		r.handle = out->handle;
		rpc_status(h, &r);
		errno = ENOMEM;
		goto done;
	}
	f->handle = out->handle;
	f->flags = how->flags;
	f->metadata = r.access == TIER_ACCESS_METADATA;
	fsp->fsp_flags.have_proc_fds = false;
	ret = descriptor;
done: {
	int saved = errno;
	TALLOC_FREE(tmp);
	errno = saved;
}
	return ret;
}
static int tier_close(struct vfs_handle_struct *h, struct files_struct *fsp)
{
	TierRequest r = {0};
	struct tier_fh *f = VFS_FETCH_FSP_EXTENSION(h, fsp);
	int ret;
	r.method = TIER_METHOD_CLOSE;
	r.handle = fh(h, fsp);
	if (!r.handle)
		return -1;
	/* Metadata handles own no file or durability work; only their daemon
	 * bookkeeping may outlive this fsp. Data-file CLOSE remains synchronous. */
	ret = f->metadata ? tier_defer_close(config(h)->client, r.handle)
			  : rpc_status(h, &r);
	VFS_REMOVE_FSP_EXTENSION(h, fsp);
	return ret;
}
static int tier_mkdirat(struct vfs_handle_struct *h, struct files_struct *dir,
			const struct smb_filename *name, mode_t mode)
{
	TierRequest r = {0};
	if (!no_stream(name))
		return -1;
	r.method = TIER_METHOD_MKDIR;
	r.parent = parent(h, dir);
	r.name = name->base_name;
	return r.parent ? rpc_status(h, &r) : -1;
}
static int tier_unlinkat(struct vfs_handle_struct *h, struct files_struct *dir,
			 const struct smb_filename *name, int flags)
{
	TierRequest r = {0};
	if (!no_stream(name))
		return -1;
	if (flags & ~AT_REMOVEDIR) {
		errno = EINVAL;
		return -1;
	}
	r.method = TIER_METHOD_UNLINK;
	r.parent = parent(h, dir);
	r.name = name->base_name;
	r.directory = (flags & AT_REMOVEDIR) != 0;
	return r.parent ? rpc_status(h, &r) : -1;
}
static int tier_renameat(struct vfs_handle_struct *h, struct files_struct *srcdir,
			 const struct smb_filename *src, struct files_struct *dstdir,
			 const struct smb_filename *dst, const struct vfs_rename_how *how)
{
	TierRequest r = {0};
	if (!no_stream(src) || !no_stream(dst))
		return -1;
	if (how->flags & ~VFS_RENAME_HOW_NO_REPLACE) {
		errno = ENOTSUP;
		return -1;
	}
	r.method = TIER_METHOD_RENAME;
	r.parent = parent(h, srcdir);
	r.target_parent = parent(h, dstdir);
	r.name = src->base_name;
	r.target_name = dst->base_name;
	r.replace = !(how->flags & VFS_RENAME_HOW_NO_REPLACE);
	return r.parent && r.target_parent ? rpc_status(h, &r) : -1;
}
static int tier_setattr(struct vfs_handle_struct *h, struct files_struct *fsp,
			TierChanges *changes)
{
	TierRequest r = {0};
	r.method = TIER_METHOD_SETATTR;
	r.handle = fh(h, fsp);
	r.changes = changes;
	return r.handle ? rpc_status(h, &r) : -1;
}
static int tier_ftruncate(struct vfs_handle_struct *h, struct files_struct *fsp,
			  off_t size)
{
	TierChanges changes = {0};
	if (size < 0) {
		errno = EINVAL;
		return -1;
	}
	changes.has_size = true;
	changes.size = size;
	return tier_setattr(h, fsp, &changes);
}
static int tier_fchmod(struct vfs_handle_struct *h, struct files_struct *fsp, mode_t mode)
{
	TierChanges changes = {0};
	changes.has_mode = true;
	changes.mode = mode;
	return tier_setattr(h, fsp, &changes);
}
static int tier_fntimes(struct vfs_handle_struct *h, struct files_struct *fsp,
			struct smb_file_time *ft)
{
	TierChanges c = {0};
	TierTimestamp at = {0}, mt = {0};
	TierTimestamp bt = {0};
	if (!is_omit_timespec(&ft->atime)) {
		at.seconds = ft->atime.tv_sec;
		at.nanos = ft->atime.tv_nsec;
		c.atime = &at;
	}
	if (!is_omit_timespec(&ft->mtime)) {
		mt.seconds = ft->mtime.tv_sec;
		mt.nanos = ft->mtime.tv_nsec;
		c.mtime = &mt;
	}
	if (!is_omit_timespec(&ft->create_time)) {
		bt.seconds = ft->create_time.tv_sec;
		bt.nanos = ft->create_time.tv_nsec;
		c.btime = &bt;
	}
	return tier_setattr(h, fsp, &c);
}

struct tier_io {
	struct tier_call *call;
	void *read_buffer;
	size_t length;
	bool read, sync;
};
static struct tevent_req *tier_io_send(struct vfs_handle_struct *h, TALLOC_CTX *ctx,
				       struct tevent_context *ev,
				       struct files_struct *fsp, void *data, size_t n,
				       off_t offset, TierMethod method)
{
	struct tier_io *state;
	struct tevent_req *req = tevent_req_create(ctx, &state, struct tier_io);
	TierRequest r = {0};
	if (!req)
		return NULL;
	if (offset < 0 || n > tier_max_data(config(h)->client)) {
		tevent_req_error(req, EINVAL);
		return tevent_req_post(req, ev);
	}
	r.method = method;
	r.handle = fh(h, fsp);
	r.offset = offset;
	r.length = n;
	{
		struct tier_fh *f = VFS_FETCH_FSP_EXTENSION(h, fsp);
		r.stable = f && (f->flags & (O_SYNC | O_DSYNC));
	}
	if (!r.handle) {
		tevent_req_error(req, errno);
		return tevent_req_post(req, ev);
	}
	state->read = method == TIER_METHOD_READ;
	state->sync = method == TIER_METHOD_SYNC;
	state->read_buffer = data;
	state->length = n;
	state->call = tier_submit(state, config(h)->client, &r,
				  method == TIER_METHOD_WRITE ? data : NULL,
				  method == TIER_METHOD_WRITE ? n : 0, req);
	if (!state->call) {
		tevent_req_error(req, errno);
		return tevent_req_post(req, ev);
	}
	return req;
}
static ssize_t tier_io_recv(struct tevent_req *req, struct vfs_aio_state *aio)
{
	struct tier_io *state = tevent_req_data(req, struct tier_io);
	TierResponse *out;
	void *data;
	size_t length;
	ZERO_STRUCTP(aio);
	if (tevent_req_is_unix_error(req, &aio->error))
		return -1;
	out = tier_result(state->call, &data, &length);
	if (!out) {
		aio->error = errno;
		return -1;
	}
	if (out->count > state->length || (state->read && length != out->count) ||
	    (!state->read && length)) {
		aio->error = EPROTO;
		return -1;
	}
	if (state->read && length)
		memcpy(state->read_buffer, data, length);
	return state->sync ? 0 : out->count;
}
static struct tevent_req *tier_pread_send(struct vfs_handle_struct *h, TALLOC_CTX *ctx,
					  struct tevent_context *ev,
					  struct files_struct *fsp, void *data, size_t n,
					  off_t offset)
{
	return tier_io_send(h, ctx, ev, fsp, data, n, offset, TIER_METHOD_READ);
}
static struct tevent_req *tier_pwrite_send(struct vfs_handle_struct *h, TALLOC_CTX *ctx,
					   struct tevent_context *ev,
					   struct files_struct *fsp, const void *data,
					   size_t n, off_t offset)
{
	return tier_io_send(h, ctx, ev, fsp, discard_const_p(void, data), n, offset,
			    TIER_METHOD_WRITE);
}
static struct tevent_req *tier_fsync_send(struct vfs_handle_struct *h, TALLOC_CTX *ctx,
					  struct tevent_context *ev,
					  struct files_struct *fsp)
{
	return tier_io_send(h, ctx, ev, fsp, NULL, 0, 0, TIER_METHOD_SYNC);
}
static int tier_fsync_recv(struct tevent_req *req, struct vfs_aio_state *aio)
{
	return tier_io_recv(req, aio);
}
static ssize_t tier_io_sync(struct vfs_handle_struct *h, struct files_struct *fsp,
			    void *data, size_t n, off_t offset, bool write)
{
	TALLOC_CTX *tmp = talloc_new(h);
	TierRequest r = {0};
	TierResponse *out;
	struct tier_call *call;
	void *bytes;
	size_t length;
	ssize_t ret = -1;
	if (offset < 0) {
		errno = EINVAL;
		goto done;
	}
	n = MIN(n, tier_max_data(config(h)->client));
	r.method = write ? TIER_METHOD_WRITE : TIER_METHOD_READ;
	r.handle = fh(h, fsp);
	r.offset = offset;
	r.length = n;
	{
		struct tier_fh *f = VFS_FETCH_FSP_EXTENSION(h, fsp);
		r.stable = f && (f->flags & (O_SYNC | O_DSYNC));
	}
	if (!r.handle)
		goto done;
	call = tier_submit(tmp, config(h)->client, &r, write ? data : NULL, write ? n : 0,
			   NULL);
	if (!call || tier_wait(call))
		goto done;
	out = tier_result(call, &bytes, &length);
	if (!out)
		goto done;
	if (out->count > n || (!write && length != out->count) || (write && length)) {
		errno = EPROTO;
		goto done;
	}
	if (!write && length)
		memcpy(data, bytes, length);
	ret = out->count;
done: {
	int saved = errno;
	TALLOC_FREE(tmp);
	errno = saved;
}
	return ret;
}
static ssize_t tier_pread(struct vfs_handle_struct *h, struct files_struct *fsp,
			  void *data, size_t n, off_t offset)
{
	return tier_io_sync(h, fsp, data, n, offset, false);
}
static ssize_t tier_pwrite(struct vfs_handle_struct *h, struct files_struct *fsp,
			   const void *data, size_t n, off_t offset)
{
	return tier_io_sync(h, fsp, discard_const_p(void, data), n, offset, true);
}
static off_t tier_lseek(struct vfs_handle_struct *h, struct files_struct *fsp,
			off_t offset, int whence)
{
	struct tier_fh *f = VFS_FETCH_FSP_EXTENSION(h, fsp);
	SMB_STRUCT_STAT s;
	off_t base;
	if (!f) {
		errno = EBADF;
		return -1;
	}
	switch (whence) {
	case SEEK_SET:
		base = 0;
		break;
	case SEEK_CUR:
		base = f->offset;
		break;
	case SEEK_END:
		if (tier_fstat(h, fsp, &s))
			return -1;
		base = s.st_ex_size;
		break;
	default:
		errno = EINVAL;
		return -1;
	}
	if (offset < -base || (offset > 0 && base > INT64_MAX - offset)) {
		errno = EINVAL;
		return -1;
	}
	f->offset = base + offset;
	return f->offset;
}
static int tier_fcntl(struct vfs_handle_struct *h, struct files_struct *fsp, int cmd,
		      va_list args)
{
	struct tier_fh *f = VFS_FETCH_FSP_EXTENSION(h, fsp);
	int flags;
	if (!f) {
		errno = EBADF;
		return -1;
	}
	if (cmd == F_GETFL)
		return f->flags;
	if (cmd != F_SETFL) {
		errno = ENOTSUP;
		return -1;
	}
	flags = va_arg(args, int);
	if ((flags ^ f->flags) & ~(O_NONBLOCK | O_ACCMODE)) {
		errno = ENOTSUP;
		return -1;
	}
	f->flags = (f->flags & O_ACCMODE) | (flags & ~O_ACCMODE);
	return 0;
}
static int tier_chdir(struct vfs_handle_struct *h, const struct smb_filename *name)
{
	struct tier_config *c = config(h);
	TALLOC_CTX *tmp = talloc_new(h);
	TierRequest r = {0};
	TierResponse *out;
	uint64_t old;
	int ret = -1;
	if (!no_stream(name))
		goto done;
	/* The share root is stable and already attached. Reopening it on every
	 * Samba service switch would also change all directory-cache keys. */
	if (strcmp(name->base_name, "/") == 0) {
		if (c->cwd != c->root) {
			char *root_name = talloc_strdup(c, "/");
			if (!root_name) {
				errno = ENOMEM;
				goto done;
			}
			old = c->cwd;
			c->cwd = c->root;
			TALLOC_FREE(c->cwd_name);
			c->cwd_name = root_name;
			/* A failed daemon cannot invalidate an already opened local fd.
			 */
			tier_defer_close(c->client, old);
		}
		ret = 0;
		goto done;
	}
	r.method = TIER_METHOD_OPEN;
	r.parent = c->cwd;
	r.name = name->base_name;
	r.directory = true;
	out = tier_rpc(tmp, c->client, &r);
	if (!out)
		goto done;
	old = c->cwd;
	c->cwd = out->handle;
	{
		char *next =
		    name->base_name[0] == '/'
			? talloc_strdup(c, name->base_name)
			: talloc_asprintf(c, "%s/%s", c->cwd_name, name->base_name);
		if (next) {
			TALLOC_FREE(c->cwd_name);
			c->cwd_name = next;
		}
	}
	if (old != c->root) {
		r.method = TIER_METHOD_CLOSE;
		r.handle = old;
		rpc_status(h, &r);
	}
	ret = 0;
done: {
	int saved = errno;
	TALLOC_FREE(tmp);
	errno = saved;
}
	return ret;
}
static struct smb_filename *tier_getwd(struct vfs_handle_struct *h, TALLOC_CTX *ctx)
{
	return synthetic_smb_fname(ctx, config(h)->cwd_name, NULL, NULL, 0, 0);
}
static const char *tier_connectpath(struct vfs_handle_struct *h,
				    const struct files_struct *dir,
				    const struct smb_filename *name)
{
	return h->conn->connectpath;
}
static struct smb_filename *tier_realpath(struct vfs_handle_struct *h, TALLOC_CTX *ctx,
					  const struct smb_filename *name)
{
	char *path;
	struct smb_filename *out;
	const char *cwd = h->conn->cwd_fsp->fsp_name->base_name;
	if (!no_stream(name))
		return NULL;
	path = name->base_name[0] == '/'
		   ? talloc_strdup(ctx, name->base_name)
		   : talloc_asprintf(ctx, "%s/%s", cwd, name->base_name);
	if (!path) {
		errno = ENOMEM;
		return NULL;
	}
	/* Tier has no symlinks. Samba canonicalizes '..' before access; the daemon
	 * separately clamps traversal at the attached namespace root. */
	out = synthetic_smb_fname(ctx, path, NULL, NULL, 0, 0);
	TALLOC_FREE(path);
	return out;
}
static int dir_open_cursor(struct tier_dir *d)
{
	TALLOC_CTX *tmp = talloc_new(d);
	TierRequest r = {0};
	TierResponse *out;
	r.method = TIER_METHOD_OPENDIR;
	r.handle = d->handle;
	out = tier_rpc(tmp, d->config->client, &r);
	if (out) {
		d->cursor = out->handle;
		d->eof = false;
		d->error = 0;
	} else
		d->error = errno;
	TALLOC_FREE(tmp);
	return d->error ? -1 : 0;
}
static DIR *tier_fdopendir(struct vfs_handle_struct *h, struct files_struct *fsp,
			   const char *mask, uint32_t attrs)
{
	struct tier_dir *d = talloc_zero(h, struct tier_dir);
	if (!d) {
		errno = ENOMEM;
		return NULL;
	}
	d->config = config(h);
	d->handle = fh(h, fsp);
	d->fsp = fsp;
	if (!d->handle) {
		int saved = errno;
		TALLOC_FREE(d);
		errno = saved;
		return NULL;
	}
	return (DIR *)d;
}
static struct dirent *tier_readdir(struct vfs_handle_struct *h, struct files_struct *fsp,
				   DIR *dir)
{
	struct tier_dir *d = (struct tier_dir *)dir;
	TierRequest r = {0};
	TierEntry *entry;
	if (d->error) {
		errno = d->error;
		return NULL;
	}
	/* Snapshot on the first read, after Samba's initial rewind. */
	if (!d->cursor && dir_open_cursor(d))
		return NULL;
	if (!d->page || d->index == d->page->n_entries) {
		if (d->eof) {
			errno = 0;
			return NULL;
		}
		TALLOC_FREE(d->page_ctx);
		d->page = NULL;
		d->page_ctx = talloc_new(d);
		r.method = TIER_METHOD_READDIR;
		r.handle = d->cursor;
		r.max_entries = 256;
		r.max_bytes = 256 * 1024;
		d->page = tier_rpc(d->page_ctx, d->config->client, &r);
		if (!d->page) {
			d->error = errno;
			return NULL;
		}
		d->index = 0;
		d->eof = d->page->eof;
		if (!d->page->n_entries) {
			if (!d->eof) {
				d->error = EPROTO;
				errno = EPROTO;
			} else
				errno = 0;
			return NULL;
		}
	}
	entry = d->page->entries[d->index++];
	if (!entry->attr || strlen(entry->name) >= sizeof(d->entry.d_name)) {
		errno = EPROTO;
		return NULL;
	}
	ZERO_STRUCT(d->entry);
	d->entry.d_ino = entry->attr->inode;
	d->entry.d_type = entry->attr->directory ? DT_DIR : DT_REG;
	memcpy(d->entry.d_name, entry->name, strlen(entry->name) + 1);
	d->entry.d_reclen = offsetof(struct dirent, d_name) + strlen(entry->name) + 1;
	return &d->entry;
}
static void tier_rewinddir(struct vfs_handle_struct *h, DIR *dir)
{
	struct tier_dir *d = (struct tier_dir *)dir;
	TierRequest r = {0};
	TALLOC_CTX *tmp;
	if (!d->cursor) {
		d->error = 0;
		return;
	}
	tmp = talloc_new(d);
	r.method = TIER_METHOD_CLOSEDIR;
	r.handle = d->cursor;
	if (!tier_rpc(tmp, d->config->client, &r)) {
		d->error = errno;
		TALLOC_FREE(tmp);
		return;
	}
	TALLOC_FREE(tmp);
	d->cursor = 0;
	TALLOC_FREE(d->page_ctx);
	d->page = NULL;
	d->index = 0;
	d->eof = false;
	d->error = 0;
}
static int tier_closedir(struct vfs_handle_struct *h, DIR *dir)
{
	struct tier_dir *d = (struct tier_dir *)dir;
	TierRequest r = {0};
	int ret;
	r.method = TIER_METHOD_CLOSEDIR;
	r.handle = d->cursor;
	ret = d->cursor ? rpc_status(h, &r) : 0;
	{
		int saved = errno;
		int closed = tier_close(h, d->fsp);
		if (ret == 0)
			ret = closed;
		else
			errno = saved;
	}
	TALLOC_FREE(d);
	return ret;
}
static uint32_t tier_capabilities(struct vfs_handle_struct *h,
				  enum timestamp_set_resolution *resolution)
{
	*resolution = TIMESTAMP_SET_NT_OR_BETTER;
	return FILE_CASE_PRESERVED_NAMES;
}
static uint64_t tier_disk_free(struct vfs_handle_struct *h,
			       const struct smb_filename *name, uint64_t *block,
			       uint64_t *free_blocks, uint64_t *total_blocks)
{
	TALLOC_CTX *tmp = talloc_new(h);
	TierRequest r = {0};
	TierResponse *out;
	uint64_t ret = (uint64_t)-1;
	r.method = TIER_METHOD_FSINFO;
	r.handle = config(h)->root;
	out = tier_rpc(tmp, config(h)->client, &r);
	if (out) {
		*block = 4096;
		*free_blocks = out->free_bytes / *block;
		*total_blocks = out->total_bytes / *block;
		ret = out->free_bytes / 1024;
	}
	{
		int saved = errno;
		TALLOC_FREE(tmp);
		errno = saved;
	}
	return ret;
}
static int tier_statvfs(struct vfs_handle_struct *h, const struct smb_filename *name,
			struct vfs_statvfs_struct *s)
{
	uint64_t block, free_blocks, total_blocks;
	if (tier_disk_free(h, name, &block, &free_blocks, &total_blocks) == (uint64_t)-1)
		return -1;
	ZERO_STRUCTP(s);
	s->OptimalTransferSize = 1024 * 1024;
	s->BlockSize = block;
	s->TotalBlocks = total_blocks;
	s->BlocksAvail = s->UserBlocksAvail = free_blocks;
	s->FsIdentifier = 0x54494552;
	s->FsCapabilities = FILE_CASE_PRESERVED_NAMES;
	return 0;
}
static uint64_t tier_alloc_size(struct vfs_handle_struct *h, struct files_struct *fsp,
				const SMB_STRUCT_STAT *s)
{
	return s->st_ex_blocks * 512;
}
static NTSTATUS tier_get_dos(struct vfs_handle_struct *h, struct files_struct *fsp,
			     uint32_t *mode)
{
	*mode = S_ISDIR(fsp->fsp_name->st.st_ex_mode) ? FILE_ATTRIBUTE_DIRECTORY
						      : FILE_ATTRIBUTE_NORMAL;
	return NT_STATUS_OK;
}
static NTSTATUS tier_set_dos(struct vfs_handle_struct *h, struct files_struct *fsp,
			     uint32_t mode)
{
	/* Use Samba's configured UNIX mappings without storing extended attributes. */
	return NT_STATUS_NOT_IMPLEMENTED;
}
static NTSTATUS tier_get_real_name(struct vfs_handle_struct *h, struct files_struct *dir,
				   const char *name, TALLOC_CTX *ctx, char **found)
{
	return NT_STATUS_NOT_SUPPORTED;
}

static int tier_readlinkat(struct vfs_handle_struct *h, const struct files_struct *dir,
			   const struct smb_filename *name, char *buf, size_t size)
{
	errno = EINVAL;
	return -1;
}
static struct vfs_fn_pointers tier_fns = {
    .connect_fn = tier_connect,
    .disconnect_fn = tier_disconnect,
    .disk_free_fn = tier_disk_free,
    .get_quota_fn = vfs_not_implemented_get_quota,
    .set_quota_fn = vfs_not_implemented_set_quota,
    .statvfs_fn = tier_statvfs,
    .fs_capabilities_fn = tier_capabilities,
    .fdopendir_fn = tier_fdopendir,
    .readdir_fn = tier_readdir,
    .rewind_dir_fn = tier_rewinddir,
    .closedir_fn = tier_closedir,
    .mkdirat_fn = tier_mkdirat,
    .openat_fn = tier_openat,
    .close_fn = tier_close,
    .pread_fn = tier_pread,
    .pwrite_fn = tier_pwrite,
    .pread_send_fn = tier_pread_send,
    .pread_recv_fn = tier_io_recv,
    .pwrite_send_fn = tier_pwrite_send,
    .pwrite_recv_fn = tier_io_recv,
    .fsync_send_fn = tier_fsync_send,
    .fsync_recv_fn = tier_fsync_recv,
    .lseek_fn = tier_lseek,
    .sendfile_fn = vfs_not_implemented_sendfile,
    .recvfile_fn = vfs_not_implemented_recvfile,
    .renameat_fn = tier_renameat,
    .stat_fn = tier_stat,
    .lstat_fn = tier_stat,
    .fstat_fn = tier_fstat,
    .fstatat_fn = tier_fstatat,
    .get_alloc_size_fn = tier_alloc_size,
    .fsctl_fn = vfs_not_implemented_fsctl,
    .offload_read_send_fn = vfs_not_implemented_offload_read_send,
    .offload_read_recv_fn = vfs_not_implemented_offload_read_recv,
    .offload_write_send_fn = vfs_not_implemented_offload_write_send,
    .offload_write_recv_fn = vfs_not_implemented_offload_write_recv,
    .fget_compression_fn = vfs_not_implemented_fget_compression,
    .set_compression_fn = vfs_not_implemented_set_compression,
    .durable_cookie_fn = vfs_not_implemented_durable_cookie,
    .durable_disconnect_fn = vfs_not_implemented_durable_disconnect,
    .durable_reconnect_fn = vfs_not_implemented_durable_reconnect,
    .unlinkat_fn = tier_unlinkat,
    .fchmod_fn = tier_fchmod,
    .fchown_fn = vfs_not_implemented_fchown,
    .lchown_fn = vfs_not_implemented_lchown,
    .chdir_fn = tier_chdir,
    .getwd_fn = tier_getwd,
    .connectpath_fn = tier_connectpath,
    .fntimes_fn = tier_fntimes,
    .ftruncate_fn = tier_ftruncate,
    .fallocate_fn = vfs_not_implemented_fallocate,
    .lock_fn = vfs_not_implemented_lock,
    .filesystem_sharemode_fn = vfs_not_implemented_filesystem_sharemode,
    .fcntl_fn = tier_fcntl,
    .linux_setlease_fn = vfs_not_implemented_linux_setlease,
    .getlock_fn = vfs_not_implemented_getlock,
    .symlinkat_fn = vfs_not_implemented_symlinkat,
    .readlinkat_fn = tier_readlinkat,
    .linkat_fn = vfs_not_implemented_linkat,
    .mknodat_fn = vfs_not_implemented_mknodat,
    .realpath_fn = tier_realpath,
    .fchflags_fn = vfs_not_implemented_fchflags,
    .get_real_filename_at_fn = tier_get_real_name,
    .fget_dos_attributes_fn = tier_get_dos,
    .fset_dos_attributes_fn = tier_set_dos,
    .getxattrat_send_fn = vfs_not_implemented_getxattrat_send,
    .getxattrat_recv_fn = vfs_not_implemented_getxattrat_recv,
    .fgetxattr_fn = vfs_not_implemented_fgetxattr,
    .flistxattr_fn = vfs_not_implemented_flistxattr,
    .fsetxattr_fn = vfs_not_implemented_fsetxattr,
    .fremovexattr_fn = vfs_not_implemented_fremovexattr,
    .sys_acl_get_fd_fn = vfs_not_implemented_sys_acl_get_fd,
    .sys_acl_blob_get_fd_fn = vfs_not_implemented_sys_acl_blob_get_fd,
    .sys_acl_set_fd_fn = vfs_not_implemented_sys_acl_set_fd,
    .sys_acl_delete_def_fd_fn = vfs_not_implemented_sys_acl_delete_def_fd,
    .create_dfs_pathat_fn = vfs_not_implemented_create_dfs_pathat,
    .read_dfs_pathat_fn = vfs_not_implemented_read_dfs_pathat,
};
_PUBLIC_ NTSTATUS samba_init_module(TALLOC_CTX *ctx);
_PUBLIC_ NTSTATUS samba_init_module(TALLOC_CTX *ctx)
{
	return smb_register_vfs(SMB_VFS_INTERFACE_VERSION, "tier", &tier_fns);
}
