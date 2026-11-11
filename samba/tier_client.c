/* SPDX-License-Identifier: GPL-3.0-or-later */
#include "includes.h"
#include "tier_client.h"
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>

#define TIER_DATA_MAX (4 * 1024 * 1024)
#define TIER_INFLIGHT_MAX 32
#define TIER_TIMEOUT_SECONDS 60

/* Calls live on the connection until their reply is consumed. The caller's
 * context only owns a link: freeing an async SMB request must not leave a
 * dangling tevent_req in the dispatcher, or discard a partially sent frame. */
struct tier_call_owner {
	struct tier_call *call;
};

struct tier_call {
	struct tier_call *prev, *next;
	struct tier_client *client;
	struct tevent_req *async;
	struct tier_call_owner *owner;
	struct tevent_timer *timer;
	struct timespec deadline;
	TierMethod method;
	uint64_t id;
	TierResponse *response;
	int error;
	bool done, deferred;
};
struct tier_client {
	int fd;
	struct tevent_fd *fde;
	struct tevent_context *ev;
	struct tier_call *calls;
	uint64_t next_id;
	unsigned count, deferred;
	int error;
	uint32_t max_data;
	uint32_t max_inflight;
	bool closing;
	TierTransport *transport;
};
static void interest(struct tier_client *c)
{
	uint16_t flags = TEVENT_FD_READ;
	if (c->transport && tier_transport_wants_write(c->transport))
		flags |= TEVENT_FD_WRITE;
	if (c->fde)
		tevent_fd_set_flags(c->fde, c->error ? 0 : flags);
}
static void finish(struct tier_call *p, int error)
{
	DLIST_REMOVE(p->client->calls, p);
	p->client->count--;
	if (p->deferred)
		p->client->deferred--;
	p->error = error;
	p->done = true;
	TALLOC_FREE(p->timer);
	if (p->owner)
		talloc_steal(p->owner, p);
	if (!p->owner) {
		TALLOC_FREE(p);
		return;
	}
	if (p->async)
		tevent_req_done(p->async);
}
static void fail(struct tier_client *c, int error)
{
	if (!c->error && !c->closing) {
		DBG_ERR("tier IPC failed: errno=%d (%s), pending=%u, request=%llu, "
			"method=%d\n",
			error, strerror(error), c->count,
			(unsigned long long)(c->calls ? c->calls->id : 0),
			c->calls ? c->calls->method : 0);
	}
	c->error = error ? error : EIO;
	if (c->fd >= 0)
		shutdown(c->fd, SHUT_RDWR);
	while (c->calls)
		finish(c->calls, c->error);
	tier_transport_free(c->transport);
	c->transport = NULL;
	interest(c);
}
static int call_destroy(struct tier_call *p)
{
	if (p->owner)
		p->owner->call = NULL;
	if (p->response)
		tier_response_free(p->response);
	return 0;
}
static int owner_destroy(struct tier_call_owner *owner)
{
	struct tier_call *p = owner->call;
	if (p) {
		p->owner = NULL;
		p->async = NULL;
		if (p->done)
			TALLOC_FREE(p);
	}
	return 0;
}
static void call_timeout(struct tevent_context *ev, struct tevent_timer *timer,
			 struct timeval now, void *data)
{
	struct tier_call *p = data;
	DBG_ERR("tier IPC timeout: request=%llu method=%d\n", (unsigned long long)p->id,
		p->method);
	fail(p->client, ETIMEDOUT);
}
static int client_destroy(struct tier_client *c)
{
	c->closing = true;
	fail(c, ECONNRESET);
	if (c->fd >= 0)
		close(c->fd);
	return 0;
}
static void pump(struct tier_client *c, bool writable)
{
	TierResponse *response;
	uint64_t id;
	int error;
	if (c->error)
		return;
	error = tier_transport_pump(c->transport, writable);
	/* A peer may close after its final reply. Deliver complete responses before
	 * failing the requests that did not receive one. */
	while ((response = tier_transport_next(c->transport, &id)) != NULL) {
		struct tier_call *p;
		for (p = c->calls; p && p->id != id; p = p->next) {
		}
		if (!p) {
			tier_response_free(response);
			fail(c, EPROTO);
			return;
		}
		p->response = response;
		if (p->deferred && response->error) {
			DBG_ERR("tier deferred CLOSE failed: request=%llu errno=%d\n",
				(unsigned long long)p->id, response->error);
			fail(c, response->error);
			return;
		}
		finish(p, 0);
	}
	if (error) {
		fail(c, error);
		return;
	}
	interest(c);
}
static void socket_event(struct tevent_context *ev, struct tevent_fd *fde, uint16_t flags,
			 void *data)
{
	pump(data, flags & TEVENT_FD_WRITE);
}
/* Poll only this IPC connection; entering tevent here would reenter Samba. */
static int poll_client(struct tier_client *c)
{
	struct pollfd fd = {.fd = c->fd, .events = POLLIN};
	struct tier_call *q;
	struct timespec now;
	int64_t remaining = INT64_MAX;
	int ret;
	if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
		fail(c, errno);
		return -1;
	}
	for (q = c->calls; q; q = q->next) {
		int64_t deadline = (q->deadline.tv_sec - now.tv_sec) * 1000 +
				   (q->deadline.tv_nsec - now.tv_nsec) / 1000000;
		remaining = MIN(remaining, deadline);
	}
	if (tier_transport_wants_write(c->transport))
		fd.events |= POLLOUT;
	if (remaining <= 0) {
		fail(c, ETIMEDOUT);
		return -1;
	}
	ret = poll(&fd, 1, (int)MIN(remaining, INT_MAX));
	if (ret < 0 && errno == EINTR)
		return 0;
	if (ret <= 0) {
		fail(c, ret ? errno : ETIMEDOUT);
		return -1;
	}
	pump(c, fd.revents & POLLOUT);
	return c->error ? -1 : 0;
}
struct tier_call *tier_submit(TALLOC_CTX *ctx, struct tier_client *c,
			      const TierRequest *request, const void *data, size_t length,
			      struct tevent_req *async)
{
	struct tier_call *p;
	int error;
	/* Deferred cleanup must not turn a previously available slot into EBUSY. */
	while (!c->error && c->count >= c->max_inflight && c->deferred) {
		if (poll_client(c))
			break;
	}
	if (c->error) {
		errno = c->error;
		return NULL;
	}
	if (c->count >= c->max_inflight) {
		DBG_WARNING("tier IPC queue full: pending=%u method=%d\n", c->count,
			    request->method);
		errno = EBUSY;
		return NULL;
	}
	if (c->next_id == UINT64_MAX) {
		fail(c, EOVERFLOW);
		errno = EOVERFLOW;
		return NULL;
	}
	p = talloc_zero(c, struct tier_call);
	if (!p) {
		errno = ENOMEM;
		return NULL;
	}
	p->client = c;
	p->id = ++c->next_id;
	p->async = async;
	if (async)
		tevent_req_defer_callback(async, c->ev);
	p->method = request->method;
	p->owner = talloc_zero(ctx, struct tier_call_owner);
	if (!p->owner) {
		TALLOC_FREE(p);
		errno = ENOMEM;
		return NULL;
	}
	p->owner->call = p;
	talloc_set_destructor(p->owner, owner_destroy);
	talloc_set_destructor(p, call_destroy);
	if (clock_gettime(CLOCK_MONOTONIC, &p->deadline) != 0) {
		TALLOC_FREE(p);
		return NULL;
	}
	p->deadline.tv_sec += TIER_TIMEOUT_SECONDS;
	p->timer = tevent_add_timer(
	    c->ev, p, timeval_current_ofs(TIER_TIMEOUT_SECONDS, 0), call_timeout, p);
	if (!p->timer) {
		TALLOC_FREE(p);
		errno = ENOMEM;
		return NULL;
	}
	error = tier_transport_submit(c->transport, request, data, length, p->id);
	if (error) {
		TALLOC_FREE(p->owner);
		TALLOC_FREE(p);
		errno = error;
		return NULL;
	}
	DLIST_ADD_END(c->calls, p);
	c->count++;
	interest(c);
	return p;
}
int tier_wait(struct tier_call *p)
{
	while (!p->done) {
		if (poll_client(p->client))
			break;
	}
	if (p->error) {
		errno = p->error;
		return -1;
	}
	return 0;
}
TierResponse *tier_result(struct tier_call *p, void **data, size_t *length)
{
	if (!p->done || p->error || !p->response) {
		errno = p->error ? p->error : EIO;
		return NULL;
	}
	if (p->response->error) {
		/* Ordinary namespace misses are common; keep them at debug level. */
		if (p->response->error == EIO || p->response->error == ENOSPC ||
		    p->response->error == EPROTO)
			DBG_ERR("tier request=%llu method=%d errno=%d\n",
				(unsigned long long)p->id, p->method, p->response->error);
		else
			DBG_DEBUG("tier request=%llu method=%d errno=%d\n",
				  (unsigned long long)p->id, p->method,
				  p->response->error);
		errno = p->response->error;
		return NULL;
	}
	if (data)
		*data = p->response->data;
	if (length)
		*length = p->response->data_length;
	return p->response;
}
TierResponse *tier_rpc(TALLOC_CTX *ctx, struct tier_client *c, const TierRequest *request)
{
	struct tier_call *p = tier_submit(ctx, c, request, NULL, 0, NULL);
	if (!p || tier_wait(p))
		return NULL;
	return tier_result(p, NULL, NULL);
}
int tier_defer_close(struct tier_client *c, uint64_t handle)
{
	TierRequest request = {0};
	struct tier_call *p;
	request.method = TIER_METHOD_CLOSE;
	request.handle = handle;
	p = tier_submit(c, c, &request, NULL, 0, NULL);
	if (!p)
		return -1;
	p->deferred = true;
	c->deferred++;
	/* The connection retains the call and any unsent frame until its reply.
	 * Never pass this ownerless call to tier_wait: finish frees it. */
	TALLOC_FREE(p->owner);
	return 0;
}
struct tier_client *tier_client_connect(TALLOC_CTX *ctx, struct tevent_context *ev,
					const char *path)
{
	struct sockaddr_un addr = {.sun_family = AF_UNIX};
	struct tier_client *c;
	TierRequest request = {0};
	TierResponse *response;
	TALLOC_CTX *tmp;
	if (strlen(path) >= sizeof(addr.sun_path)) {
		errno = ENAMETOOLONG;
		return NULL;
	}
	c = talloc_zero(ctx, struct tier_client);
	if (!c) {
		errno = ENOMEM;
		return NULL;
	}
	c->fd = -1;
	c->ev = ev;
	c->max_data = TIER_DATA_MAX;
	c->max_inflight = TIER_INFLIGHT_MAX;
	talloc_set_destructor(c, client_destroy);
	c->fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (c->fd == -1)
		goto error;
	memcpy(addr.sun_path, path, strlen(path) + 1);
	if (connect(c->fd, (struct sockaddr *)&addr, sizeof(addr)))
		goto error;
	if (fcntl(c->fd, F_SETFL, O_NONBLOCK))
		goto error;
	c->transport = tier_transport_new(c->fd);
	if (!c->transport) {
		errno = ENOMEM;
		goto error;
	}
	c->fde = tevent_add_fd(ev, c, c->fd, TEVENT_FD_READ, socket_event, c);
	if (!c->fde) {
		errno = ENOMEM;
		goto error;
	}
	tmp = talloc_new(c);
	request.method = TIER_METHOD_HELLO;
	request.protocol_version = 1;
	response = tier_rpc(tmp, c, &request);
	if (!response)
		goto error;
	if (response->protocol_version != 1 || !response->max_data ||
	    !response->max_inflight || !response->max_message) {
		errno = EPROTO;
		goto error;
	}
	c->max_data = MIN(response->max_data, TIER_DATA_MAX);
	tier_transport_set_max_data(c->transport, c->max_data);
	c->max_inflight = MIN(response->max_inflight, TIER_INFLIGHT_MAX);
	TALLOC_FREE(tmp);
	return c;
error: {
	int saved = errno;
	DBG_ERR("tier IPC connect failed: socket=%s errno=%d (%s)\n", path, saved,
		strerror(saved));
	TALLOC_FREE(c);
	errno = saved;
}
	return NULL;
}
uint32_t tier_max_data(struct tier_client *c) { return c->max_data; }
