/* SPDX-License-Identifier: GPL-3.0-or-later */
#include "includes.h"
#include "smbd/globals.h"
#include "smbd/smbd.h"
#include <dirent.h>

extern int tier_connect(const char *, void **);
extern void tier_disconnect(void *);
extern int tier_open(const void *, const char *, int, bool, void **);
extern void tier_release(void *);
extern int tier_stat(const void *, const char *, struct stat *);
extern int tier_identity(const void *, const char *, uint64_t *);
extern int tier_list(const void *, const char *, void **);
extern int tier_entry(const void *, size_t, char *, size_t);
extern void tier_list_free(void *);
extern int tier_mkdir(const void *, const char *);
extern int tier_remove(const void *, const char *, bool);
extern int tier_rename(const void *, const char *, const char *);
extern int tier_setattr(const void *, const char *, int,
                        const struct timespec *);

struct connection {
  void *store;
  char *cwd;
};
struct opened {
  void *owner;
  char *path;
  bool metadata;
};
struct listing {
  void *entries;
  size_t offset;
  struct dirent entry;
};
static struct connection *connection(struct vfs_handle_struct *h) {
  return h->data;
}
static char *relative(struct vfs_handle_struct *h,
                      const struct files_struct *dir,
                      const struct smb_filename *name) {
  struct opened *opened = dir ? VFS_FETCH_FSP_EXTENSION(h, dir) : NULL;
  const char *base = opened ? opened->path : connection(h)->cwd;
  const char *part = name->base_name;
  if (name->stream_name) {
    errno = ENOTSUP;
    return NULL;
  }
  if (part[0] == '/') {
    size_t length = strlen(h->conn->connectpath);
    base = "";
    if (strncmp(part, h->conn->connectpath, length) == 0 &&
        (part[length] == '/' || part[length] == '\0'))
      part += length;
    while (*part == '/')
      part++;
  }
  if (strcmp(part, ".") == 0)
    part = "";
  if (!*part)
    return talloc_strdup(talloc_tos(), base);
  return talloc_asprintf(talloc_tos(), "%s%s%s", base, *base ? "/" : "", part);
}
static int connect_share(struct vfs_handle_struct *h, const char *service,
                         const char *user) {
  struct connection *c = talloc_zero(h, struct connection);
  const char *config = lp_parm_const_string(SNUM(h->conn), "tier", "config",
                                            "/etc/tier/config.toml");
  if (!c) {
    errno = ENOMEM;
    return -1;
  }
  if (SMB_VFS_NEXT_CONNECT(h, service, user) != 0) {
    TALLOC_FREE(c);
    return -1;
  }
  if (tier_connect(config, &c->store) != 0) {
    TALLOC_FREE(c);
    return -1;
  }
  c->cwd = talloc_strdup(c, "");
  h->data = c;
  return 0;
}
static void disconnect_share(struct vfs_handle_struct *h) {
  tier_disconnect(connection(h)->store);
  TALLOC_FREE(h->data);
  SMB_VFS_NEXT_DISCONNECT(h);
}
static void convert(const struct stat *s, SMB_STRUCT_STAT *out) {
  ZERO_STRUCTP(out);
  out->st_ex_dev = s->st_dev;
  out->st_ex_ino = s->st_ino;
  out->st_ex_mode = s->st_mode;
  out->st_ex_nlink = s->st_nlink;
  out->st_ex_uid = s->st_uid;
  out->st_ex_gid = s->st_gid;
  out->st_ex_size = s->st_size;
  out->st_ex_blksize = s->st_blksize;
  out->st_ex_blocks = s->st_blocks;
  out->st_ex_atime = s->st_atim;
  out->st_ex_mtime = s->st_mtim;
  out->st_ex_ctime = s->st_ctim;
  out->st_ex_btime = s->st_ctim;
}
static int stat_at(struct vfs_handle_struct *h, const struct files_struct *dir,
                   const struct smb_filename *name, SMB_STRUCT_STAT *out,
                   int flags) {
  char *path = relative(h, dir, name);
  struct stat s;
  if (!path || tier_stat(connection(h)->store, path, &s) != 0)
    return -1;
  convert(&s, out);
  uint64_t id;
  if (tier_identity(connection(h)->store, path, &id) != 0)
    return -1;
  out->st_ex_dev = 0x54494552;
  out->st_ex_ino = id;
  return 0;
}
static int stat_path(struct vfs_handle_struct *h, struct smb_filename *name) {
  return stat_at(h, NULL, name, &name->st, 0);
}
static int stat_fd(struct vfs_handle_struct *h, struct files_struct *fsp,
                   SMB_STRUCT_STAT *out) {
  struct opened *opened = VFS_FETCH_FSP_EXTENSION(h, fsp);
  struct stat s;
  if (!opened)
    return SMB_VFS_NEXT_FSTAT(h, fsp, out);
  if (opened->metadata) {
    if (tier_stat(connection(h)->store, opened->path, &s) != 0)
      return -1;
    convert(&s, out);
  } else if (SMB_VFS_NEXT_FSTAT(h, fsp, out) != 0)
    return -1;
  uint64_t id;
  if (tier_identity(connection(h)->store, opened->path, &id) != 0)
    return -1;
  out->st_ex_dev = 0x54494552;
  out->st_ex_ino = id;
  return 0;
}
static int open_at(struct vfs_handle_struct *h, const struct files_struct *dir,
                   const struct smb_filename *name, struct files_struct *fsp,
                   const struct vfs_open_how *how) {
  char *path = relative(h, dir, name);
  struct opened *opened;
  int fd;
  if (!path)
    return -1;
  opened = VFS_ADD_FSP_EXTENSION(h, fsp, struct opened, NULL);
  if (!opened) {
    errno = ENOMEM;
    return -1;
  }
  opened->path = talloc_strdup(fsp, path);
  opened->metadata =
      fsp->fsp_flags.is_pathref || (how->flags & O_PATH) ||
      (fsp->access_mask &&
       !(fsp->access_mask &
         (SEC_FILE_READ_DATA | SEC_FILE_WRITE_DATA | SEC_FILE_APPEND_DATA)) &&
       !(how->flags & (O_CREAT | O_TRUNC)));
  fd = tier_open(connection(h)->store, path, how->flags, opened->metadata,
                 &opened->owner);
  if (fd < 0)
    VFS_REMOVE_FSP_EXTENSION(h, fsp);
  fsp->fsp_flags.have_proc_fds = false;
  return fd;
}
static int close_file(struct vfs_handle_struct *h, struct files_struct *fsp) {
  struct opened *opened = VFS_FETCH_FSP_EXTENSION(h, fsp);
  int result = SMB_VFS_NEXT_CLOSE(h, fsp), saved = errno;
  if (opened)
    tier_release(opened->owner);
  VFS_REMOVE_FSP_EXTENSION(h, fsp);
  errno = saved;
  return result;
}
static DIR *open_directory(struct vfs_handle_struct *h,
                           struct files_struct *fsp, const char *mask,
                           uint32_t attrs) {
  struct opened *opened = VFS_FETCH_FSP_EXTENSION(h, fsp);
  struct listing *listing = talloc_zero(h, struct listing);
  if (!listing) {
    errno = ENOMEM;
    return NULL;
  }
  if (!opened ||
      tier_list(connection(h)->store, opened->path, &listing->entries) != 0) {
    TALLOC_FREE(listing);
    return NULL;
  }
  return (DIR *)listing;
}
static struct dirent *read_directory(struct vfs_handle_struct *h,
                                     struct files_struct *fsp, DIR *dir) {
  struct listing *listing = (struct listing *)dir;
  int result = tier_entry(listing->entries, listing->offset,
                          listing->entry.d_name, sizeof(listing->entry.d_name));
  if (result <= 0) {
    if (!result)
      errno = 0;
    return NULL;
  }
  listing->offset++;
  listing->entry.d_ino = listing->offset;
  listing->entry.d_type = DT_UNKNOWN;
  return &listing->entry;
}
static void rewind_directory(struct vfs_handle_struct *h, DIR *dir) {
  ((struct listing *)dir)->offset = 0;
}
static int close_directory(struct vfs_handle_struct *h, DIR *dir) {
  struct listing *listing = (struct listing *)dir;
  tier_list_free(listing->entries);
  TALLOC_FREE(listing);
  return 0;
}
static int mkdir_at(struct vfs_handle_struct *h, struct files_struct *dir,
                    const struct smb_filename *name, mode_t mode) {
  char *path = relative(h, dir, name);
  return path ? tier_mkdir(connection(h)->store, path) : -1;
}
static int unlink_at(struct vfs_handle_struct *h, struct files_struct *dir,
                     const struct smb_filename *name, int flags) {
  char *path = relative(h, dir, name);
  return path ? tier_remove(connection(h)->store, path,
                            (flags & AT_REMOVEDIR) != 0)
              : -1;
}
static int rename_at(struct vfs_handle_struct *h, struct files_struct *srcdir,
                     const struct smb_filename *src,
                     struct files_struct *dstdir,
                     const struct smb_filename *dst,
                     const struct vfs_rename_how *how) {
  char *old = relative(h, srcdir, src), *new = relative(h, dstdir, dst);
  if (how->flags) {
    errno = ENOTSUP;
    return -1;
  }
  return old && new ? tier_rename(connection(h)->store, old, new) : -1;
}
static int change_directory(struct vfs_handle_struct *h,
                            const struct smb_filename *name) {
  char *path = relative(h, NULL, name);
  struct stat s;
  if (!path || tier_stat(connection(h)->store, path, &s) != 0)
    return -1;
  if (!S_ISDIR(s.st_mode)) {
    errno = ENOTDIR;
    return -1;
  }
  TALLOC_FREE(connection(h)->cwd);
  connection(h)->cwd = talloc_strdup(connection(h), path);
  return 0;
}
static int chmod_file(struct vfs_handle_struct *h, struct files_struct *fsp,
                      mode_t mode) {
  struct opened *opened = VFS_FETCH_FSP_EXTENSION(h, fsp);
  if (!opened)
    return SMB_VFS_NEXT_FCHMOD(h, fsp, mode);
  return tier_setattr(connection(h)->store, opened->path, mode, NULL);
}
static struct smb_filename *working_directory(struct vfs_handle_struct *h,
                                              TALLOC_CTX *ctx) {
  char *path =
      talloc_asprintf(ctx, "%s/%s", h->conn->connectpath, connection(h)->cwd);
  if (!path) {
    errno = ENOMEM;
    return NULL;
  }
  return synthetic_smb_fname(ctx, path, NULL, NULL, 0, 0);
}
static int times_file(struct vfs_handle_struct *h, struct files_struct *fsp,
                      struct smb_file_time *ft) {
  struct opened *opened = VFS_FETCH_FSP_EXTENSION(h, fsp);
  struct timespec times[2] = {{.tv_nsec = UTIME_NOW}, {.tv_nsec = UTIME_NOW}};
  if (ft) {
    if (is_omit_timespec(&ft->atime) && is_omit_timespec(&ft->mtime))
      return 0;
    times[0] = ft->atime;
    times[1] = ft->mtime;
    if (is_omit_timespec(&ft->atime))
      times[0].tv_nsec = UTIME_OMIT;
    if (is_omit_timespec(&ft->mtime))
      times[1].tv_nsec = UTIME_OMIT;
  }
  if (!opened)
    return futimens(fsp_get_io_fd(fsp), times);
  return tier_setattr(connection(h)->store, opened->path, -1, times);
}
static struct vfs_fn_pointers operations = {
    .connect_fn = connect_share,
    .disconnect_fn = disconnect_share,
    .openat_fn = open_at,
    .close_fn = close_file,
    .stat_fn = stat_path,
    .lstat_fn = stat_path,
    .fstat_fn = stat_fd,
    .fstatat_fn = stat_at,
    .fdopendir_fn = open_directory,
    .readdir_fn = read_directory,
    .rewind_dir_fn = rewind_directory,
    .closedir_fn = close_directory,
    .mkdirat_fn = mkdir_at,
    .unlinkat_fn = unlink_at,
    .renameat_fn = rename_at,
    .chdir_fn = change_directory,
    .fchmod_fn = chmod_file,
    .fntimes_fn = times_file,
    .getwd_fn = working_directory,
    .symlinkat_fn = vfs_not_implemented_symlinkat,
    .linkat_fn = vfs_not_implemented_linkat,
    .fgetxattr_fn = vfs_not_implemented_fgetxattr,
    .fsetxattr_fn = vfs_not_implemented_fsetxattr,
    .flistxattr_fn = vfs_not_implemented_flistxattr,
    .fremovexattr_fn = vfs_not_implemented_fremovexattr,
};
__attribute__((visibility("default"))) NTSTATUS
samba_init_module(TALLOC_CTX *ctx) {
  return smb_register_vfs(SMB_VFS_INTERFACE_VERSION, "tier", &operations);
}
