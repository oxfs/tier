/* SPDX-License-Identifier: GPL-3.0-or-later */
#ifndef TIER_RUST_H
#define TIER_RUST_H
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef enum {
	TIER_METHOD_INVALID,
	TIER_METHOD_HELLO,
	TIER_METHOD_ATTACH,
	TIER_METHOD_STAT_AT,
	TIER_METHOD_GETATTR,
	TIER_METHOD_OPEN,
	TIER_METHOD_SETATTR,
	TIER_METHOD_MKDIR,
	TIER_METHOD_RENAME,
	TIER_METHOD_UNLINK,
	TIER_METHOD_FSINFO,
	TIER_METHOD_READ,
	TIER_METHOD_WRITE,
	TIER_METHOD_SYNC,
	TIER_METHOD_CLOSE,
	TIER_METHOD_OPENDIR,
	TIER_METHOD_READDIR,
	TIER_METHOD_CLOSEDIR,
	TIER_METHOD_DETACH
} TierMethod;
enum {
	TIER_ACCESS_METADATA,
	TIER_ACCESS_READ_ONLY,
	TIER_ACCESS_WRITE_ONLY,
	TIER_ACCESS_READ_WRITE
};
enum {
	TIER_DISPOSITION_EXISTING,
	TIER_DISPOSITION_CREATE_NEW,
	TIER_DISPOSITION_OPEN_OR_CREATE
};
typedef struct {
	int64_t seconds;
	uint32_t nanos;
} TierTimestamp;
typedef struct {
	char *node_id;
	uint64_t inode;
	bool directory;
	uint64_t size, allocated;
	uint32_t mode, uid, gid, nlink;
	TierTimestamp *atime, *mtime, *ctime, *btime;
	uint64_t version;
} TierAttr;
typedef struct {
	uint64_t size;
	TierTimestamp *atime, *mtime;
	uint32_t mode;
	bool has_size, has_mode;
	TierTimestamp *btime;
} TierChanges;
typedef struct {
	int32_t method;
	uint64_t handle, parent;
	const char *name;
	uint64_t target_parent;
	const char *target_name;
	int32_t access, disposition;
	bool truncate, directory, replace;
	uint64_t offset;
	uint32_t length;
	bool stable;
	const TierChanges *changes;
	uint32_t max_entries, max_bytes, protocol_version;
	bool local_access;
} TierRequest;
typedef struct {
	char *name;
	TierAttr *attr;
} TierEntry;
typedef struct {
	int32_t error;
	char *message;
	uint64_t handle;
	TierAttr *attr;
	size_t n_entries;
	TierEntry **entries;
	uint32_t count;
	bool eof, created;
	uint64_t total_bytes, free_bytes;
	char *instance;
	uint32_t protocol_version, max_message, max_data, max_inflight;
	uint8_t *data;
	size_t data_length;
} TierResponse;
typedef struct TierTransport TierTransport;
/* All calls run on the owning smbd thread. fd is borrowed and remains C-owned.
 * Requests/data are borrowed only during submit. Responses and all nested
 * pointers remain Rust-owned until tier_response_free; never free them in C.
 * Functions returning int return zero or a positive errno. Fatal pump errors
 * require abandoning this transport; requests are never replayed. */
TierTransport *tier_transport_new(int fd);
void tier_transport_free(TierTransport *);
int tier_transport_submit(TierTransport *, const TierRequest *, const uint8_t *, size_t,
			  uint64_t);
int tier_transport_pump(TierTransport *, bool writable);
bool tier_transport_wants_write(const TierTransport *);
TierResponse *tier_transport_next(TierTransport *, uint64_t *id);
void tier_transport_set_max_data(TierTransport *, uint32_t);
void tier_response_free(TierResponse *);
typedef struct ReadJob TierReadJob;
int tier_control_connect(TierTransport *, const char *);
int tier_control_pump(TierTransport *);
bool tier_is_local_handle(const TierTransport *, uint64_t);
void tier_local_hits(const TierTransport *, uint64_t (*)[3]);
void tier_transport_abort(TierTransport *, int);
int tier_control_fd(const TierTransport *);
bool tier_control_wants_write(const TierTransport *);
int tier_local_read(TierTransport *, uint64_t, uint64_t, size_t, TierReadJob **);
void tier_local_read_run(TierReadJob *);
TierResponse *tier_local_read_finish(TierReadJob *);
#endif
