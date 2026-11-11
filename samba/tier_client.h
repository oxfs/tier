#ifndef TIER_CLIENT_H
#define TIER_CLIENT_H
#include "tier_rust.h"

struct tier_client;
struct pthreadpool_tevent;
struct tier_call;
struct tier_client *tier_client_connect(TALLOC_CTX *, struct tevent_context *,
					const char *, struct pthreadpool_tevent *);
struct tier_call *tier_submit(TALLOC_CTX *, struct tier_client *, const TierRequest *,
			      const void *, size_t, struct tevent_req *);
int tier_wait(struct tier_call *);
TierResponse *tier_result(struct tier_call *, void **, size_t *);
TierResponse *tier_rpc(TALLOC_CTX *, struct tier_client *, const TierRequest *);
int tier_defer_close(struct tier_client *, uint64_t);
uint32_t tier_max_data(struct tier_client *);
#endif
