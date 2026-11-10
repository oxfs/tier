/* SPDX-License-Identifier: GPL-3.0-or-later */
#include "includes.h"
#include "tier_client.h"
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>

#define TIER_HEADER 24
#define TIER_MESSAGE_MAX (1024 * 1024)
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
	Tier__Ipc__Method method;
	uint64_t id;
	uint8_t *frame;
	size_t length, sent;
	Tier__Ipc__Response *response;
	uint8_t *data;
	size_t data_length;
	int error;
	bool done;
};
struct tier_client {
	int fd;
	struct tevent_fd *fde;
	struct tevent_context *ev;
	struct tier_call *calls;
	uint64_t next_id;
	unsigned count;
	int error;
	uint32_t max_data;
	uint32_t max_inflight;
	bool closing;
	uint8_t header[TIER_HEADER];
	size_t header_used, body_used, body_length;
	uint8_t *body;
};
static uint64_t get_be(const uint8_t *p, size_t n)
{
	uint64_t v = 0;
	while (n--)
		v = (v << 8) | *p++;
	return v;
}
static void put_be(uint8_t *p, uint64_t v, size_t n)
{
	while (n) {
		p[--n] = v & 255;
		v >>= 8;
	}
}
static void interest(struct tier_client *c)
{
	struct tier_call *p;
	uint16_t flags = TEVENT_FD_READ;
	for (p = c->calls; p; p = p->next) {
		if (p->sent < p->length) {
			flags |= TEVENT_FD_WRITE;
			break;
		}
	}
	if (c->fde)
		tevent_fd_set_flags(c->fde, c->error ? 0 : flags);
}
static void finish(struct tier_call *p, int error)
{
	DLIST_REMOVE(p->client->calls, p);
	p->client->count--;
	p->error = error;
	p->done = true;
	TALLOC_FREE(p->frame);
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
	while (c->calls)
		finish(c->calls, c->error);
	interest(c);
}
static int call_destroy(struct tier_call *p)
{
	if (p->owner)
		p->owner->call = NULL;
	if (p->response)
		tier__ipc__response__free_unpacked(p->response, NULL);
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
	struct tier_call *p;
	ssize_t n;
	if (c->error)
		return;
	if (writable) {
		for (p = c->calls; p; p = p->next) {
			while (p->sent < p->length) {
				n = send(c->fd, p->frame + p->sent, p->length - p->sent,
					 MSG_NOSIGNAL);
				if (n < 0 && errno == EINTR)
					continue;
				if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))
					goto receive;
				if (n <= 0) {
					fail(c, n ? errno : EPIPE);
					return;
				}
				p->sent += n;
			}
		}
	}
receive:
	for (;;) {
		uint32_t mlen, dlen;
		uint64_t id;
		if (c->header_used < TIER_HEADER) {
			n = recv(c->fd, c->header + c->header_used,
				 TIER_HEADER - c->header_used, 0);
			if (n < 0 && errno == EINTR)
				continue;
			if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))
				break;
			if (n <= 0) {
				fail(c, n ? errno : ECONNRESET);
				return;
			}
			c->header_used += n;
			if (c->header_used != TIER_HEADER)
				continue;
			mlen = get_be(c->header + 16, 4);
			dlen = get_be(c->header + 20, 4);
			if (get_be(c->header, 4) != 0x54494552 ||
			    get_be(c->header + 4, 2) != 1 ||
			    get_be(c->header + 6, 2) != 0 || mlen > TIER_MESSAGE_MAX ||
			    dlen > c->max_data) {
				fail(c, EPROTO);
				return;
			}
			c->body_length = (size_t)mlen + dlen;
			c->body = talloc_array(c, uint8_t, c->body_length);
			if (!c->body) {
				fail(c, ENOMEM);
				return;
			}
		}
		if (c->body_used < c->body_length) {
			n = recv(c->fd, c->body + c->body_used,
				 c->body_length - c->body_used, 0);
			if (n < 0 && errno == EINTR)
				continue;
			if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))
				break;
			if (n <= 0) {
				fail(c, n ? errno : ECONNRESET);
				return;
			}
			c->body_used += n;
			if (c->body_used != c->body_length)
				continue;
		}
		id = get_be(c->header + 8, 8);
		mlen = get_be(c->header + 16, 4);
		dlen = get_be(c->header + 20, 4);
		for (p = c->calls; p && p->id != id; p = p->next) {
		}
		if (!p || p->sent != p->length) {
			fail(c, EPROTO);
			return;
		}
		p->response = tier__ipc__response__unpack(NULL, mlen, c->body);
		if (!p->response) {
			fail(c, EPROTO);
			return;
		}
		if (p->response->error < 0 || p->response->error > 4095 ||
		    (dlen &&
		     (p->method != TIER__IPC__METHOD__READ || p->response->error))) {
			fail(c, EPROTO);
			return;
		}
		if (dlen) {
			p->data = talloc_steal(p, c->body);
			memmove(p->data, p->data + mlen, dlen);
			p->data_length = dlen;
			c->body = NULL;
		}
		TALLOC_FREE(c->body);
		c->header_used = c->body_used = c->body_length = 0;
		finish(p, 0);
	}
	interest(c);
}
static void socket_event(struct tevent_context *ev, struct tevent_fd *fde, uint16_t flags,
			 void *data)
{
	pump(data, flags & TEVENT_FD_WRITE);
}
struct tier_call *tier_submit(TALLOC_CTX *ctx, struct tier_client *c,
			      const Tier__Ipc__Request *request, const void *data,
			      size_t length, struct tevent_req *async)
{
	struct tier_call *p;
	size_t mlen = tier__ipc__request__get_packed_size(request);
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
	if (mlen > TIER_MESSAGE_MAX || length > c->max_data) {
		errno = EMSGSIZE;
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
	p->length = TIER_HEADER + mlen + length;
	p->frame = talloc_array(p, uint8_t, p->length);
	if (!p->frame) {
		TALLOC_FREE(p);
		errno = ENOMEM;
		return NULL;
	}
	put_be(p->frame, 0x54494552, 4);
	put_be(p->frame + 4, 1, 2);
	put_be(p->frame + 6, 0, 2);
	put_be(p->frame + 8, p->id, 8);
	put_be(p->frame + 16, mlen, 4);
	put_be(p->frame + 20, length, 4);
	tier__ipc__request__pack(request, p->frame + TIER_HEADER);
	if (length)
		memcpy(p->frame + TIER_HEADER + mlen, data, length);
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
	DLIST_ADD_END(c->calls, p);
	c->count++;
	interest(c);
	return p;
}
int tier_wait(struct tier_call *p)
{
	while (!p->done) {
		struct pollfd fd = {.fd = p->client->fd, .events = POLLIN};
		struct tier_call *q;
		struct timespec now;
		int64_t remaining;
		int ret;
		for (q = p->client->calls; q; q = q->next) {
			if (q->sent < q->length) {
				fd.events |= POLLOUT;
				break;
			}
		}
		if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
			fail(p->client, errno);
			break;
		}
		remaining = (p->deadline.tv_sec - now.tv_sec) * 1000 +
			    (p->deadline.tv_nsec - now.tv_nsec) / 1000000;
		if (remaining <= 0) {
			fail(p->client, ETIMEDOUT);
			break;
		}
		ret = poll(&fd, 1, (int)remaining);
		if (ret < 0 && errno == EINTR)
			continue;
		if (ret <= 0) {
			fail(p->client, ret ? errno : ETIMEDOUT);
			break;
		}
		pump(p->client, fd.revents & POLLOUT);
	}
	if (p->error) {
		errno = p->error;
		return -1;
	}
	return 0;
}
Tier__Ipc__Response *tier_result(struct tier_call *p, void **data, size_t *length)
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
		*data = p->data;
	if (length)
		*length = p->data_length;
	return p->response;
}
Tier__Ipc__Response *tier_rpc(TALLOC_CTX *ctx, struct tier_client *c,
			      const Tier__Ipc__Request *request)
{
	struct tier_call *p = tier_submit(ctx, c, request, NULL, 0, NULL);
	if (!p || tier_wait(p))
		return NULL;
	return tier_result(p, NULL, NULL);
}
struct tier_client *tier_client_connect(TALLOC_CTX *ctx, struct tevent_context *ev,
					const char *path)
{
	struct sockaddr_un addr = {.sun_family = AF_UNIX};
	struct tier_client *c;
	Tier__Ipc__Request request = TIER__IPC__REQUEST__INIT;
	Tier__Ipc__Response *response;
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
	c->fde = tevent_add_fd(ev, c, c->fd, TEVENT_FD_READ, socket_event, c);
	if (!c->fde) {
		errno = ENOMEM;
		goto error;
	}
	tmp = talloc_new(c);
	request.method = TIER__IPC__METHOD__HELLO;
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
