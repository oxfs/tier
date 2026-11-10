#ifndef TIER_CLIENT_H
#define TIER_CLIENT_H
#include "tier.pb-c.h"

struct tier_client;
struct tier_call;
struct tier_client *tier_client_connect(TALLOC_CTX *, struct tevent_context *,
					const char *);
struct tier_call *tier_submit(TALLOC_CTX *, struct tier_client *,
			      const Tier__Ipc__Request *, const void *, size_t,
			      struct tevent_req *);
int tier_wait(struct tier_call *);
Tier__Ipc__Response *tier_result(struct tier_call *, void **, size_t *);
Tier__Ipc__Response *tier_rpc(TALLOC_CTX *, struct tier_client *,
			      const Tier__Ipc__Request *);
uint32_t tier_max_data(struct tier_client *);
#endif
